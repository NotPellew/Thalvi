//! Carries an accepted capture to the desktop: pastes it into the window bound at wake, runs the task, or keeps the text on the clipboard.

use std::fmt::Display;
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;

use crate::capture::CaptureRoute;
use crate::config::DesktopConfig;
use crate::engine::{CaptureOutcome, EngineState};
use crate::host::{Delivered, Host, HostError, PasteText, WindowApp};
use crate::learn::{FieldLimit, FieldProbe};
use crate::metrics::{Event, Recorder};
use crate::report::{UserError, causes};
use crate::tasks::{self, Task};

#[cfg(test)]
pub(crate) mod fakes;

/// The window a capture started in, or why none could be bound.
pub struct Bound<T>(Result<T, HostError>);

impl<T: Display> Bound<T> {
    /// The window's name for notices; `None` when nothing was bound.
    #[must_use]
    pub fn window(&self) -> Option<String> {
        self.0.as_ref().ok().map(ToString::to_string)
    }
}

/// The confirmation shown while a submit ending is pending, naming the window that receives it.
#[must_use]
pub fn submit_notice(state: &EngineState, window: &str) -> Option<String> {
    match state {
        EngineState::PendingEnding {
            route: CaptureRoute::Submit,
            text,
            ..
        } => Some(format!(
            "Submit to {window}? This presses Enter. Stay quiet to confirm, or speak to continue.\n{text}"
        )),
        _ => None,
    }
}

/// Why unconfirmed lines were kept on the clipboard instead of pasted into a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "Thalvi did not paste these lines into the terminal, because a pasted line break can run a command."
)]
pub struct TerminalLines;

impl UserError for TerminalLines {
    fn headline(&self) -> String {
        "Copied — paste the lines yourself".to_owned()
    }
}

/// What delivering a capture came to.
pub struct Finished {
    /// The text reached its destination; otherwise it is kept on the clipboard when possible.
    pub delivered: bool,
    /// It is not known whether the paste reached the window, so the text may be in it; never with `delivered`.
    pub maybe_partial: bool,
    /// Says what happened and where the dictated text went.
    pub notice: String,
    /// `notice` as one short line for the overlay.
    pub headline: String,
    /// The technical causes behind a failure, outermost first.
    pub causes: Vec<String>,
    /// The selected task's destination (the address opened or the app focused), when delivery
    /// chose a task; `None` for other routes and when no task could be chosen. Never the
    /// window bound at wake.
    pub task_target: Option<String>,
    /// The probe armed before a pasted capture, or why none could be armed. Only on the paste
    /// route with learning on and a paste that reached a window. Resolving it belongs to the
    /// watch, never to delivery.
    pub field: Option<Result<Box<dyn FieldProbe>, FieldLimit>>,
}

impl Finished {
    fn delivered(notice: String) -> Self {
        Self {
            delivered: true,
            maybe_partial: false,
            notice,
            headline: "Delivered".to_owned(),
            causes: Vec::new(),
            task_target: None,
            field: None,
        }
    }

    fn kept(notice: String, headline: String) -> Self {
        Self {
            delivered: false,
            maybe_partial: false,
            notice,
            headline,
            causes: Vec::new(),
            task_target: None,
            field: None,
        }
    }
}

/// Delivers accepted captures through one desktop host.
pub struct Delivery<H> {
    host: Arc<H>,
}

impl<H> Delivery<H>
where
    H: Host + 'static,
{
    #[must_use]
    pub fn new(host: H) -> Self {
        Self {
            host: Arc::new(host),
        }
    }

    #[cfg(test)]
    pub(crate) fn host(&self) -> &H {
        &self.host
    }

    /// Whether the desktop can only put dictation on the clipboard, never paste it.
    #[must_use]
    pub fn copies_only(&self) -> bool {
        self.host.copies_only()
    }

    /// Binds the window that has focus now; call at the wake, before the capture starts.
    pub async fn bind(&self, recorder: &Recorder) -> Bound<H::Target> {
        Bound(
            self.call(recorder, "capture_target", H::capture_target)
                .await,
        )
    }

    /// The bound app and its title, when this desktop can name them.
    #[must_use]
    pub fn writing_target(&self, bound: &Bound<H::Target>) -> Option<WindowApp> {
        self.host.writing_target(bound.0.as_ref().ok()?).ok()
    }

    /// Acts on how a capture ended and says what came of it; `None` when the user cancelled and
    /// there is nothing to report.
    ///
    /// The host leaves pasted text on the clipboard, so it can be pasted elsewhere if the window
    /// was the wrong one. Text that cannot be delivered is put on the clipboard, and the notice
    /// says whether anything may have been pasted.
    pub async fn finish(
        &self,
        outcome: &CaptureOutcome,
        bound: &Bound<H::Target>,
        config: &DesktopConfig,
        recorder: &Recorder,
    ) -> Option<Finished> {
        match outcome {
            CaptureOutcome::Cancelled { .. } => None,
            CaptureOutcome::Failed {
                error,
                recoverable_text,
            } => Some(
                self.keep(
                    recorder,
                    recoverable_text.as_deref().unwrap_or_default(),
                    error,
                )
                .await,
            ),
            CaptureOutcome::Accepted(request) => Some(match request.route {
                // The engine writes a Write request out and hands on a Paste; one that arrives
                // unwritten is pasted as recognized, never invented.
                CaptureRoute::Paste => {
                    self.paste_in(
                        recorder,
                        bound,
                        &request.text,
                        false,
                        config.stt.learn_from_corrections,
                    )
                    .await
                }
                CaptureRoute::Write => {
                    self.paste_in(recorder, bound, &request.text, false, false)
                        .await
                }
                CaptureRoute::Submit => {
                    self.paste_in(recorder, bound, &request.text, true, false)
                        .await
                }
                CaptureRoute::Task => self.run_task(recorder, config, &request.text).await,
            }),
        }
    }

    async fn paste_in(
        &self,
        recorder: &Recorder,
        bound: &Bound<H::Target>,
        text: &str,
        enter: bool,
        learn: bool,
    ) -> Finished {
        if text.is_empty() && !enter {
            return Finished::kept(
                "There was nothing to paste.".to_owned(),
                "Nothing to paste".to_owned(),
            );
        }
        let mut armed = None;
        let result = match (&bound.0, PasteText::new(text)) {
            (Ok(target), Ok(pasted)) => {
                if !enter && pasted.as_str().contains('\n') && self.host.is_terminal(target) {
                    return self.keep(recorder, text, &TerminalLines).await;
                }
                let target = target.clone();
                if learn {
                    armed = Some(self.host.watch_field(&target));
                }
                self.call(recorder, "deliver", move |host| {
                    host.deliver(&target, &pasted, enter)
                })
                .await
            }
            (_, Err(error)) => Err(error),
            (Err(error), Ok(_)) => Err(HostError::NothingTyped(format!(
                "{error} Nothing was pasted."
            ))),
        };
        match result {
            Ok(delivered) => {
                let notice = match (delivered, enter) {
                    (Delivered::InWindow, _) => "Delivered.",
                    (Delivered::OnClipboard, true) => "Copied — press Ctrl+V then Enter",
                    (Delivered::OnClipboard, false) => "Copied — press Ctrl+V to paste",
                };
                let field = match (armed, delivered) {
                    (Some(Err(limit)), _) => Some(Err(limit)),
                    (Some(Ok(probe)), Delivered::InWindow) => Some(Ok(probe)),
                    _ => None,
                };
                Finished {
                    field,
                    ..Finished::delivered(notice.to_owned())
                }
            }
            Err(error) => Finished {
                maybe_partial: matches!(error, HostError::MaybePartial(_)),
                ..self.keep(recorder, text, &error).await
            },
        }
    }

    /// Puts `text` on the clipboard.
    ///
    /// # Errors
    /// Why the clipboard could not be set.
    pub async fn copy_text(&self, recorder: &Recorder, text: &str) -> Result<(), HostError> {
        let owned = text.to_owned();
        self.call(recorder, "copy", move |host| host.copy(&owned))
            .await
    }

    async fn run_task(&self, recorder: &Recorder, config: &DesktopConfig, text: &str) -> Finished {
        let task = match tasks::decide(text, &config.scripts) {
            Ok(task) => task,
            Err(error) => {
                return self.keep(recorder, text, &error).await;
            }
        };
        let target = match &task {
            Task::OpenUrl { url, .. } => url.as_str().to_owned(),
            Task::FocusApp { label, .. } => label.clone(),
        };
        let (call, keep) = match &task {
            Task::OpenUrl { .. } => ("open_url", text.to_owned()),
            Task::FocusApp { .. } => ("focus_app", text.to_owned()),
        };
        match self
            .call(recorder, call, move |host| tasks::execute(&task, host))
            .await
        {
            Ok(notice) => Finished {
                task_target: Some(target),
                ..Finished::delivered(notice)
            },
            Err(error) => Finished {
                task_target: Some(target),
                ..self.keep(recorder, &keep, &error).await
            },
        }
    }

    /// Puts `text` on the clipboard after `error`, and says so; if the clipboard fails too, the
    /// notice carries the text and the headline says to look for it.
    async fn keep(
        &self,
        recorder: &Recorder,
        text: &str,
        error: &(impl UserError + 'static),
    ) -> Finished {
        let what_happened = error.to_string();
        let (notice, headline) = if text.is_empty() {
            (what_happened, error.headline())
        } else {
            let owned = text.to_owned();
            match self
                .call(recorder, "copy", move |host| host.copy(&owned))
                .await
            {
                Ok(()) => (
                    format!("{what_happened} Captured text is on the clipboard."),
                    error.headline(),
                ),
                Err(copy_error) => (
                    format!(
                        "{what_happened} Clipboard recovery failed: {copy_error} Your text follows:\n{text}"
                    ),
                    "Clipboard failed — text in Settings".to_owned(),
                ),
            }
        };
        Finished {
            causes: causes(error),
            ..Finished::kept(notice, headline)
        }
    }

    /// Runs one host call off the async runtime and records its latency and outcome.
    async fn call<T: Send + 'static>(
        &self,
        recorder: &Recorder,
        name: &'static str,
        call: impl FnOnce(&H) -> Result<T, HostError> + Send + 'static,
    ) -> Result<T, HostError> {
        let host = Arc::clone(&self.host);
        let started = Instant::now();
        let result = tokio::task::spawn_blocking(move || call(&host))
            .await
            .unwrap_or_else(|_| {
                Err(HostError::MaybePartial(
                    "The desktop action stopped unexpectedly. Check the target before retrying."
                        .to_owned(),
                ))
            });
        recorder.metric(
            Event::HostCall,
            match &result {
                Ok(_) => "ok",
                Err(HostError::Unsupported(_)) => "unsupported",
                Err(HostError::NothingTyped(_)) => "nothing_typed",
                Err(HostError::MaybePartial(_)) => "maybe_partial",
            },
            Some(started.elapsed()),
            json!({"call": name}),
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{FakeFieldSource, FakeHost, FakeProbe, FakeWindow};
    use super::*;
    use crate::audio::AudioError;
    use crate::capture::CaptureRequest;
    use crate::engine::CaptureError;
    use crate::learn::{Field, FieldEnd, FieldProbe, watch as learn_watch};
    use crate::metrics::RecordKinds;
    use crate::recognizer::{ConnectFailure, Detail, RecognizerError, SpeechSource};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    /// A probe whose field resolution waits for the test to release it.
    struct BlockingProbe {
        entered: mpsc::Sender<()>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
        source: FakeFieldSource,
    }

    impl FieldProbe for BlockingProbe {
        fn field(self: Box<Self>, _pasted: &str) -> Result<Box<dyn Field>, FieldLimit> {
            let _ = self.entered.send(());
            let _ = self
                .release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv();
            Ok(self.source.field())
        }
    }

    const CONFIG: &str = r#"{
        "apps": {"editor": {"classes": ["Code"]}},
        "scripts": [
            {"phrase": "open the project page", "action": "open_url",
             "destination": "https://github.com/NotPellew/Thalvi", "label": "the project page"},
            {"phrase": "switch to editor", "action": "focus_app", "destination": "editor",
             "label": "the editor"}
        ]
    }"#;

    fn accepted(route: CaptureRoute, text: &str) -> CaptureOutcome {
        CaptureOutcome::Accepted(CaptureRequest {
            route,
            text: text.to_owned(),
        })
    }

    async fn finish(host: FakeHost, outcome: CaptureOutcome) -> (Option<Finished>, Vec<String>) {
        let delivery = Delivery::new(host);
        let recorder = Recorder::disabled();
        let config = DesktopConfig::from_json_str(CONFIG).unwrap();
        let bound = delivery.bind(&recorder).await;
        let finished = delivery.finish(&outcome, &bound, &config, &recorder).await;
        (finished, delivery.host.calls())
    }

    async fn run(host: FakeHost, outcome: CaptureOutcome) -> (Option<String>, Vec<String>) {
        let (finished, calls) = finish(host, outcome).await;
        (finished.map(|finished| finished.notice), calls)
    }

    #[tokio::test]
    async fn copying_text_puts_it_on_the_clipboard_and_says_why_it_could_not() {
        let locked = HostError::Unsupported("The clipboard is locked.".to_owned());
        for (host, expected) in [
            (FakeHost::default(), Ok(())),
            (
                FakeHost::default().failing("copy", locked.clone()),
                Err(locked),
            ),
        ] {
            let delivery = Delivery::new(host);
            let result = delivery.copy_text(&Recorder::disabled(), "fixed").await;
            assert_eq!(result, expected);
            assert_eq!(delivery.host.calls(), ["copy:\"fixed\""]);
        }
    }

    #[tokio::test]
    async fn only_text_that_reached_its_destination_counts_as_delivered_and_each_outcome_has_its_headline()
     {
        let deliver_fails = |message: &str| {
            FakeHost::default().failing("deliver", HostError::NothingTyped(message.to_owned()))
        };
        let no_clipboard = || {
            FakeHost::default().failing("copy", HostError::Unsupported("No clipboard.".to_owned()))
        };
        let failed = |error| CaptureOutcome::Failed {
            error,
            recoverable_text: Some("kept".to_owned()),
        };
        for (host, outcome, expected) in [
            (
                FakeHost::default(),
                accepted(CaptureRoute::Paste, "hi"),
                Some((true, "Delivered", &[][..])),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Submit, "hi"),
                Some((true, "Delivered", &[])),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Task, "switch to editor"),
                Some((true, "Delivered", &[])),
            ),
            (
                deliver_fails("No."),
                accepted(CaptureRoute::Paste, "hi"),
                Some((false, "Nothing was pasted — Settings", &[])),
            ),
            (
                deliver_fails("Nope."),
                accepted(CaptureRoute::Paste, "text"),
                Some((false, "Nothing was pasted — Settings", &[])),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Paste, ""),
                Some((false, "Nothing to paste", &[])),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Task, "unknown phrase"),
                Some((false, "That task isn't configured yet", &[])),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Task, "do a backflip"),
                Some((false, "That task isn't configured yet", &[])),
            ),
            (
                FakeHost::default(),
                failed(CaptureError::MicStopped),
                Some((false, "Microphone stopped mid-capture", &[])),
            ),
            (
                FakeHost::default(),
                failed(CaptureError::Audio(AudioError::Busy)),
                Some((false, "Microphone is used by another app", &[])),
            ),
            (
                no_clipboard(),
                failed(CaptureError::Audio(AudioError::Busy)),
                Some((false, "Clipboard failed — text in Settings", &[])),
            ),
            (
                FakeHost::default(),
                CaptureOutcome::Failed {
                    error: CaptureError::Recognizer(RecognizerError::Unreachable {
                        speech: SpeechSource::direct("Office", "ws://127.0.0.1:8080".to_owned()),
                        reason: ConnectFailure::Refused,
                        detail: Detail("Connection refused (os error 111)".to_owned()),
                    }),
                    recoverable_text: None,
                },
                Some((
                    false,
                    "Can't reach speech server — Settings",
                    &["Connection refused (os error 111)"],
                )),
            ),
            (
                FakeHost::default(),
                CaptureOutcome::Cancelled { dictated: None },
                None,
            ),
        ] {
            let (finished, _) = finish(host, outcome.clone()).await;
            assert_eq!(
                finished.map(|finished| (finished.delivered, finished.headline, finished.causes)),
                expected.map(|(delivered, headline, causes)| (
                    delivered,
                    headline.to_owned(),
                    causes.iter().map(|cause| (*cause).to_owned()).collect()
                )),
                "{outcome:?}"
            );
        }
    }

    fn calls(calls: &[&str]) -> Vec<String> {
        calls.iter().map(|call| (*call).to_owned()).collect()
    }

    #[tokio::test]
    async fn only_a_paste_with_learning_on_arms_and_resolves_a_field_probe() {
        let source = FakeFieldSource::default();
        source.text("before cyber x2 after");
        let cases = [
            (
                "paste with the switch on",
                accepted(CaptureRoute::Paste, "cyber x2"),
                FakeHost::default().watching(FakeProbe::new(source.clone())),
                r#"{"stt":{"learn_from_corrections":true}}"#,
                true,
            ),
            (
                "paste with the switch off",
                accepted(CaptureRoute::Paste, "cyber x2"),
                FakeHost::default().watching(FakeProbe::new(source.clone())),
                "{}",
                false,
            ),
            (
                "submit",
                accepted(CaptureRoute::Submit, "cyber x2"),
                FakeHost::default().watching(FakeProbe::new(source.clone())),
                r#"{"stt":{"learn_from_corrections":true}}"#,
                false,
            ),
            (
                "a terminal hold-back",
                accepted(CaptureRoute::Paste, "line\nbreak"),
                FakeHost::default()
                    .terminal()
                    .watching(FakeProbe::new(source.clone())),
                r#"{"stt":{"learn_from_corrections":true}}"#,
                false,
            ),
        ];
        for (label, outcome, host, json, watches) in cases {
            let delivery = Delivery::new(host.clone());
            let config = DesktopConfig::from_json_str(json).unwrap();
            let bound = delivery.bind(&Recorder::disabled()).await;
            let finished = delivery
                .finish(&outcome, &bound, &config, &Recorder::disabled())
                .await
                .expect("an accepted capture is settled");
            assert_eq!(
                host.calls().contains(&"watch_field".to_owned()),
                watches,
                "{label}"
            );
            assert_eq!(finished.field.is_some(), watches, "{label}");
        }

        let limit = FakeHost::default().watching(FakeProbe::unavailable(
            crate::learn::FieldLimit::NoAccessibility,
        ));
        let delivery = Delivery::new(limit.clone());
        let config =
            DesktopConfig::from_json_str(r#"{"stt":{"learn_from_corrections":true}}"#).unwrap();
        let bound = delivery.bind(&Recorder::disabled()).await;
        let finished = delivery
            .finish(
                &accepted(CaptureRoute::Paste, "cyber x2"),
                &bound,
                &config,
                &Recorder::disabled(),
            )
            .await
            .expect("an accepted capture is settled");
        assert!(
            matches!(
                finished.field,
                Some(Err(crate::learn::FieldLimit::NoAccessibility))
            ),
            "the reason reaches the runner"
        );
    }

    #[tokio::test]
    async fn a_blocking_probe_never_holds_back_delivery_and_the_watch_resumes_after_it() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let source = FakeFieldSource::default();
        source.text("before cyber x2 after");
        source.text("before Cybrex2 after");
        source.end(FieldEnd::FocusLeft);
        let host = FakeHost::default().watching_boxed(Box::new(BlockingProbe {
            entered: entered_tx,
            release: Arc::new(Mutex::new(release_rx)),
            source,
        }));
        let delivery = Delivery::new(host);
        let config =
            DesktopConfig::from_json_str(r#"{"stt":{"learn_from_corrections":true}}"#).unwrap();
        let bound = delivery.bind(&Recorder::disabled()).await;
        let finished = tokio::time::timeout(
            Duration::from_secs(2),
            delivery.finish(
                &accepted(CaptureRoute::Paste, "cyber x2"),
                &bound,
                &config,
                &Recorder::disabled(),
            ),
        )
        .await
        .expect("delivery did not wait for the field")
        .expect("an accepted capture is settled");
        assert!(finished.delivered, "the paste was admitted");
        let Some(Ok(probe)) = finished.field else {
            panic!("a probe was handed on");
        };
        assert!(
            entered_rx.try_recv().is_err(),
            "the paste path did not resolve the probe"
        );
        let task = tokio::spawn(async move {
            let field = tokio::task::spawn_blocking(move || probe.field("cyber x2"))
                .await
                .expect("the probe task");
            let (_enabled, enabled_rx) = tokio::sync::watch::channel(true);
            let (_cancel, cancel_rx) = tokio::sync::oneshot::channel();
            learn_watch(
                field,
                "cyber x2",
                &[],
                enabled_rx,
                cancel_rx,
                &Recorder::disabled().metrics(),
            )
            .await
        });
        let entered = tokio::task::spawn_blocking(move || entered_rx.recv())
            .await
            .expect("the entry task");
        assert!(entered.is_ok(), "the watch resolved the probe");
        release_tx.send(()).expect("the watch is still waiting");
        let learned = task.await.expect("the watch task");
        assert_eq!(learned.corrected.as_deref(), Some("Cybrex2"));
    }

    #[tokio::test]
    async fn paste_and_submit_hand_the_host_the_text_with_line_breaks_kept_and_normalized() {
        for (route, text, expected) in [
            (
                CaptureRoute::Paste,
                "café 👋 --help $(x)",
                "deliver:the editor:\"café 👋 --help $(x)\":enter=false",
            ),
            (
                CaptureRoute::Write,
                "Hi Anna,\r\n\r\n\tSorry\rBob",
                "deliver:the editor:\"Hi Anna,\\n\\n\\tSorry\\nBob\":enter=false",
            ),
            (
                CaptureRoute::Submit,
                "run it",
                "deliver:the editor:\"run it\":enter=true",
            ),
            (
                CaptureRoute::Submit,
                "",
                "deliver:the editor:\"\":enter=true",
            ),
        ] {
            let (finished, seen) = finish(FakeHost::default(), accepted(route, text)).await;
            let finished = finished.unwrap();
            assert!(finished.delivered, "{route:?} {text}");
            assert_eq!(finished.notice, "Delivered.", "{route:?} {text}");
            assert_eq!(
                seen,
                calls(&["capture_target", expected]),
                "{route:?} {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_terminal_gets_only_single_lines_and_confirmed_submits() {
        let held_back = "Thalvi did not paste these lines into the terminal, because a pasted line break can run a command. Captured text is on the clipboard.";
        for (route, text, delivered, host_call, notice) in [
            (
                CaptureRoute::Paste,
                "first\nsecond",
                false,
                "copy:\"first\\nsecond\"",
                held_back,
            ),
            (
                CaptureRoute::Write,
                "first\r\nsecond",
                false,
                "copy:\"first\\r\\nsecond\"",
                held_back,
            ),
            (
                CaptureRoute::Paste,
                "ls\t-la",
                true,
                "deliver:the editor:\"ls\\t-la\":enter=false",
                "Delivered.",
            ),
            (
                CaptureRoute::Submit,
                "first\nsecond",
                true,
                "deliver:the editor:\"first\\nsecond\":enter=true",
                "Delivered.",
            ),
        ] {
            let (finished, seen) =
                finish(FakeHost::default().terminal(), accepted(route, text)).await;
            let finished = finished.unwrap();
            assert_eq!(
                seen,
                calls(&["capture_target", host_call]),
                "{route:?} {text:?}"
            );
            assert_eq!(finished.delivered, delivered, "{route:?} {text:?}");
            assert_eq!(finished.notice, notice, "{route:?} {text:?}");
        }
    }

    #[tokio::test]
    async fn a_desktop_that_only_copies_says_so_and_does_not_copy_twice() {
        for (route, enter, notice) in [
            (CaptureRoute::Paste, false, "Copied — press Ctrl+V to paste"),
            (
                CaptureRoute::Submit,
                true,
                "Copied — press Ctrl+V then Enter",
            ),
        ] {
            let (finished, seen) = finish(
                FakeHost::default().clipboard_only(),
                accepted(route, "secret words"),
            )
            .await;
            let finished = finished.unwrap();
            assert!(finished.delivered, "{route:?}");
            assert_eq!(finished.notice, notice, "{route:?}");
            assert_eq!(
                seen,
                calls(&[
                    "capture_target",
                    &format!("deliver:the editor:\"secret words\":enter={enter}"),
                ]),
                "{route:?}: the host's delivery is the one copy"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_paste_pastes_and_copies_nothing() {
        let (notice, seen) = run(FakeHost::default(), accepted(CaptureRoute::Paste, "")).await;
        assert_eq!(notice.as_deref(), Some("There was nothing to paste."));
        assert_eq!(seen, calls(&["capture_target"]));
    }

    #[tokio::test]
    async fn every_failure_says_what_happened_and_keeps_the_text_on_the_clipboard() {
        let deliver = |route: CaptureRoute, text: &str| {
            format!(
                "deliver:the editor:{text:?}:enter={}",
                route == CaptureRoute::Submit
            )
        };
        for (host, outcome, expected, seen) in [
            (
                FakeHost::default().failing(
                    "deliver",
                    HostError::Unsupported(
                        "Pasting needs wtype, which is not installed.".to_owned(),
                    ),
                ),
                accepted(CaptureRoute::Submit, "keep me"),
                "Pasting needs wtype, which is not installed. Captured text is on the clipboard.",
                calls(&[
                    "capture_target",
                    &deliver(CaptureRoute::Submit, "keep me"),
                    "copy:\"keep me\"",
                ]),
            ),
            (
                FakeHost::default().failing(
                    "deliver",
                    HostError::NothingTyped("The window closed. Nothing was pasted.".to_owned()),
                ),
                accepted(CaptureRoute::Submit, "keep me"),
                "The window closed. Nothing was pasted. Captured text is on the clipboard.",
                calls(&[
                    "capture_target",
                    &deliver(CaptureRoute::Submit, "keep me"),
                    "copy:\"keep me\"",
                ]),
            ),
            (
                FakeHost::default().failing(
                    "deliver",
                    HostError::MaybePartial(
                        "Thalvi may or may not have pasted the text.".to_owned(),
                    ),
                ),
                accepted(CaptureRoute::Submit, "keep me"),
                "Thalvi may or may not have pasted the text. Captured text is on the clipboard.",
                calls(&[
                    "capture_target",
                    &deliver(CaptureRoute::Submit, "keep me"),
                    "copy:\"keep me\"",
                ]),
            ),
            (
                FakeHost::default().failing(
                    "capture_target",
                    HostError::NothingTyped(
                        "The desktop did not identify the focused window.".to_owned(),
                    ),
                ),
                accepted(CaptureRoute::Paste, "keep me"),
                "The desktop did not identify the focused window. Nothing was pasted. Captured text is on the clipboard.",
                calls(&["capture_target", "copy:\"keep me\""]),
            ),
            (
                FakeHost::default().failing("deliver", HostError::NothingTyped("Gone.".to_owned())),
                accepted(CaptureRoute::Submit, ""),
                "Gone.",
                calls(&["capture_target", &deliver(CaptureRoute::Submit, "")]),
            ),
            (
                FakeHost::default().panicking("deliver"),
                accepted(CaptureRoute::Paste, "keep me"),
                "The desktop action stopped unexpectedly. Check the target before retrying. Captured text is on the clipboard.",
                calls(&[
                    "capture_target",
                    &deliver(CaptureRoute::Paste, "keep me"),
                    "copy:\"keep me\"",
                ]),
            ),
        ] {
            let (notice, seen_calls) = run(host, outcome.clone()).await;
            assert_eq!(notice.as_deref(), Some(expected), "{outcome:?}: {expected}");
            assert_eq!(seen_calls, seen, "{outcome:?}: {expected}");
        }
    }

    #[tokio::test]
    async fn text_with_control_characters_is_never_sent_to_the_host() {
        for text in ["esc\u{1b}[A", "bell\u{7}", "next\u{85}line"] {
            let (notice, seen) =
                run(FakeHost::default(), accepted(CaptureRoute::Paste, text)).await;
            assert!(
                notice.as_deref().is_some_and(|notice| notice.starts_with(
                    "The text contains control characters that could act in the window. Nothing was pasted."
                ) && notice
                    .ends_with("Captured text is on the clipboard.")),
                "{text:?}: {notice:?}"
            );
            assert_eq!(
                seen,
                calls(&["capture_target", &format!("copy:{text:?}")]),
                "{text:?}"
            );
        }
    }

    #[tokio::test]
    async fn when_the_clipboard_fails_too_the_notice_carries_the_text() {
        let (notice, _) = run(
            FakeHost::default()
                .failing("deliver", HostError::NothingTyped("Nope.".to_owned()))
                .failing("copy", HostError::Unsupported("No clipboard.".to_owned())),
            accepted(CaptureRoute::Paste, "keep me"),
        )
        .await;
        assert_eq!(
            notice.as_deref(),
            Some("Nope. Clipboard recovery failed: No clipboard. Your text follows:\nkeep me")
        );
    }

    #[tokio::test]
    async fn failed_and_cancelled_captures_keep_the_text_and_deliver_nothing() {
        let failed = |recoverable_text: Option<&str>| CaptureOutcome::Failed {
            error: CaptureError::NotReady {
                waited: Duration::from_secs(8),
                speech: SpeechSource::direct("Office", "ws://127.0.0.1:8080".to_owned()),
            },
            recoverable_text: recoverable_text.map(str::to_owned),
        };
        let (notice, seen) = run(FakeHost::default(), failed(Some("so far"))).await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "The speech server \"Office\" at ws://127.0.0.1:8080 was not ready after 8 seconds. Nothing was delivered. Check that it is running, or fix its address under Settings → Recognition. Captured text is on the clipboard."
            )
        );
        assert_eq!(seen, calls(&["capture_target", "copy:\"so far\""]));

        let (notice, seen) = run(FakeHost::default(), failed(None)).await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "The speech server \"Office\" at ws://127.0.0.1:8080 was not ready after 8 seconds. Nothing was delivered. Check that it is running, or fix its address under Settings → Recognition."
            )
        );
        assert_eq!(seen, calls(&["capture_target"]));

        let (notice, seen) = run(
            FakeHost::default(),
            CaptureOutcome::Cancelled { dictated: None },
        )
        .await;
        assert_eq!(notice, None);
        assert_eq!(seen, calls(&["capture_target"]));

        let writing_failed = CaptureOutcome::Failed {
            error: CaptureError::Writing(crate::writing::WritingError::Timeout),
            recoverable_text: Some("tell Anna I'm late".to_owned()),
        };
        let (finished, typed) = finish(FakeHost::default(), writing_failed).await;
        let finished = finished.unwrap();
        assert!(!finished.delivered);
        assert_eq!(
            finished.notice,
            "The writing model took longer than 15 seconds. Nothing was pasted. Captured text is on the clipboard."
        );
        assert_eq!(typed, ["capture_target", "copy:\"tell Anna I'm late\""]);
    }

    #[tokio::test]
    async fn scripted_tasks_run_through_the_host() {
        for (spoken, notice, call) in [
            (
                "Open the project page.",
                "Asked the browser to open the project page.",
                "open_url:https://github.com/NotPellew/Thalvi",
            ),
            ("switch to editor", "Focused the editor.", "focus_app:Code"),
            (
                "open example.com",
                "Asked the browser to open example.com.",
                "open_url:https://example.com/",
            ),
        ] {
            let (seen_notice, seen) =
                run(FakeHost::default(), accepted(CaptureRoute::Task, spoken)).await;
            assert_eq!(seen_notice.as_deref(), Some(notice), "{spoken}");
            assert_eq!(seen, calls(&["capture_target", call]), "{spoken}");
        }
    }

    #[tokio::test]
    async fn task_captures_record_the_destination_they_tried_never_the_wake_window() {
        use crate::history::{Capture, Outcome};

        const TASK_CONFIG: &str = r#"{
            "apps": {"editor": {"classes": ["Code"]}},
            "scripts": [
                {"phrase": "open the project page", "action": "open_url",
                 "destination": "https://github.com/NotPellew/Thalvi", "label": "the project page"},
                {"phrase": "switch to editor", "action": "focus_app", "destination": "editor",
                 "label": "the code editor"}
            ]
        }"#;

        async fn capture_of(host: FakeHost, spoken: &str) -> Option<Capture> {
            let delivery = Delivery::new(host);
            let recorder = Recorder::disabled();
            let config = DesktopConfig::from_json_str(TASK_CONFIG).unwrap();
            let bound = delivery.bind(&recorder).await;
            assert_eq!(bound.window().as_deref(), Some("the editor"));
            let outcome = accepted(CaptureRoute::Task, spoken);
            let finished = delivery.finish(&outcome, &bound, &config, &recorder).await;
            Capture::of(&outcome, finished.as_ref(), bound.window())
        }

        for (spoken, target, outcome) in [
            (
                "open the project page",
                Some("https://github.com/NotPellew/Thalvi"),
                Outcome::Accepted,
            ),
            (
                "switch to editor",
                Some("the code editor"),
                Outcome::Accepted,
            ),
            ("do a backflip", None, Outcome::Failed),
        ] {
            let capture = capture_of(FakeHost::default(), spoken).await.unwrap();
            assert_eq!(
                (capture.target.as_deref(), capture.outcome),
                (target, outcome),
                "{spoken}"
            );
        }

        let failed = capture_of(
            FakeHost::default().failing(
                "focus_app",
                HostError::NothingTyped("No window of that app is open.".to_owned()),
            ),
            "switch to editor",
        )
        .await
        .unwrap();
        assert_eq!(
            (failed.target.as_deref(), failed.outcome),
            (Some("the code editor"), Outcome::Failed)
        );
    }

    #[tokio::test]
    async fn unknown_failed_and_impossible_tasks_keep_the_captured_text() {
        let (notice, seen) = run(
            FakeHost::default(),
            accepted(CaptureRoute::Task, "do a backflip"),
        )
        .await;
        assert_eq!(
            notice.as_deref(),
            Some("That task isn't configured yet. Captured text is on the clipboard.")
        );
        assert_eq!(seen, calls(&["capture_target", "copy:\"do a backflip\""]));

        let (notice, seen) = run(
            FakeHost::default().failing(
                "focus_app",
                HostError::NothingTyped("No window of that app is open.".to_owned()),
            ),
            accepted(CaptureRoute::Task, "switch to editor"),
        )
        .await;
        assert_eq!(
            notice.as_deref(),
            Some("No window of that app is open. Captured text is on the clipboard.")
        );
        assert_eq!(
            seen,
            calls(&[
                "capture_target",
                "focus_app:Code",
                "copy:\"switch to editor\""
            ])
        );

        let (notice, seen) = run(
            FakeHost::default().failing(
                "focus_app",
                HostError::Unsupported(
                    "Switching apps is unavailable on GNOME Wayland.".to_owned(),
                ),
            ),
            accepted(CaptureRoute::Task, "switch to editor"),
        )
        .await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "Switching apps is unavailable on GNOME Wayland. Captured text is on the clipboard."
            )
        );
        assert_eq!(
            seen.last().map(String::as_str),
            Some("copy:\"switch to editor\"")
        );
    }

    #[test]
    fn only_a_pending_submit_asks_for_confirmation_and_names_the_window() {
        let pending = |route| EngineState::PendingEnding {
            route,
            text: "run it".to_owned(),
            waiting_for_quiet: false,
            preview: None,
        };
        assert_eq!(
            submit_notice(&pending(CaptureRoute::Submit), "firefox").as_deref(),
            Some(
                "Submit to firefox? This presses Enter. Stay quiet to confirm, or speak to continue.\nrun it"
            )
        );
        for state in [
            pending(CaptureRoute::Paste),
            pending(CaptureRoute::Task),
            EngineState::Listening,
        ] {
            assert_eq!(submit_notice(&state, "firefox"), None, "{state:?}");
        }
    }

    #[test]
    fn a_bound_window_names_its_app_only_when_bound_and_supported() {
        let bound = Bound(Ok(FakeWindow("firefox")));
        assert_eq!(bound.window().as_deref(), Some("firefox"));
        let failed = Bound::<FakeWindow>(Err(HostError::Unsupported(String::new())));
        assert_eq!(failed.window(), None);
        for (host, copies_only, expected) in [
            (
                FakeHost::default(),
                false,
                Some(WindowApp::new("firefox", "")),
            ),
            (FakeHost::default().clipboard_only(), true, None),
        ] {
            let delivery = Delivery::new(host);
            assert_eq!(delivery.copies_only(), copies_only);
            assert_eq!(delivery.writing_target(&bound), expected);
            assert_eq!(delivery.writing_target(&failed), None);
        }
    }

    #[tokio::test]
    async fn every_host_call_records_its_name_and_outcome_but_no_text() {
        let root =
            std::env::temp_dir().join(format!("thalvi-delivery-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let recorder = Recorder::in_dir(
            RecordKinds {
                metrics: true,
                text: false,
                audio: false,
            },
            &root,
        )
        .unwrap();
        let delivery = Delivery::new(
            FakeHost::default().failing("deliver", HostError::MaybePartial("Stopped.".to_owned())),
        );
        let config = DesktopConfig::from_json_str(CONFIG).unwrap();
        let bound = delivery.bind(&recorder).await;
        delivery
            .finish(
                &accepted(CaptureRoute::Paste, "secret words"),
                &bound,
                &config,
                &recorder,
            )
            .await;
        recorder.finish().await;

        let written = std::fs::read_to_string(root.join("metrics.jsonl")).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let events: Vec<(String, String, bool)> = written
            .lines()
            .map(|line| {
                let line: serde_json::Value = serde_json::from_str(line).unwrap();
                (
                    line["call"].as_str().unwrap().to_owned(),
                    line["outcome"].as_str().unwrap().to_owned(),
                    line["event"] == "desktop_host_call" && line["duration_ms"].is_number(),
                )
            })
            .collect();
        assert_eq!(
            events,
            [
                ("capture_target".to_owned(), "ok".to_owned(), true),
                ("deliver".to_owned(), "maybe_partial".to_owned(), true),
                ("copy".to_owned(), "ok".to_owned(), true),
            ]
        );
        assert!(!written.contains("secret"));
    }
}

//! Carries an accepted capture to the desktop: types it into the window bound at wake, runs the task, or keeps the text on the clipboard.

use std::fmt::Display;
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;

use crate::capture::CaptureRoute;
use crate::config::DesktopConfig;
use crate::engine::{CaptureOutcome, EngineState};
use crate::host::{Delivered, Host, HostError, TypedText};
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

/// What delivering a capture came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The text reached its destination; otherwise it is kept on the clipboard when possible.
    pub delivered: bool,
    /// Typing stopped partway, so some of the text may be in the window; never with `delivered`.
    pub maybe_partial: bool,
    /// Says what happened and where the dictated text went.
    pub notice: String,
    /// `notice` as one short line for the overlay.
    pub headline: &'static str,
    /// The technical causes behind a failure, outermost first.
    pub causes: Vec<String>,
    /// The selected task's destination (the address opened or the app focused), when delivery
    /// chose a task; `None` for other routes and when no task could be chosen. Never the
    /// window bound at wake.
    pub task_target: Option<String>,
}

impl Finished {
    fn delivered(notice: String) -> Self {
        Self {
            delivered: true,
            maybe_partial: false,
            notice,
            headline: "Delivered",
            causes: Vec::new(),
            task_target: None,
        }
    }

    fn kept(notice: String, headline: &'static str) -> Self {
        Self {
            delivered: false,
            maybe_partial: false,
            notice,
            headline,
            causes: Vec::new(),
            task_target: None,
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

    /// Whether the desktop can only put dictation on the clipboard, never type it.
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

    /// Acts on how a capture ended and says what came of it; `None` when the user cancelled and
    /// there is nothing to report.
    ///
    /// Text typed into the window is also copied to the clipboard, so it can be pasted elsewhere
    /// if the window was the wrong one. Text that cannot be delivered is put on the clipboard, and
    /// the notice says whether anything may have been typed.
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
                CaptureRoute::Paste => self.type_in(recorder, bound, &request.text, false).await,
                CaptureRoute::Submit => self.type_in(recorder, bound, &request.text, true).await,
                CaptureRoute::Task => self.run_task(recorder, config, &request.text).await,
            }),
        }
    }

    async fn type_in(
        &self,
        recorder: &Recorder,
        bound: &Bound<H::Target>,
        text: &str,
        enter: bool,
    ) -> Finished {
        if text.is_empty() && !enter {
            return Finished::kept("There was nothing to type.".to_owned(), "Nothing to type");
        }
        let result = match (&bound.0, TypedText::new(text)) {
            (Ok(target), Ok(typed)) => {
                let target = target.clone();
                self.call(recorder, "deliver", move |host| {
                    host.deliver(&target, &typed, enter)
                })
                .await
            }
            (_, Err(error)) => Err(error),
            (Err(error), Ok(_)) => Err(HostError::NothingTyped(format!(
                "{error} Nothing was typed."
            ))),
        };
        match result {
            Ok(Delivered::InWindow) => {
                if !text.is_empty() {
                    // Best effort: the text already reached the window, and a failed call is recorded.
                    let owned = text.to_owned();
                    let _ = self
                        .call(recorder, "copy", move |host| host.copy(&owned))
                        .await;
                }
                Finished::delivered("Delivered.".to_owned())
            }
            Ok(Delivered::OnClipboard) => Finished::delivered(
                if enter {
                    "Copied — press Ctrl+V then Enter"
                } else {
                    "Copied — press Ctrl+V to paste"
                }
                .to_owned(),
            ),
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
            Task::DiscordMessage { destination, .. } => destination.clone(),
        };
        let (call, keep) = match &task {
            Task::OpenUrl { .. } => ("open_url", text.to_owned()),
            Task::FocusApp { .. } => ("focus_app", text.to_owned()),
            Task::DiscordMessage { message, .. } => ("discord_message", message.clone()),
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
                    "Clipboard failed — text in Settings",
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
    use super::fakes::{FakeHost, FakeWindow};
    use super::*;
    use crate::audio::AudioError;
    use crate::capture::CaptureRequest;
    use crate::engine::CaptureError;
    use crate::metrics::RecordKinds;
    use crate::recognizer::{ConnectFailure, Detail, RecognizerError, ServerUrl, SpeechSource};
    use std::time::Duration;

    const CONFIG: &str = r#"{
        "apps": {"editor": {"classes": ["Code"]}},
        "scripts": [
            {"phrase": "open the project page", "action": "open_url",
             "destination": "https://github.com/NotPellew/Thalvi", "label": "the project page"},
            {"phrase": "switch to editor", "action": "focus_app", "destination": "editor",
             "label": "the editor"},
            {"phrase": "message the demo channel saying", "action": "discord_message",
             "destination": "https://discord.com/channels/1/2", "label": "Demo"}
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
    async fn only_text_that_reached_its_destination_counts_as_delivered() {
        let deliver_fails =
            || FakeHost::default().failing("deliver", HostError::NothingTyped("No.".to_owned()));
        let failed = CaptureOutcome::Failed {
            error: CaptureError::MicStopped,
            recoverable_text: Some("kept".to_owned()),
        };
        for (host, outcome, expected) in [
            (
                FakeHost::default(),
                accepted(CaptureRoute::Paste, "hi"),
                Some(true),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Submit, "hi"),
                Some(true),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Task, "switch to editor"),
                Some(true),
            ),
            (
                deliver_fails(),
                accepted(CaptureRoute::Paste, "hi"),
                Some(false),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Paste, ""),
                Some(false),
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Task, "unknown phrase"),
                Some(false),
            ),
            (FakeHost::default(), failed, Some(false)),
            (
                FakeHost::default(),
                CaptureOutcome::Cancelled { dictated: None },
                None,
            ),
        ] {
            let (finished, _) = finish(host, outcome.clone()).await;
            assert_eq!(
                finished.map(|finished| finished.delivered),
                expected,
                "{outcome:?}"
            );
        }
    }

    fn calls(calls: &[&str]) -> Vec<String> {
        calls.iter().map(|call| (*call).to_owned()).collect()
    }

    #[tokio::test]
    async fn paste_and_submit_type_the_exact_text_and_copy_it_unless_there_is_none() {
        let copy_fails =
            || FakeHost::default().failing("copy", HostError::Unsupported("No.".to_owned()));
        for (host, route, text, expected) in [
            (
                FakeHost::default(),
                CaptureRoute::Paste,
                "café 👋 --help $(x)",
                &[
                    "deliver:the editor:\"café 👋 --help $(x)\":enter=false",
                    "copy:\"café 👋 --help $(x)\"",
                ][..],
            ),
            (
                FakeHost::default(),
                CaptureRoute::Submit,
                "run it",
                &[
                    "deliver:the editor:\"run it\":enter=true",
                    "copy:\"run it\"",
                ],
            ),
            (
                FakeHost::default(),
                CaptureRoute::Submit,
                "",
                &["deliver:the editor:\"\":enter=true"],
            ),
            (
                copy_fails(),
                CaptureRoute::Paste,
                "hi",
                &["deliver:the editor:\"hi\":enter=false", "copy:\"hi\""],
            ),
        ] {
            let (finished, seen) = finish(host, accepted(route, text)).await;
            let finished = finished.unwrap();
            assert!(finished.delivered, "{route:?} {text}");
            assert_eq!(finished.notice, "Delivered.", "{route:?} {text}");
            let mut want = vec!["capture_target"];
            want.extend_from_slice(expected);
            assert_eq!(seen, calls(&want), "{route:?} {text}");
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
    async fn an_app_switch_a_desktop_cannot_make_leaves_the_text_on_the_clipboard() {
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

    #[tokio::test]
    async fn an_empty_paste_types_and_copies_nothing() {
        let (notice, seen) = run(FakeHost::default(), accepted(CaptureRoute::Paste, "")).await;
        assert_eq!(notice.as_deref(), Some("There was nothing to type."));
        assert_eq!(seen, calls(&["capture_target"]));
    }

    #[tokio::test]
    async fn every_typed_failure_keeps_the_text_on_the_clipboard_and_says_what_happened() {
        for (error, expected) in [
            (
                HostError::Unsupported("Typing needs wtype, which is not installed.".to_owned()),
                "Typing needs wtype, which is not installed. Captured text is on the clipboard.",
            ),
            (
                HostError::NothingTyped("The window closed. Nothing was typed.".to_owned()),
                "The window closed. Nothing was typed. Captured text is on the clipboard.",
            ),
            (
                HostError::MaybePartial("Typing stopped partway.".to_owned()),
                "Typing stopped partway. Captured text is on the clipboard.",
            ),
        ] {
            let (notice, seen) = run(
                FakeHost::default().failing("deliver", error.clone()),
                accepted(CaptureRoute::Submit, "keep me"),
            )
            .await;
            assert_eq!(notice.as_deref(), Some(expected), "{error:?}");
            assert_eq!(
                seen,
                calls(&[
                    "capture_target",
                    "deliver:the editor:\"keep me\":enter=true",
                    "copy:\"keep me\""
                ]),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn text_with_control_characters_is_never_sent_to_the_host() {
        for text in ["two\nlines", "tab\there", "esc\u{1b}[A"] {
            let (notice, seen) =
                run(FakeHost::default(), accepted(CaptureRoute::Paste, text)).await;
            assert!(
                notice.as_deref().is_some_and(|notice| notice.starts_with(
                    "The text contains control characters that could press keys. Nothing was typed."
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
    async fn without_a_bound_window_nothing_is_typed() {
        let (notice, seen) = run(
            FakeHost::default().failing(
                "capture_target",
                HostError::NothingTyped(
                    "The desktop did not identify the focused window.".to_owned(),
                ),
            ),
            accepted(CaptureRoute::Paste, "keep me"),
        )
        .await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "The desktop did not identify the focused window. Nothing was typed. Captured text is on the clipboard."
            )
        );
        assert_eq!(seen, calls(&["capture_target", "copy:\"keep me\""]));
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
    async fn an_empty_submit_that_fails_has_no_text_to_keep() {
        let (notice, seen) = run(
            FakeHost::default().failing("deliver", HostError::NothingTyped("Gone.".to_owned())),
            accepted(CaptureRoute::Submit, ""),
        )
        .await;
        assert_eq!(notice.as_deref(), Some("Gone."));
        assert_eq!(
            seen,
            calls(&["capture_target", "deliver:the editor:\"\":enter=true"])
        );
    }

    #[tokio::test]
    async fn a_host_that_panics_is_reported_as_possibly_partial_and_the_text_is_kept() {
        let (notice, seen) = run(
            FakeHost::default().panicking("deliver"),
            accepted(CaptureRoute::Paste, "keep me"),
        )
        .await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "The desktop action stopped unexpectedly. Check the target before retrying. Captured text is on the clipboard."
            )
        );
        assert_eq!(seen.last().map(String::as_str), Some("copy:\"keep me\""));
    }

    #[tokio::test]
    async fn each_failure_has_a_headline_for_the_overlay_and_a_lost_clipboard_says_where_the_text_is()
     {
        let failed = |error| CaptureOutcome::Failed {
            error,
            recoverable_text: Some("kept".to_owned()),
        };
        let no_clipboard = || {
            FakeHost::default().failing("copy", HostError::Unsupported("No clipboard.".to_owned()))
        };
        let cases = [
            (
                FakeHost::default(),
                failed(CaptureError::Audio(AudioError::Busy)),
                "Microphone is used by another app",
            ),
            (
                no_clipboard(),
                failed(CaptureError::Audio(AudioError::Busy)),
                "Clipboard failed — text in Settings",
            ),
            (
                FakeHost::default().failing("deliver", HostError::NothingTyped("Nope.".to_owned())),
                accepted(CaptureRoute::Paste, "text"),
                "Nothing was typed — Settings",
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Task, "do a backflip"),
                "That task isn't configured yet",
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Paste, ""),
                "Nothing to type",
            ),
            (
                FakeHost::default(),
                accepted(CaptureRoute::Paste, "hi"),
                "Delivered",
            ),
        ];
        for (host, outcome, headline) in cases {
            let (finished, _) = finish(host, outcome).await;
            assert_eq!(finished.map(|finished| finished.headline), Some(headline));
        }
    }

    #[tokio::test]
    async fn a_failed_capture_hands_on_the_technical_causes() {
        let (finished, _) = finish(
            FakeHost::default(),
            CaptureOutcome::Failed {
                error: CaptureError::Recognizer(RecognizerError::Unreachable {
                    speech: SpeechSource::thalvi_server("Office", &ServerUrl::default()),
                    reason: ConnectFailure::Refused,
                    detail: Detail("Connection refused (os error 111)".to_owned()),
                }),
                recoverable_text: None,
            },
        )
        .await;
        assert_eq!(
            finished.map(|finished| finished.causes),
            Some(vec!["Connection refused (os error 111)".to_owned()])
        );
    }

    #[tokio::test]
    async fn a_failed_capture_keeps_what_was_dictated_and_a_cancelled_one_does_nothing() {
        let failed = |recoverable_text: Option<&str>| CaptureOutcome::Failed {
            error: CaptureError::NotReady {
                waited: Duration::from_secs(8),
                speech: SpeechSource::thalvi_server("Office", &ServerUrl::default()),
            },
            recoverable_text: recoverable_text.map(str::to_owned),
        };
        let (notice, seen) = run(FakeHost::default(), failed(Some("so far"))).await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "The Thalvi server \"Office\" at ws://127.0.0.1:8080 was not ready after 8 seconds. Nothing was delivered. Start the Thalvi server, or fix its address under Settings → Speech. Captured text is on the clipboard."
            )
        );
        assert_eq!(seen, calls(&["capture_target", "copy:\"so far\""]));

        let (notice, seen) = run(FakeHost::default(), failed(None)).await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "The Thalvi server \"Office\" at ws://127.0.0.1:8080 was not ready after 8 seconds. Nothing was delivered. Start the Thalvi server, or fix its address under Settings → Speech."
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
    async fn an_unknown_task_and_a_failed_task_keep_the_captured_text() {
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
    }

    #[tokio::test]
    async fn a_discord_message_is_refused_before_the_host_acts_and_only_the_message_is_kept() {
        let (notice, seen) = run(
            FakeHost::default(),
            accepted(
                CaptureRoute::Task,
                "Message the demo channel saying hello there",
            ),
        )
        .await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "Discord messages are switched off for now. Nothing was typed or sent. Captured text is on the clipboard."
            )
        );
        assert_eq!(seen, calls(&["capture_target", "copy:\"hello there\""]));
    }

    #[test]
    fn only_a_pending_submit_asks_for_confirmation_and_names_the_window() {
        let pending = |route| EngineState::PendingEnding {
            route,
            text: "run it".to_owned(),
            waiting_for_quiet: false,
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
    fn a_bound_window_is_named_by_its_display() {
        assert_eq!(
            Bound(Ok(FakeWindow("firefox"))).window().as_deref(),
            Some("firefox")
        );
        assert_eq!(
            Bound::<FakeWindow>(Err(HostError::Unsupported(String::new()))).window(),
            None
        );
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

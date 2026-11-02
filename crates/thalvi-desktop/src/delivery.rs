//! Carries an accepted capture to the desktop: types it into the window bound at wake, runs the task, or keeps the text on the clipboard.

use std::fmt::Display;
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;

use crate::capture::CaptureRoute;
use crate::config::DesktopConfig;
use crate::engine::{CaptureOutcome, EngineState};
use crate::host::{Host, HostError, TypedText};
use crate::metrics::{Event, Recorder};
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
    /// Says what happened and where the dictated text went.
    pub notice: String,
}

impl Finished {
    fn delivered(notice: String) -> Self {
        Self {
            delivered: true,
            notice,
        }
    }

    fn kept(notice: String) -> Self {
        Self {
            delivered: false,
            notice,
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
    /// Text that cannot be delivered is put on the clipboard, and the notice says whether anything
    /// may have been typed.
    pub async fn finish(
        &self,
        outcome: &CaptureOutcome,
        bound: &Bound<H::Target>,
        config: &DesktopConfig,
        recorder: &Recorder,
    ) -> Option<Finished> {
        match outcome {
            CaptureOutcome::Cancelled => None,
            CaptureOutcome::Failed {
                error,
                recoverable_text,
            } => Some(Finished::kept(match recoverable_text {
                Some(text) => self.keep(recorder, text, &error.to_string()).await,
                None => error.to_string(),
            })),
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
            return Finished::kept("There was nothing to type.".to_owned());
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
            Ok(()) => Finished::delivered("Delivered.".to_owned()),
            Err(error) => Finished::kept(self.keep(recorder, text, &error.to_string()).await),
        }
    }

    async fn run_task(&self, recorder: &Recorder, config: &DesktopConfig, text: &str) -> Finished {
        let task = match tasks::decide(text, config) {
            Ok(task) => task,
            Err(error) => {
                return Finished::kept(self.keep(recorder, text, &error.to_string()).await);
            }
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
            Ok(notice) => Finished::delivered(notice),
            Err(error) => Finished::kept(self.keep(recorder, &keep, &error.to_string()).await),
        }
    }

    /// Puts `text` on the clipboard after `what_happened`, and says so; if the clipboard fails
    /// too, the notice carries the text.
    async fn keep(&self, recorder: &Recorder, text: &str, what_happened: &str) -> String {
        if text.is_empty() {
            return what_happened.to_owned();
        }
        let owned = text.to_owned();
        match self
            .call(recorder, "copy", move |host| host.copy(&owned))
            .await
        {
            Ok(()) => format!("{what_happened} Captured text is on the clipboard."),
            Err(error) => format!(
                "{what_happened} Clipboard recovery failed: {error} Your text follows:\n{text}"
            ),
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
    use crate::capture::CaptureRequest;
    use crate::engine::CaptureError;
    use crate::metrics::RecordKinds;

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
        let config = DesktopConfig::from_json_str(CONFIG, None).unwrap();
        let bound = delivery.bind(&recorder).await;
        let finished = delivery.finish(&outcome, &bound, &config, &recorder).await;
        (finished, delivery.host.calls())
    }

    async fn run(host: FakeHost, outcome: CaptureOutcome) -> (Option<String>, Vec<String>) {
        let (finished, calls) = finish(host, outcome).await;
        (finished.map(|finished| finished.notice), calls)
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
            (FakeHost::default(), CaptureOutcome::Cancelled, None),
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
    async fn paste_and_submit_type_the_exact_text_into_the_bound_window() {
        for (route, text, call) in [
            (
                CaptureRoute::Paste,
                "café 👋 --help $(x)",
                "deliver:the editor:\"café 👋 --help $(x)\":enter=false",
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
            let (notice, seen) = run(FakeHost::default(), accepted(route, text)).await;
            assert_eq!(notice.as_deref(), Some("Delivered."), "{route:?} {text}");
            assert_eq!(seen, calls(&["capture_target", call]), "{route:?} {text}");
        }
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
    async fn a_failed_capture_keeps_what_was_dictated_and_a_cancelled_one_does_nothing() {
        let failed = |recoverable_text: Option<&str>| CaptureOutcome::Failed {
            error: CaptureError::NotReady,
            recoverable_text: recoverable_text.map(str::to_owned),
        };
        let (notice, seen) = run(FakeHost::default(), failed(Some("so far"))).await;
        assert_eq!(
            notice.as_deref(),
            Some(
                "Speech recognition was not ready in time. Nothing was delivered. Captured text is on the clipboard."
            )
        );
        assert_eq!(seen, calls(&["capture_target", "copy:\"so far\""]));

        let (notice, seen) = run(FakeHost::default(), failed(None)).await;
        assert_eq!(
            notice.as_deref(),
            Some("Speech recognition was not ready in time. Nothing was delivered.")
        );
        assert_eq!(seen, calls(&["capture_target"]));

        let (notice, seen) = run(FakeHost::default(), CaptureOutcome::Cancelled).await;
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
        let config = DesktopConfig::from_json_str(CONFIG, None).unwrap();
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

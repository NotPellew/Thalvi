//! What the UI is told: the engine's state, the pause switch and the delivery outcome as one typed status.

use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use thalvi_desktop::capture::{CaptureRoute, EndingPauses};
use thalvi_desktop::device::{Export, Step};
use thalvi_desktop::engine::EngineState;
use thalvi_desktop::report::{UserError, causes};

pub const STARTING_NOTICE: &str = "Starting listening. The microphone is not ready yet.";
pub const WAKE_DOWNLOAD_NOTICE: &str = "Downloading the wake word model.";
pub const SPOTTER_DOWNLOAD_NOTICE: &str = "Downloading the ending phrase model.";

/// The status event's payload; its JSON form is the contract with the TypeScript UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Status {
    Paused,
    Listening,
    /// Listening with the wake word off: only the hold keys start a capture.
    KeyboardOnly,
    /// The wake word was heard and the recognizer is connecting.
    Starting {
        target: Option<String>,
    },
    Capturing {
        target: Option<String>,
        committed: String,
        partial: Option<String>,
        /// What the user should know while the capture goes on; none holds dictated words.
        notice: Option<String>,
    },
    Pending {
        target: Option<String>,
        route: CaptureRoute,
        text: String,
        waiting_for_quiet: bool,
        /// The pause configured for `route`.
        pause_ms: u64,
        /// The action is a copy to the clipboard, whatever the route.
        copies: bool,
    },
    /// The writing model is putting the dictation into proper text; `text` is what has come in so far.
    Writing {
        target: Option<String>,
        text: String,
    },
    /// The writing model is reasoning and has written no text yet.
    Thinking {
        target: Option<String>,
    },
    /// A second transcription or the delivery is running.
    Working {
        target: Option<String>,
        route: CaptureRoute,
        /// The capture can still be cancelled; delivery cannot be.
        cancellable: bool,
    },
    Done {
        target: Option<String>,
        route: CaptureRoute,
        notice: String,
    },
    Failed {
        notice: String,
        /// `notice` as one short line that fits the overlay.
        headline: &'static str,
        /// The technical causes behind `notice`, outermost first, for the error report.
        causes: Vec<String>,
        kind: FailureKind,
    },
    /// A model is being fetched or loaded. For the speech model the wake word already works.
    Loading {
        notice: String,
        /// Files are being downloaded; only then does the overlay say so.
        #[serde(skip)]
        downloading: bool,
    },
}

/// What a failure stopped, which decides what the overlay's label may claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureKind {
    /// A capture's text was not delivered to its destination.
    Delivery,
    /// Anything else: a model, the microphone, a refused key press.
    Other,
}

/// What went wrong: the notice for the user and the technical causes for the error report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub notice: String,
    pub headline: &'static str,
    pub causes: Vec<String>,
    pub kind: FailureKind,
}

impl Failure {
    /// `error`'s `Display` as the notice, its headline and its source chain as the causes.
    #[must_use]
    pub fn of(error: &(impl UserError + 'static)) -> Self {
        Self {
            notice: error.to_string(),
            headline: error.headline(),
            causes: causes(error),
            kind: FailureKind::Other,
        }
    }
}

/// The latest failure the status showed. Only a newer failure replaces it, so it outlives the
/// overlay hiding it and the status recovering.
#[derive(Clone, Debug, Default)]
pub struct LastError(Arc<Mutex<Option<Failure>>>);

impl LastError {
    /// Keeps `status` if it is a failure.
    pub fn record(&self, status: &Status) {
        if let Status::Failed {
            notice,
            headline,
            causes,
            kind,
        } = status
        {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(Failure {
                notice: notice.clone(),
                headline,
                causes: causes.clone(),
                kind: *kind,
            });
        }
    }

    #[must_use]
    pub fn get(&self) -> Option<Failure> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// What the notice says while the speech model loads: from disk, or which download it is waiting for and how far it is.
#[must_use]
pub fn load_notice(step: Step) -> String {
    match step {
        Step::Reading => "Loading the speech model.".to_owned(),
        Step::Fetching(Export::Cpu) => {
            "Loading the speech model. Downloading about 650 MB.".to_owned()
        }
        Step::Fetching(Export::Gpu) => {
            "Loading the speech model. Downloading the model for the GPU, about 1.3 GB.".to_owned()
        }
        Step::Downloading(Export::Cpu, percent) => {
            format!("Downloading the speech model, about 650 MB \u{2014} {percent}%")
        }
        Step::Downloading(Export::Gpu, percent) => {
            format!("Downloading the model for the GPU, about 1.3 GB \u{2014} {percent}%")
        }
    }
}

/// What the runner reports besides the engine's own state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Follow the engine.
    Idle,
    /// Models or the microphone are starting.
    Starting,
    /// A wake word model is downloading.
    WakeDownload,
    /// The ending phrase model is downloading.
    SpotterDownload,
    /// The microphone is closed.
    Paused,
    /// An accepted capture is being delivered.
    Delivering(CaptureRoute),
    /// The last capture was delivered, until the next one starts.
    Done { route: CaptureRoute, notice: String },
    /// The last capture or the microphone failed, until the next one starts or listening resumes.
    Failed(Failure),
}

/// Everything the status is derived from.
#[derive(Clone, Debug)]
pub struct View {
    pub mode: Mode,
    pub engine: EngineState,
    /// The window bound at the latest wake.
    pub target: Option<String>,
    /// The desktop only copies dictation to the clipboard, so ending an action is a copy.
    pub copies: bool,
    pub pauses: EndingPauses,
    /// What the speech model needs, while it loads.
    pub loading: Option<Step>,
    /// The wake word starts captures; off, listening is for the hold keys alone.
    pub wake_words: bool,
}

impl View {
    #[must_use]
    pub fn status(&self) -> Status {
        let target = self.target.clone();
        if let (Some(step), EngineState::Listening, Mode::Idle | Mode::Done { .. }) =
            (self.loading, &self.engine, &self.mode)
        {
            return Status::Loading {
                notice: load_notice(step),
                downloading: matches!(step, Step::Fetching(_) | Step::Downloading(..)),
            };
        }
        match (&self.mode, &self.engine) {
            (Mode::Starting, _) => Status::Loading {
                notice: STARTING_NOTICE.to_owned(),
                downloading: false,
            },
            (Mode::WakeDownload, _) => Status::Loading {
                notice: WAKE_DOWNLOAD_NOTICE.to_owned(),
                downloading: true,
            },
            (Mode::SpotterDownload, _) => Status::Loading {
                notice: SPOTTER_DOWNLOAD_NOTICE.to_owned(),
                downloading: true,
            },
            (Mode::Paused, _) => Status::Paused,
            (Mode::Delivering(route), _) => Status::Working {
                target,
                route: *route,
                cancellable: false,
            },
            (Mode::Done { route, notice }, EngineState::Listening) => Status::Done {
                target,
                route: *route,
                notice: notice.clone(),
            },
            (Mode::Failed(failure), EngineState::Listening) => Status::Failed {
                notice: failure.notice.clone(),
                headline: failure.headline,
                causes: failure.causes.clone(),
                kind: failure.kind,
            },
            (Mode::Idle | Mode::Done { .. } | Mode::Failed(_), engine) => match engine {
                EngineState::Listening if self.wake_words => Status::Listening,
                EngineState::Listening => Status::KeyboardOnly,
                EngineState::StartingCapture => Status::Starting { target },
                EngineState::Capturing {
                    committed,
                    partial,
                    notice,
                } => Status::Capturing {
                    target,
                    committed: committed.clone(),
                    partial: partial.clone(),
                    notice: notice.clone(),
                },
                EngineState::PendingEnding {
                    route,
                    text,
                    waiting_for_quiet,
                } => Status::Pending {
                    target,
                    route: *route,
                    text: text.clone(),
                    waiting_for_quiet: *waiting_for_quiet,
                    pause_ms: u64::try_from(self.pauses.get(*route).as_millis())
                        .unwrap_or(u64::MAX),
                    copies: self.copies,
                },
                EngineState::TranscribingSecondary { route } => Status::Working {
                    target,
                    route: *route,
                    cancellable: true,
                },
                EngineState::Writing { text } => Status::Writing {
                    target,
                    text: text.clone(),
                },
                EngineState::Thinking => Status::Thinking { target },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use thalvi_desktop::recognizer::{ConnectFailure, Detail, RecognizerError, SpeechSource};

    use super::*;

    fn view(mode: Mode, engine: EngineState) -> View {
        View {
            mode,
            engine,
            target: Some("kitty".to_owned()),
            copies: false,
            pauses: EndingPauses::default(),
            loading: None,
            wake_words: true,
        }
    }

    fn kitty() -> String {
        "kitty".to_owned()
    }

    fn pending(route: CaptureRoute, waiting_for_quiet: bool) -> EngineState {
        EngineState::PendingEnding {
            route,
            text: "hello".to_owned(),
            waiting_for_quiet,
        }
    }

    #[test]
    fn listening_is_with_wake_words_keyboard_only_or_paused_and_nothing_else_changes() {
        // (mode, engine, wake words on, status)
        let cases = [
            (Mode::Idle, EngineState::Listening, true, Status::Listening),
            (
                Mode::Idle,
                EngineState::Listening,
                false,
                Status::KeyboardOnly,
            ),
            (Mode::Paused, EngineState::Listening, true, Status::Paused),
            (Mode::Paused, EngineState::Listening, false, Status::Paused),
            (
                Mode::Idle,
                EngineState::StartingCapture,
                false,
                Status::Starting {
                    target: Some(kitty()),
                },
            ),
            (
                Mode::Done {
                    route: CaptureRoute::Paste,
                    notice: "Pasted.".to_owned(),
                },
                EngineState::Listening,
                false,
                Status::Done {
                    target: Some(kitty()),
                    route: CaptureRoute::Paste,
                    notice: "Pasted.".to_owned(),
                },
            ),
        ];
        for (mode, engine, wake_words, expected) in cases {
            let status = View {
                wake_words,
                ..view(mode, engine)
            }
            .status();
            assert_eq!(status, expected, "wake words {wake_words}");
        }
    }

    #[test]
    fn the_status_follows_the_engine() {
        let cases = [
            (EngineState::Listening, Status::Listening),
            (
                EngineState::StartingCapture,
                Status::Starting {
                    target: Some(kitty()),
                },
            ),
            (
                capturing(),
                Status::Capturing {
                    target: Some(kitty()),
                    committed: "hello".to_owned(),
                    partial: Some("wor".to_owned()),
                    notice: None,
                },
            ),
            (
                EngineState::Capturing {
                    committed: "hello".to_owned(),
                    partial: None,
                    notice: Some("Heard \u{201c}paste that\u{201d}.".to_owned()),
                },
                Status::Capturing {
                    target: Some(kitty()),
                    committed: "hello".to_owned(),
                    partial: None,
                    notice: Some("Heard \u{201c}paste that\u{201d}.".to_owned()),
                },
            ),
            (
                pending(CaptureRoute::Submit, true),
                Status::Pending {
                    target: Some(kitty()),
                    route: CaptureRoute::Submit,
                    text: "hello".to_owned(),
                    waiting_for_quiet: true,
                    pause_ms: 2500,
                    copies: false,
                },
            ),
            (
                pending(CaptureRoute::Paste, false),
                Status::Pending {
                    target: Some(kitty()),
                    route: CaptureRoute::Paste,
                    text: "hello".to_owned(),
                    waiting_for_quiet: false,
                    pause_ms: 700,
                    copies: false,
                },
            ),
            (
                pending(CaptureRoute::Write, false),
                Status::Pending {
                    target: Some(kitty()),
                    route: CaptureRoute::Write,
                    text: "hello".to_owned(),
                    waiting_for_quiet: false,
                    pause_ms: 700,
                    copies: false,
                },
            ),
            (
                EngineState::Writing {
                    text: "Hello, ".to_owned(),
                },
                Status::Writing {
                    target: Some(kitty()),
                    text: "Hello, ".to_owned(),
                },
            ),
            (
                EngineState::Thinking,
                Status::Thinking {
                    target: Some(kitty()),
                },
            ),
            (
                EngineState::TranscribingSecondary {
                    route: CaptureRoute::Task,
                },
                Status::Working {
                    target: Some(kitty()),
                    route: CaptureRoute::Task,
                    cancellable: true,
                },
            ),
        ];
        for (engine, expected) in cases {
            let label = format!("{engine:?}");
            assert_eq!(view(Mode::Idle, engine).status(), expected, "{label}");
        }
    }

    #[test]
    fn the_runner_overrides_the_engine_until_the_next_capture_starts() {
        let done = Mode::Done {
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
        };
        let failed = Mode::Failed(Failure {
            kind: FailureKind::Other,
            notice: "The microphone is off.".to_owned(),
            headline: "Failed",
            causes: Vec::new(),
        });
        let cases = [
            (
                Mode::Starting,
                EngineState::Listening,
                Status::Loading {
                    notice: STARTING_NOTICE.to_owned(),
                    downloading: false,
                },
            ),
            (
                Mode::WakeDownload,
                EngineState::Listening,
                Status::Loading {
                    notice: WAKE_DOWNLOAD_NOTICE.to_owned(),
                    downloading: true,
                },
            ),
            (
                Mode::SpotterDownload,
                EngineState::Listening,
                Status::Loading {
                    notice: SPOTTER_DOWNLOAD_NOTICE.to_owned(),
                    downloading: true,
                },
            ),
            (Mode::Paused, capturing(), Status::Paused),
            (
                Mode::Delivering(CaptureRoute::Submit),
                EngineState::Listening,
                Status::Working {
                    target: Some(kitty()),
                    route: CaptureRoute::Submit,
                    cancellable: false,
                },
            ),
            (
                done.clone(),
                EngineState::Listening,
                Status::Done {
                    target: Some(kitty()),
                    route: CaptureRoute::Paste,
                    notice: "Delivered.".to_owned(),
                },
            ),
            (
                done,
                EngineState::StartingCapture,
                Status::Starting {
                    target: Some(kitty()),
                },
            ),
            (
                failed.clone(),
                EngineState::Listening,
                Status::Failed {
                    kind: FailureKind::Other,
                    notice: "The microphone is off.".to_owned(),
                    headline: "Failed",
                    causes: Vec::new(),
                },
            ),
            (
                failed,
                capturing(),
                Status::Capturing {
                    target: Some(kitty()),
                    committed: "hello".to_owned(),
                    partial: Some("wor".to_owned()),
                    notice: None,
                },
            ),
        ];
        for (mode, engine, expected) in cases {
            let label = format!("{mode:?} {engine:?}");
            assert_eq!(view(mode, engine).status(), expected, "{label}");
        }
    }

    #[test]
    fn the_loading_notice_names_the_download_and_its_percent_and_the_overlay_shows_only_downloads()
    {
        use Export::{Cpu, Gpu};
        for (step, notice, downloading) in [
            (Step::Reading, "Loading the speech model.", false),
            (
                Step::Fetching(Cpu),
                "Loading the speech model. Downloading about 650 MB.",
                true,
            ),
            (
                Step::Fetching(Gpu),
                "Loading the speech model. Downloading the model for the GPU, about 1.3 GB.",
                true,
            ),
            (
                Step::Downloading(Cpu, 7),
                "Downloading the speech model, about 650 MB \u{2014} 7%",
                true,
            ),
            (
                Step::Downloading(Gpu, 100),
                "Downloading the model for the GPU, about 1.3 GB \u{2014} 100%",
                true,
            ),
        ] {
            let mut view = view(Mode::Idle, EngineState::Listening);
            view.loading = Some(step);
            assert_eq!(
                view.status(),
                Status::Loading {
                    notice: notice.to_owned(),
                    downloading
                },
                "{step:?}"
            );
        }
    }

    #[test]
    fn loading_the_speech_model_shows_until_a_capture_starts_a_failure_or_a_pause() {
        let loading = |mode, engine| {
            let mut view = view(mode, engine);
            view.loading = Some(Step::Reading);
            view.status()
        };
        let notice = || "Loading the speech model.".to_owned();
        for mode in [
            Mode::Idle,
            Mode::Done {
                route: CaptureRoute::Paste,
                notice: String::new(),
            },
        ] {
            assert_eq!(
                loading(mode, EngineState::Listening),
                Status::Loading {
                    notice: notice(),
                    downloading: false
                }
            );
        }
        assert_eq!(
            loading(
                Mode::Failed(Failure {
                    kind: FailureKind::Other,
                    notice: "The microphone is off.".to_owned(),
                    headline: "Failed",
                    causes: vec!["open failed".to_owned()],
                }),
                EngineState::Listening
            ),
            Status::Failed {
                kind: FailureKind::Other,
                notice: "The microphone is off.".to_owned(),
                headline: "Failed",
                causes: vec!["open failed".to_owned()],
            }
        );
        assert_eq!(
            loading(Mode::Paused, EngineState::Listening),
            Status::Paused
        );
        assert_eq!(
            loading(Mode::Idle, EngineState::StartingCapture),
            Status::Starting {
                target: Some(kitty())
            }
        );
    }

    fn capturing() -> EngineState {
        EngineState::Capturing {
            committed: "hello".to_owned(),
            partial: Some("wor".to_owned()),
            notice: None,
        }
    }

    #[test]
    fn the_status_is_the_json_the_ui_reads() {
        let cases = [
            (Status::Paused, json!({"state": "paused"})),
            (Status::KeyboardOnly, json!({"state": "keyboardOnly"})),
            (
                Status::Starting { target: None },
                json!({"state": "starting", "target": null}),
            ),
            (
                Status::Capturing {
                    target: Some(kitty()),
                    committed: "a".to_owned(),
                    partial: None,
                    notice: None,
                },
                json!({"state": "capturing", "target": "kitty", "committed": "a", "partial": null, "notice": null}),
            ),
            (
                Status::Capturing {
                    target: None,
                    committed: "a".to_owned(),
                    partial: None,
                    notice: Some("Say it again.".to_owned()),
                },
                json!({"state": "capturing", "target": null, "committed": "a", "partial": null, "notice": "Say it again."}),
            ),
            (
                Status::Pending {
                    target: Some(kitty()),
                    route: CaptureRoute::Submit,
                    text: "hi".to_owned(),
                    waiting_for_quiet: false,
                    pause_ms: 2500,
                    copies: true,
                },
                json!({"state": "pending", "target": "kitty", "route": "submit", "text": "hi",
                       "waitingForQuiet": false, "pauseMs": 2500, "copies": true}),
            ),
            (
                Status::Working {
                    target: Some(kitty()),
                    route: CaptureRoute::Task,
                    cancellable: true,
                },
                json!({"state": "working", "target": "kitty", "route": "task", "cancellable": true}),
            ),
            (
                Status::Done {
                    target: Some(kitty()),
                    route: CaptureRoute::Paste,
                    notice: "Delivered.".to_owned(),
                },
                json!({"state": "done", "target": "kitty", "route": "paste", "notice": "Delivered."}),
            ),
            (
                Status::Failed {
                    kind: FailureKind::Other,
                    notice: "No.".to_owned(),
                    headline: "No",
                    causes: vec!["connection refused (os error 111)".to_owned()],
                },
                json!({"state": "failed", "notice": "No.", "headline": "No", "causes": ["connection refused (os error 111)"], "kind": "other"}),
            ),
            (
                Status::Loading {
                    notice: "Wait.".to_owned(),
                    downloading: true,
                },
                json!({"state": "loading", "notice": "Wait."}),
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(serde_json::to_value(&status).unwrap(), expected);
        }
    }

    #[test]
    fn the_last_error_outlives_the_failure_until_a_newer_one_replaces_it() {
        let failed = |notice: &str| Status::Failed {
            kind: FailureKind::Other,
            notice: notice.to_owned(),
            headline: "Failed",
            causes: vec![format!("cause of {notice}")],
        };
        let last = LastError::default();
        assert_eq!(last.get(), None);
        for (status, expected) in [
            (Status::Listening, None),
            (failed("First."), Some("First.")),
            (Status::Listening, Some("First.")),
            (Status::Paused, Some("First.")),
            (failed("Second."), Some("Second.")),
        ] {
            last.record(&status);
            assert_eq!(
                last.get().map(|failure| failure.notice),
                expected.map(str::to_owned),
                "{status:?}"
            );
        }
        assert_eq!(
            last.get().map(|failure| failure.causes),
            Some(vec!["cause of Second.".to_owned()])
        );
    }

    #[test]
    fn a_failure_carries_the_errors_text_headline_and_causes() {
        let error = RecognizerError::Unreachable {
            speech: SpeechSource::direct("Realtime", "ws://127.0.0.1:9".to_owned()),
            reason: ConnectFailure::Refused,
            detail: Detail("connection refused (os error 111)".to_owned()),
        };
        assert_eq!(
            Failure::of(&error),
            Failure {
                kind: FailureKind::Other,
                notice: error.to_string(),
                headline: "Can't reach speech server — Settings",
                causes: vec!["connection refused (os error 111)".to_owned()],
            }
        );
    }
}

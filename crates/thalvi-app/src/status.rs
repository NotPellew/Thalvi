//! What the UI is told: the engine's state, the pause switch and the delivery outcome as one typed status.

use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use thalvi_desktop::capture::{CaptureRoute, EndingPauses};
use thalvi_desktop::engine::EngineState;
use thalvi_desktop::local::LocalRecognizer;
use thalvi_desktop::report::{UserError, causes};
use thalvi_desktop::transducer::TransducerSource;

pub const STARTING_NOTICE: &str = "Starting listening. The microphone is not ready yet.";
pub const WAKE_DOWNLOAD_NOTICE: &str = "Downloading the wake word model.";

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
    /// The wake word was heard and the recognizer is connecting.
    Starting {
        target: Option<String>,
    },
    Capturing {
        target: Option<String>,
        committed: String,
        partial: Option<String>,
    },
    Pending {
        target: Option<String>,
        route: CaptureRoute,
        text: String,
        waiting_for_quiet: bool,
        /// The pause configured for `route`.
        pause_ms: u64,
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
    },
    /// A model is being fetched or loaded. For the speech model the wake word already works.
    Loading {
        notice: String,
        /// Files are being downloaded; only then does the overlay say so.
        #[serde(skip)]
        downloading: bool,
    },
}

/// What went wrong: the notice for the user and the technical causes for the error report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub notice: String,
    pub headline: &'static str,
    pub causes: Vec<String>,
}

impl Failure {
    /// `error`'s `Display` as the notice, its headline and its source chain as the causes.
    #[must_use]
    pub fn of(error: &(impl UserError + 'static)) -> Self {
        Self {
            notice: error.to_string(),
            headline: error.headline(),
            causes: causes(error),
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
        } = status
        {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(Failure {
                notice: notice.clone(),
                headline,
                causes: causes.clone(),
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

/// What the speech model needs before it is ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechLoad {
    FromDisk,
    Download,
}

impl SpeechLoad {
    pub async fn of(local: &LocalRecognizer<TransducerSource>) -> Self {
        if local.source().is_cached().await {
            Self::FromDisk
        } else {
            Self::Download
        }
    }

    #[must_use]
    pub fn notice(self) -> &'static str {
        match self {
            Self::FromDisk => "Loading the speech model.",
            Self::Download => "Loading the speech model. The first time it downloads about 650 MB.",
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
    pub pauses: EndingPauses,
    /// What the speech model needs, while it loads.
    pub loading: Option<SpeechLoad>,
}

impl View {
    #[must_use]
    pub fn status(&self) -> Status {
        let target = self.target.clone();
        if let (Some(load), EngineState::Listening, Mode::Idle | Mode::Done { .. }) =
            (self.loading, &self.engine, &self.mode)
        {
            return Status::Loading {
                notice: load.notice().to_owned(),
                downloading: load == SpeechLoad::Download,
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
            },
            (Mode::Idle | Mode::Done { .. } | Mode::Failed(_), engine) => match engine {
                EngineState::Listening => Status::Listening,
                EngineState::StartingCapture => Status::Starting { target },
                EngineState::Capturing { committed, partial } => Status::Capturing {
                    target,
                    committed: committed.clone(),
                    partial: partial.clone(),
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
                },
                EngineState::TranscribingSecondary { route } => Status::Working {
                    target,
                    route: *route,
                    cancellable: true,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use thalvi_desktop::recognizer::{ConnectFailure, Detail, RecognizerError, ServerUrl};

    use super::*;

    fn view(mode: Mode, engine: EngineState) -> View {
        View {
            mode,
            engine,
            target: Some("kitty".to_owned()),
            pauses: EndingPauses::default(),
            loading: None,
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
                },
            ),
        ];
        for (mode, engine, expected) in cases {
            let label = format!("{mode:?} {engine:?}");
            assert_eq!(view(mode, engine).status(), expected, "{label}");
        }
    }

    #[test]
    fn loading_the_speech_model_shows_until_a_capture_starts_a_failure_or_a_pause() {
        let loading = |mode, engine| {
            let mut view = view(mode, engine);
            view.loading = Some(SpeechLoad::FromDisk);
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
                    notice: "The microphone is off.".to_owned(),
                    headline: "Failed",
                    causes: vec!["open failed".to_owned()],
                }),
                EngineState::Listening
            ),
            Status::Failed {
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
        }
    }

    #[test]
    fn the_status_is_the_json_the_ui_reads() {
        let cases = [
            (Status::Paused, json!({"state": "paused"})),
            (
                Status::Starting { target: None },
                json!({"state": "starting", "target": null}),
            ),
            (
                Status::Capturing {
                    target: Some(kitty()),
                    committed: "a".to_owned(),
                    partial: None,
                },
                json!({"state": "capturing", "target": "kitty", "committed": "a", "partial": null}),
            ),
            (
                Status::Pending {
                    target: Some(kitty()),
                    route: CaptureRoute::Submit,
                    text: "hi".to_owned(),
                    waiting_for_quiet: false,
                    pause_ms: 2500,
                },
                json!({"state": "pending", "target": "kitty", "route": "submit", "text": "hi",
                       "waitingForQuiet": false, "pauseMs": 2500}),
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
                    notice: "No.".to_owned(),
                    headline: "No",
                    causes: vec!["connection refused (os error 111)".to_owned()],
                },
                json!({"state": "failed", "notice": "No.", "headline": "No", "causes": ["connection refused (os error 111)"]}),
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
            server: ServerUrl::default(),
            reason: ConnectFailure::Refused,
            detail: Detail("connection refused (os error 111)".to_owned()),
        };
        assert_eq!(
            Failure::of(&error),
            Failure {
                notice: error.to_string(),
                headline: "Can't reach speech server — Settings",
                causes: vec!["connection refused (os error 111)".to_owned()],
            }
        );
    }
}

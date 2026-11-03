//! Replays cancellation after the accepted cue with recognizer close held pending.
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thalvi_desktop::audio::InputGate;
use thalvi_desktop::capture::CaptureRoute;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::cues::{Cue, CueError, CueTiming};
use thalvi_desktop::delivery::{Bound, Delivery, Finished};
use thalvi_desktop::draft::Draft;
use thalvi_desktop::draft::Drafts;
use thalvi_desktop::engine::fakes::{FakeFrames, FakeWake};
use thalvi_desktop::engine::{CaptureOutcome, CuePlayer, Engine, Parts};
use thalvi_desktop::frame::Frame;
use thalvi_desktop::history::{Capture, Outcome, Store as History};
use thalvi_desktop::hold::{HoldInput, Presses, Refusal};
use thalvi_desktop::host::{App, Host, HostError, TypedText, WebUrl};
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::recognizer::{
    FakeRecognizer, FakeSession, Recognizer, RecognizerError, RecognizerEvent, RecognizerSession,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Default)]
struct RecordingHost(Arc<Mutex<Vec<&'static str>>>);
impl RecordingHost {
    fn called(&self, call: &'static str) {
        self.0.lock().unwrap().push(call);
    }
}
impl Host for RecordingHost {
    type Target = String;
    const OVERLAY_TAKES_CLICKS: bool = false;
    fn capture_target(&self) -> Result<String, HostError> {
        Ok("Editor".to_owned())
    }
    fn deliver(&self, _: &String, _: &TypedText, _: bool) -> Result<(), HostError> {
        self.called("deliver");
        Ok(())
    }
    fn copy(&self, _: &str) -> Result<(), HostError> {
        self.called("copy");
        Ok(())
    }
    fn focus_app(&self, _: &App) -> Result<String, HostError> {
        self.called("focus_app");
        Ok("App".to_owned())
    }
    fn open_url(&self, _: &WebUrl) -> Result<(), HostError> {
        self.called("open_url");
        Ok(())
    }
    fn open_folder(&self, _: &std::path::Path) -> Result<(), HostError> {
        self.called("open_folder");
        Ok(())
    }
    fn prepare_overlay(&self, _: f64, _: f64) -> Result<f64, HostError> {
        Ok(0.0)
    }
}

// INSERT_RUNNER_SOURCE

struct ImmediateCues;
impl CuePlayer for ImmediateCues {
    async fn play(&self, _: Cue) -> Result<Option<CueTiming>, CueError> {
        Ok(None)
    }
}

struct CloseGate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
struct HeldRecognizer {
    inner: FakeRecognizer,
    gate: Mutex<Option<CloseGate>>,
}
struct HeldSession {
    inner: FakeSession,
    gate: CloseGate,
}
impl Recognizer for HeldRecognizer {
    type Session = HeldSession;
    async fn connect(&self) -> Result<Self::Session, RecognizerError> {
        let gate = self.gate.lock().unwrap().take().unwrap();
        Ok(HeldSession {
            inner: self.inner.connect().await?,
            gate,
        })
    }
}
impl RecognizerSession for HeldSession {
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        self.inner.send_frame(frame).await
    }
    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        self.inner.next_event().await
    }
    async fn cancel(self) {
        self.inner.cancel().await;
    }
    async fn close(self) {
        self.gate.entered.send(()).unwrap();
        self.gate.release.await.unwrap();
        self.inner.close().await;
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_before_close_returns_must_not_deliver() {
    closing_capture(true).await;
}

#[tokio::test(start_paused = true)]
async fn a_press_while_close_is_pending_is_reported_without_losing_delivery() {
    closing_capture(false).await;
}

async fn closing_capture(cancel: bool) {
    let config = DesktopConfig::from_json_str(r#"{"ending_pause":{"paste":0}}"#).unwrap();
    let (inner, mut server) = FakeRecognizer::new();
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let recognizer = HeldRecognizer {
        inner,
        gate: Mutex::new(Some(CloseGate {
            entered: entered_tx,
            release: release_rx,
        })),
    };
    let gate = InputGate::default();
    let (mut frames, _feed) = FakeFrames::new(&gate);
    let (wake, _wake_control) = FakeWake::new();
    let (hold_input, hold) = HoldInput::new();
    let runner_key = hold.clone();
    let (mut engine, handle) = Engine::new(
        Parts {
            recognizer,
            wake,
            cues: ImmediateCues,
            gate,
            http: reqwest::Client::new(),
            hold,
            notice: Box::new(|_| {}),
        },
        &config,
        Recorder::disabled(),
    );
    server.ready();
    server.emit(Ok(RecognizerEvent::Final("keep me paste that".to_owned())));
    let (send, mut controls) = mpsc::unbounded_channel();
    let (consumed, consumed_rx) = oneshot::channel();
    let consumed = Mutex::new(Some(consumed));
    let mut requests = Requests::default();
    let driven = drive(
        engine.capture(&mut frames),
        &mut controls,
        || {
            handle.cancel();
            if let Some(signal) = consumed.lock().unwrap().take() {
                signal.send(()).unwrap();
            }
        },
        &mut requests,
    );
    let ordering = async {
        entered.await.unwrap();
        if cancel {
            send.send(Control::Cancel).unwrap();
            consumed_rx.await.unwrap();
        } else {
            hold_input.set(true);
            hold_input.set(false);
        }
        release.send(()).unwrap();
        // Keep the channel open until drive finishes; disconnection is a separate Quit input.
        send
    };
    let (outcome, _sender) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(driven, ordering)
    })
    .await
    .expect("the event-driven replay must finish");
    assert_eq!(
        engine.take_ignored(),
        if cancel {
            vec![]
        } else {
            vec![Refusal::Capturing]
        },
        "PRESS_DURING_CLOSE_SILENTLY_DROPPED"
    );
    let host = RecordingHost::default();
    let delivery = Delivery::new(host.clone());
    let recorder = Recorder::disabled();
    let bound = delivery.bind(&recorder).await;
    let history = Arc::new(History::at(
        std::env::temp_dir().join(format!("formal-cancel-history-{cancel}.json")),
    ));
    let drafts = Drafts::default();
    let mut key = Presses::new(runner_key);
    let finished = if let Some(outcome) = &outcome {
        Settling {
            delivery: &delivery,
            history: &history,
            drafts: &drafts,
            config: &config,
            recorder: &recorder,
        }
        .settle(outcome, requests.ended, &bound, &mut key, |_, _| {})
        .await
    } else {
        None
    };
    let delivered = finished.as_ref().is_some_and(|finished| finished.delivered);
    let calls = host.0.lock().unwrap().clone();
    eprintln!(
        "Cancel consumed while close pending; outcome={outcome:?}; ended={}; delivered={delivered}; host calls={calls:?}",
        requests.ended
    );
    assert!(
        if cancel {
            !delivered && calls.is_empty()
        } else {
            delivered && calls == ["deliver", "copy"]
        },
        "CANCEL_REACHED_DELIVERY: a consumed Cancel must prevent host delivery"
    );
    // The history is written on a blocking thread: wait for the one entry, bounded.
    let entries = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let entries = history.list().unwrap();
            if !entries.is_empty() {
                return entries;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the cancelled capture's text was not kept");
    assert_eq!(
        (entries.len(), entries[0].outcome, entries[0].text.as_str()),
        (
            1,
            if cancel {
                Outcome::Cancelled
            } else {
                Outcome::Accepted
            },
            "keep me"
        ),
        "a consumed Cancel keeps the dictated text once, as cancelled"
    );
    assert_eq!(
        drafts.view().map(|draft| draft.original),
        Some("keep me".to_owned())
    );
}

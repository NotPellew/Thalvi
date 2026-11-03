//! Replays cancellation after the accepted cue with recognizer close held pending.
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::time::Duration;
use thalvi_desktop::audio::InputGate;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::cues::{Cue, CueError, CueTiming};
use thalvi_desktop::delivery::Delivery;
use thalvi_desktop::host;
#[path = "../src/delivery/fakes.rs"]
mod delivery_fakes;
use delivery_fakes::FakeHost;
use thalvi_desktop::engine::fakes::{FakeFrames, FakeWake};
use thalvi_desktop::engine::{CaptureOutcome, CuePlayer, Engine, Parts};
use thalvi_desktop::frame::Frame;
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::recognizer::{
    FakeRecognizer, FakeSession, Recognizer, RecognizerError, RecognizerEvent, RecognizerSession,
};
use tokio::sync::{mpsc, oneshot};

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
    let (mut engine, handle) = Engine::new(
        Parts {
            recognizer,
            wake,
            cues: ImmediateCues,
            gate,
            http: reqwest::Client::new(),
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
        send.send(Control::Cancel).unwrap();
        consumed_rx.await.unwrap();
        release.send(()).unwrap();
        // Keep the channel open until drive finishes; disconnection is a separate Quit input.
        send
    };
    let (outcome, _sender) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(driven, ordering)
    })
    .await
    .expect("the event-driven replay must finish");
    let delivery = Delivery::new(FakeHost::default());
    let recorder = Recorder::disabled();
    let bound = delivery.bind(&recorder).await;
    let delivered = if let Some(outcome) = &outcome {
        !requests.quit
            && delivery
                .finish(outcome, &bound, &config, &recorder)
                .await
                .is_some_and(|finished| finished.delivered)
    } else {
        false
    };
    eprintln!("Cancel consumed while close pending; outcome={outcome:?}; delivered={delivered}");
    assert!(
        !delivered,
        "CANCEL_REACHED_DELIVERY: a consumed Cancel must prevent host delivery"
    );
    assert!(
        !matches!(outcome, Some(CaptureOutcome::Accepted(_))),
        "cancel must not admit Accepted"
    );
}

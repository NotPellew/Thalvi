//! The local recognizer streams a model's text through the recognizer seam, and reports what each recognition cost.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use serde_json::Value;
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::{LocalRecognizer, ModelSource, SpeechModel, SpeechStream};
use thalvi_desktop::metrics::{RecordKinds, Recorder};
use thalvi_desktop::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerSession};

const WAIT: Duration = Duration::from_secs(5);
const FRAME: Frame = [0; FRAME_SAMPLES];

type Reply = Result<Vec<RecognizerEvent>, RecognizerError>;

/// Answers frame `n` with `script(n)`; the first frame waits for `gate` when one is given.
struct Fake {
    script: fn(usize) -> Reply,
    gate: Mutex<Option<mpsc::Receiver<()>>>,
    seen: Arc<AtomicUsize>,
    done: mpsc::Sender<()>,
}

/// Fails to load this many times, then provides the [`Fake`] it was given.
struct Source {
    failures: AtomicUsize,
    fake: Mutex<Option<Fake>>,
}

impl ModelSource for Source {
    type Model = Fake;

    fn load(&self) -> impl Future<Output = Result<Fake, RecognizerError>> + Send {
        let failing = self
            .failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        std::future::ready(if failing {
            Err(RecognizerError {
                message: "Could not download the speech model.".to_owned(),
            })
        } else {
            self.fake
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .ok_or_else(|| RecognizerError {
                    message: "The fake model loads once.".to_owned(),
                })
        })
    }
}

struct FakeStream {
    script: fn(usize) -> Reply,
    gate: Option<mpsc::Receiver<()>>,
    seen: Arc<AtomicUsize>,
    done: mpsc::Sender<()>,
}

impl SpeechModel for Fake {
    type Stream = FakeStream;

    fn stream(&self) -> FakeStream {
        FakeStream {
            script: self.script,
            gate: self
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
            seen: Arc::clone(&self.seen),
            done: self.done.clone(),
        }
    }
}

impl SpeechStream for FakeStream {
    fn accept(&mut self, _frame: &Frame) -> Reply {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n == 0
            && let Some(gate) = self.gate.take()
        {
            let _ = gate.recv_timeout(WAIT);
        }
        (self.script)(n)
    }
}

impl Drop for FakeStream {
    /// The worker reports its metrics before it drops the stream.
    fn drop(&mut self) {
        let _ = self.done.send(());
    }
}

struct Rig {
    recognizer: LocalRecognizer<Source>,
    recorder: Recorder,
    dir: PathBuf,
    seen: Arc<AtomicUsize>,
    done: mpsc::Receiver<()>,
}

fn rig(
    name: &str,
    script: fn(usize) -> Reply,
    gate: Option<mpsc::Receiver<()>>,
    failures: usize,
) -> Rig {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("local-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let kinds = RecordKinds {
        metrics: true,
        ..RecordKinds::default()
    };
    let recorder = Recorder::in_dir(kinds, &dir).expect("recorder");
    let (done_tx, done) = mpsc::channel();
    let seen = Arc::new(AtomicUsize::new(0));
    let fake = Fake {
        script,
        gate: Mutex::new(gate),
        seen: Arc::clone(&seen),
        done: done_tx,
    };
    Rig {
        recognizer: LocalRecognizer::new(
            Source {
                failures: AtomicUsize::new(failures),
                fake: Mutex::new(Some(fake)),
            },
            "fake-model",
            recorder.metrics(),
        ),
        recorder,
        dir,
        seen,
        done,
    }
}

impl Rig {
    /// Waits for the worker to finish and returns every metrics line written.
    async fn metrics(self) -> Vec<Value> {
        let done = self.done;
        tokio::task::spawn_blocking(move || done.recv_timeout(WAIT))
            .await
            .expect("join")
            .expect("the worker finishes");
        self.recorder.finish().await;
        let text = std::fs::read_to_string(self.dir.join("metrics.jsonl")).expect("metrics");
        text.lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect()
    }

    /// The recognition's metrics line.
    async fn metric(self) -> Value {
        let lines = self.metrics().await;
        lines
            .into_iter()
            .find(|line| line["event"] == "desktop_local_stt")
            .expect("a recognition line")
    }
}

fn partial(text: &str) -> RecognizerEvent {
    RecognizerEvent::Partial(text.to_owned())
}

#[expect(clippy::unnecessary_wraps, reason = "scripts all return a Reply")]
fn dictation(n: usize) -> Reply {
    match n {
        1 => Ok(vec![partial("hello")]),
        2 => Ok(vec![partial("hello world")]),
        3 => Ok(vec![
            RecognizerEvent::Final("hello world".to_owned()),
            partial("and"),
        ]),
        _ => Ok(vec![]),
    }
}

#[tokio::test]
async fn partials_and_finals_arrive_in_order_and_the_recognition_is_reported() {
    let rig = rig("dictation", dictation, None, 0);
    let mut session = rig.recognizer.connect().await.expect("connects");
    for _ in 0..4 {
        session.send_frame(&FRAME).await.expect("sends");
    }
    let mut heard = Vec::new();
    for _ in 0..4 {
        heard.push(
            tokio::time::timeout(WAIT, session.next_event())
                .await
                .expect("in time")
                .expect("text"),
        );
    }
    assert_eq!(
        heard,
        [
            partial("hello"),
            partial("hello world"),
            RecognizerEvent::Final("hello world".to_owned()),
            partial("and"),
        ]
    );
    session.close().await;

    let metric = rig.metric().await;
    assert_eq!(metric["event"], "desktop_local_stt");
    assert_eq!(metric["outcome"], "ok");
    assert_eq!(metric["engine"], "local");
    assert_eq!(metric["device"], "cpu");
    assert_eq!(metric["model"], "fake-model");
    assert_eq!(metric["frames"], 4);
    assert!(metric["duration_ms"].as_f64().is_some());
    assert!(metric["text_late_max_ms"].as_f64().is_some());
}

fn fails_on_the_third_frame(n: usize) -> Reply {
    match n {
        0 => Ok(vec![partial("hel")]),
        1 => Ok(vec![]),
        _ => Err(RecognizerError {
            message: "The speech model stopped working.".to_owned(),
        }),
    }
}

#[tokio::test]
async fn a_model_error_reaches_the_caller_after_the_text_before_it_and_stops_the_work() {
    let rig = rig("error", fails_on_the_third_frame, None, 0);
    let mut session = rig.recognizer.connect().await.expect("connects");
    for _ in 0..6 {
        session.send_frame(&FRAME).await.expect("sends");
    }
    assert_eq!(session.next_event().await, Ok(partial("hel")));
    let error = session.next_event().await.expect_err("fails");
    assert_eq!(error.message, "The speech model stopped working.");
    let seen = Arc::clone(&rig.seen);
    assert_eq!(rig.metric().await["outcome"], "error");
    assert_eq!(seen.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cancelling_drops_the_frames_still_waiting_for_the_model() {
    let (release, gate) = mpsc::channel();
    let rig = rig("cancel", |_| Ok(vec![]), Some(gate), 0);
    let mut session = rig.recognizer.connect().await.expect("connects");
    for _ in 0..5 {
        session.send_frame(&FRAME).await.expect("sends");
    }
    // Cancel while the model is busy with the first frame.
    tokio::time::timeout(WAIT, async {
        while rig.seen.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the model starts on the first frame");
    session.cancel().await;
    let _ = release.send(());

    let metric = rig.metric().await;
    assert_eq!(metric["outcome"], "cancelled");
    assert_eq!(metric["frames"], 1);
}

#[tokio::test]
async fn a_model_that_cannot_be_fetched_is_reported_and_fetched_again_on_the_next_try() {
    let rig = rig("load", dictation, None, 1);
    let error = rig
        .recognizer
        .connect()
        .await
        .err()
        .expect("first try fails");
    assert_eq!(error.message, "Could not download the speech model.");
    let mut session = rig.recognizer.connect().await.expect("second try loads");
    session.send_frame(&FRAME).await.expect("sends");
    session.send_frame(&FRAME).await.expect("sends");
    assert_eq!(session.next_event().await, Ok(partial("hello")));
    session.cancel().await;

    let loads: Vec<(Value, Value)> = rig
        .metrics()
        .await
        .into_iter()
        .filter(|line| line["event"] == "desktop_speech_load")
        .map(|line| (line["outcome"].clone(), line["model"].clone()))
        .collect();
    assert_eq!(
        loads,
        [
            ("error".into(), "fake-model".into()),
            ("ok".into(), "fake-model".into())
        ]
    );
}

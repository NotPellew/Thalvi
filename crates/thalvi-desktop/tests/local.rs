//! The local recognizer streams a model's text through the recognizer seam, and reports what each recognition cost.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use serde_json::Value;
use thalvi_desktop::device::{
    Adapter, Adapters, Device, Download, Export, Fallback, Gpu, Nodes, Placement, Provider, Step,
};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::{LocalRecognizer, ModelSource, SpeechModel, SpeechStream};
use thalvi_desktop::metrics::{RecordKinds, Recorder};
use thalvi_desktop::models::{CatalogSpeech, ModelStore};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::{
    RELEASE_LIMIT, REPLAYED_RELEASE_LIMIT, Recognizer, RecognizerError, RecognizerEvent,
    RecognizerSession,
};
use thalvi_desktop::report::UserError;
use thalvi_desktop::transducer::{SpeechChoice, TransducerSource};
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(5);
const FRAME: Frame = [0; FRAME_SAMPLES];

type Reply = Result<Vec<RecognizerEvent>, RecognizerError>;

/// Holds the model on its first frame: it reports `started`, then waits for `release`.
struct Gate {
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

/// The test's side of a [`Gate`].
struct Hold {
    started: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

fn gate() -> (Hold, Gate) {
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    (
        Hold { started, release },
        Gate {
            started: started_tx,
            release: release_rx,
        },
    )
}

impl Hold {
    /// Blocks the test until the model is on its first frame, without letting a paused clock advance.
    fn model_started(&self) {
        self.started
            .recv_timeout(WAIT)
            .expect("the model starts on the first frame");
    }

    fn release(&self) {
        let _ = self.release.send(());
    }
}

/// Answers frame `n` with `script(n)`; the first frame waits at `gate` when one is given.
struct Fake {
    device: Device,
    placement: Option<Placement>,
    script: fn(usize) -> Reply,
    gate: Mutex<Option<Gate>>,
    seen: Arc<AtomicUsize>,
    done: mpsc::Sender<()>,
    release_limit: Duration,
    /// Words the model holds back until the input ends.
    held_back: Option<&'static str>,
}

/// Fails to load this many times, then provides the [`Fake`]s it was given, one for each load.
struct Source {
    failures: AtomicUsize,
    fakes: Mutex<VecDeque<Fake>>,
    /// The first load to start reports this step and waits to be released.
    held: Mutex<Option<(Step, Arc<Notify>)>>,
    started: AtomicUsize,
}

impl ModelSource for Source {
    type Model = Fake;

    fn load(
        &self,
        step: impl Fn(Step) + Send + Sync,
    ) -> impl Future<Output = Result<Fake, RecognizerError>> + Send {
        self.started.fetch_add(1, Ordering::SeqCst);
        step(Step::Reading);
        let held = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        async move {
            if let Some((held, release)) = held {
                step(held);
                release.notified().await;
            }
            let failing = self
                .failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            if failing {
                Err(RecognizerError::said(
                    "Could not download the speech model.",
                ))
            } else {
                self.fakes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .pop_front()
                    .ok_or_else(|| RecognizerError::said("The fake source has no model left."))
            }
        }
    }
}

struct FakeStream {
    script: fn(usize) -> Reply,
    gate: Option<Gate>,
    seen: Arc<AtomicUsize>,
    done: mpsc::Sender<()>,
    held_back: Option<&'static str>,
}

impl SpeechModel for Fake {
    type Stream = FakeStream;

    fn device(&self) -> Device {
        self.device.clone()
    }

    fn placement(&self) -> Option<Placement> {
        self.placement
    }

    fn release_limit(&self) -> Duration {
        self.release_limit
    }

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
            held_back: self.held_back,
        }
    }
}

impl SpeechStream for FakeStream {
    fn accept(&mut self, _frame: &Frame) -> Reply {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n == 0
            && let Some(gate) = self.gate.take()
        {
            let _ = gate.started.send(());
            let _ = gate.release.recv_timeout(WAIT);
        }
        (self.script)(n)
    }

    fn end_input(&mut self) -> Reply {
        Ok(self.held_back.take().map_or_else(Vec::new, |text| {
            vec![RecognizerEvent::Final(text.to_owned())]
        }))
    }

    fn holds_text_for_end(&self) -> bool {
        self.held_back.is_some()
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
    done_tx: mpsc::Sender<()>,
}

fn rig(name: &str, script: fn(usize) -> Reply, gate: Option<Gate>, failures: usize) -> Rig {
    rig_on(
        name,
        Device::Cpu(Fallback::CpuModel),
        None,
        script,
        gate,
        failures,
    )
}

fn rig_on(
    name: &str,
    device: Device,
    placement: Option<Placement>,
    script: fn(usize) -> Reply,
    gate: Option<Gate>,
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
        device,
        placement,
        script,
        gate: Mutex::new(gate),
        seen: Arc::clone(&seen),
        done: done_tx.clone(),
        release_limit: RELEASE_LIMIT,
        held_back: None,
    };
    Rig {
        recognizer: LocalRecognizer::new(
            Source {
                failures: AtomicUsize::new(failures),
                fakes: Mutex::new(VecDeque::from([fake])),
                held: Mutex::new(None),
                started: AtomicUsize::new(0),
            },
            "fake-model",
            "fake-model",
            recorder.metrics(),
        ),
        recorder,
        dir,
        seen,
        done,
        done_tx,
    }
}

impl Rig {
    /// The model that the next load provides, on the CPU.
    fn then_provide(&self, script: fn(usize) -> Reply) {
        self.recognizer
            .source()
            .fakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(Fake {
                device: Device::Cpu(Fallback::CpuModel),
                placement: None,
                script,
                gate: Mutex::new(None),
                seen: Arc::new(AtomicUsize::new(0)),
                done: self.done_tx.clone(),
                release_limit: RELEASE_LIMIT,
                held_back: None,
            });
    }

    /// Makes the model the next load provides hold `text` back until the input ends.
    fn then_hold_back(&self, text: &'static str) {
        self.recognizer
            .source()
            .fakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .front_mut()
            .expect("a model to load")
            .held_back = Some(text);
    }

    /// Sets how long the model the next load provides takes for its final after the release.
    fn then_release_limit(&self, limit: Duration) {
        self.recognizer
            .source()
            .fakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .front_mut()
            .expect("a model to load")
            .release_limit = limit;
    }

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
async fn the_session_reports_how_long_its_model_takes_for_its_final_after_the_release() {
    for (name, limit) in [
        ("short", RELEASE_LIMIT),
        ("replaying", REPLAYED_RELEASE_LIMIT),
    ] {
        let rig = rig(name, |_| Ok(vec![]), None, 0);
        rig.then_release_limit(limit);
        let session = rig.recognizer.connect().await.expect("connects");
        assert_eq!(session.release_limit(), limit, "{name}");
        session.close().await;
        let _ = rig.metrics().await;
    }
}

#[tokio::test]
async fn a_released_hold_stays_behind_until_the_model_answers_the_end_of_its_audio() {
    // Nemotron Unified holds the last word back until the release flush's silence gives it the
    // right context, so the wait must last until that answer, not until the flush is sent.
    let rig = rig(
        "release-flush",
        |n| match n {
            0 => Ok(vec![partial("reduced our hosting")]),
            _ => Ok(vec![]),
        },
        None,
        0,
    );
    rig.then_hold_back("reduced our hosting bills");
    let mut session = rig.recognizer.connect().await.expect("connects");
    session.send_frame(&FRAME).await.expect("sends");
    session.hold_released();
    for _ in 0..10 {
        session.send_frame(&FRAME).await.expect("sends");
    }
    session.input_ended();
    tokio::time::timeout(WAIT, async {
        assert_eq!(
            session.next_event().await,
            Ok(partial("reduced our hosting"))
        );
        assert!(
            session.behind(),
            "the model has not brought out the words it held back from the release flush yet"
        );
        assert_eq!(
            session.next_event().await,
            Ok(RecognizerEvent::Final(
                "reduced our hosting bills".to_owned()
            ))
        );
        assert_eq!(session.next_event().await, Ok(RecognizerEvent::Ended));
    })
    .await
    .expect("the model answers the end of its input");
    assert!(
        !session.behind(),
        "the model has answered every frame and the end of the hold's audio"
    );
    session.close().await;
    let _ = rig.metrics().await;
}

#[tokio::test]
async fn partials_and_finals_arrive_in_order_and_the_recognition_is_reported() {
    let rig = rig("dictation", dictation, None, 0);
    let mut session = rig.recognizer.connect().await.expect("connects");
    assert!(!session.buffers_audio(), "local recognition streams frames");
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
    assert_eq!(metric["model"], "fake-model");
    assert_eq!(metric["frames"], 4);
    assert!(metric["duration_ms"].as_f64().is_some());
    assert!(metric["text_late_max_ms"].as_f64().is_some());
}

#[tokio::test]
async fn the_load_and_each_recognition_report_the_device_and_any_reason_for_the_cpu() {
    let rtx = Gpu {
        name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
        api: "Vulkan",
    };
    for (name, device, expected) in [
        (
            "gpu",
            Device::Gpu(rtx),
            serde_json::json!({"device": "gpu", "gpu": "NVIDIA GeForce RTX 5070 Ti", "gpu_api": "Vulkan"}),
        ),
        (
            "no-gpu",
            Device::Cpu(Fallback::NoUsableGpu),
            serde_json::json!({"device": "cpu", "fallback": "no_usable_gpu"}),
        ),
        (
            "driver",
            Device::Cpu(Fallback::DriverFailed),
            serde_json::json!({"device": "cpu", "fallback": "driver_failed"}),
        ),
        (
            "partial",
            Device::Cpu(Fallback::CannotRunFully),
            serde_json::json!({"device": "cpu", "fallback": "cannot_run_fully"}),
        ),
        (
            "gpu-files-missing",
            Device::Cpu(Fallback::GpuFilesMissing),
            serde_json::json!({"device": "cpu", "fallback": "gpu_files_missing"}),
        ),
        (
            "cpu-model",
            Device::Cpu(Fallback::CpuModel),
            serde_json::json!({"device": "cpu", "fallback": "cpu_model"}),
        ),
    ] {
        let rig = rig_on(&format!("device-{name}"), device, None, dictation, None, 0);
        let mut session = rig.recognizer.connect().await.expect("connects");
        session.send_frame(&FRAME).await.expect("sends");
        session.close().await;
        let lines = rig.metrics().await;
        for event in ["desktop_speech_load", "desktop_local_stt"] {
            let line = lines
                .iter()
                .find(|line| line["event"] == event)
                .unwrap_or_else(|| panic!("{name}: no {event} line"));
            for (key, value) in expected.as_object().expect("object") {
                assert_eq!(&line[key], value, "{name}: {event} {key}");
            }
            let other = if expected.get("fallback").is_some() {
                "gpu"
            } else {
                "fallback"
            };
            assert!(line.get(other).is_none(), "{name}: {event} carries {other}");
        }
    }
}

fn fails_on_the_third_frame(n: usize) -> Reply {
    match n {
        0 => Ok(vec![partial("hel")]),
        1 => Ok(vec![]),
        _ => Err(RecognizerError::said("The speech model stopped working.")),
    }
}

#[tokio::test]
async fn a_model_error_reaches_the_caller_after_the_text_before_it_and_stops_the_work() {
    let (hold, gate) = gate();
    let rig = rig("error", fails_on_the_third_frame, Some(gate), 0);
    let mut session = rig.recognizer.connect().await.expect("connects");
    // Queue every frame before the model can fail, so no send meets a stopped worker.
    for _ in 0..6 {
        session.send_frame(&FRAME).await.expect("sends");
    }
    hold.release();
    assert_eq!(session.next_event().await, Ok(partial("hel")));
    let error = session.next_event().await.expect_err("fails");
    assert_eq!(error.to_string(), "The speech model stopped working.");
    let seen = Arc::clone(&rig.seen);
    assert_eq!(rig.metric().await["outcome"], "error");
    assert_eq!(seen.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cancelling_drops_the_frames_still_waiting_for_the_model() {
    let (hold, gate) = gate();
    let rig = rig("cancel", |_| Ok(vec![]), Some(gate), 0);
    let mut session = rig.recognizer.connect().await.expect("connects");
    for _ in 0..5 {
        session.send_frame(&FRAME).await.expect("sends");
    }
    // Cancel while the model is busy with the first frame.
    hold.model_started();
    session.cancel().await;
    hold.release();

    let metric = rig.metric().await;
    assert_eq!(metric["outcome"], "cancelled");
    assert_eq!(metric["frames"], 1);
}

/// The model is still on the first frame while the microphone has moved on, as one slower than real time is.
#[tokio::test(start_paused = true)]
async fn a_model_three_seconds_behind_stops_the_recognition_and_names_itself() {
    let (hold, gate) = gate();
    let rig = rig("behind", |_| Ok(vec![]), Some(gate), 0);
    let seen = Arc::clone(&rig.seen);
    let mut session = rig.recognizer.connect().await.expect("connects");
    session.send_frame(&FRAME).await.expect("sends");
    hold.model_started();
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the model starts on the first frame"
    );
    let mut stopped = None;
    for _ in 0..40 {
        match session.send_frame(&FRAME).await {
            Ok(()) => tokio::time::advance(Duration::from_millis(80)).await,
            Err(error) => {
                stopped = Some(error);
                break;
            }
        }
    }
    let error = stopped.expect("a model three seconds behind stops the recognition");
    assert_eq!(
        error.to_string(),
        "This computer can't keep up with fake-model. Nothing was delivered."
    );
    session.cancel().await;
    hold.release();

    let metric = rig.metric().await;
    assert_eq!(metric["outcome"], "cant_keep_up");
    assert_eq!(metric["frames"], 1, "the audio queued behind it is dropped");
    assert!(
        metric["lag_ms"].as_f64().expect("a lag") >= 3_000.0,
        "{metric}"
    );
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the model was never fed the backlog"
    );
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
    assert_eq!(error.to_string(), "Could not download the speech model.");
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

#[tokio::test]
async fn telling_the_step_it_is_already_at_wakes_nobody_watching_the_load() {
    let rig = rig("steps", dictation, None, 0);
    let steps = rig.recognizer.steps();

    rig.recognizer.load().await.expect("loads");

    assert!(!steps.has_changed().expect("the recognizer lives"));
    assert_eq!(*steps.borrow(), Step::Reading);
}

#[tokio::test]
async fn a_catalog_model_that_cannot_be_saved_fails_with_the_model_errors_own_headline_after_trying_the_cpu_files_last()
 {
    for (name, model, discrete_gpu) in [
        ("nemotron_160ms", CatalogSpeech::Nemotron160ms, false),
        ("unified_160ms", CatalogSpeech::Unified160ms, true),
    ] {
        let blocker =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("local-{name}-blocker"));
        std::fs::write(&blocker, b"a file where the model folder should be").expect("writes");
        let checks = Arc::new(AtomicUsize::new(0));
        let adapters = if discrete_gpu {
            let checks = Arc::clone(&checks);
            Adapters::checking(move || {
                checks.fetch_add(1, Ordering::SeqCst);
                Adapter::Discrete(Gpu {
                    name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
                    api: "Vulkan",
                })
            })
        } else {
            Adapters::default()
        };
        let source = TransducerSource::new(
            ModelStore::in_dir(blocker.join("models")),
            SpeechChoice::Catalog(model),
            &WordList::default(),
            adapters,
        );
        let steps = Mutex::new(Vec::new());

        let error = source
            .load(|step| steps.lock().expect("steps").push(step))
            .await
            .err()
            .expect("no folder to fetch into");

        let mut expected = Vec::new();
        if discrete_gpu {
            let Some(gpu_assets) = model.gpu_assets() else {
                panic!("{name} has GPU files");
            };
            expected.push(Step::Fetching(Download {
                export: Export::Gpu,
                size: gpu_assets.size(),
            }));
        }
        expected.push(Step::Fetching(Download {
            export: Export::Cpu,
            size: model.assets().size(),
        }));
        assert_eq!(*steps.lock().expect("steps"), expected, "{name}");
        assert_eq!(error.headline(), "Can't save the model file", "{name}");
        assert!(
            error.to_string().starts_with(&format!(
                "Could not save the speech model {name}.encoder.int8.onnx in "
            )),
            "{name}: the error is the CPU files': {error}"
        );
        assert_eq!(
            checks.load(Ordering::SeqCst),
            usize::from(discrete_gpu),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_catalog_model_is_cached_for_an_export_when_the_files_that_export_loads_are_on_disk() {
    let model = CatalogSpeech::Unified160ms;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("local-cached");
    let _ = std::fs::remove_dir_all(&dir);
    let source = TransducerSource::new(
        ModelStore::in_dir(&dir),
        SpeechChoice::Catalog(model),
        &WordList::default(),
        Adapters::default(),
    );
    let on_disk = |assets: thalvi_desktop::models::SpeechAssets<'_>| {
        for asset in [
            Some(assets.encoder),
            assets.encoder_data,
            Some(assets.decoder),
            Some(assets.joiner),
            Some(assets.tokens),
        ]
        .into_iter()
        .flatten()
        {
            let path = dir.join(asset.file_name);
            std::fs::create_dir_all(path.parent().expect("a folder")).expect("folder");
            std::fs::File::create(path)
                .expect("file")
                .set_len(asset.size)
                .expect("size");
        }
    };
    let cached = async || {
        [
            source.cached(Export::Cpu).await,
            source.cached(Export::Gpu).await,
        ]
    };
    assert_eq!(cached().await, [false, false], "nothing is on disk");
    on_disk(model.assets());
    assert_eq!(
        cached().await,
        [true, false],
        "the int8 files serve only the CPU"
    );
    on_disk(model.assets_of(Export::Gpu));
    assert_eq!(cached().await, [true, true], "the fp16 files serve the GPU");
    let _ = std::fs::remove_dir_all(&dir);
}

fn fails_at_once(_: usize) -> Reply {
    Err(RecognizerError::said("The speech model stopped working."))
}

#[tokio::test]
async fn a_model_error_drops_the_model_so_the_next_capture_loads_and_plans_again() {
    let rig = rig("lost", fails_at_once, None, 0);
    assert_eq!(rig.recognizer.name(), "fake-model");
    rig.then_provide(dictation);
    let mut lost = rig.recognizer.connect().await.expect("connects");
    assert_eq!(
        rig.recognizer.device(),
        Some(Device::Cpu(Fallback::CpuModel)),
        "loaded"
    );
    lost.send_frame(&FRAME).await.expect("sends");
    let error = lost.next_event().await.expect_err("the capture fails");
    assert_eq!(error.to_string(), "The speech model stopped working.");
    assert_eq!(rig.recognizer.device(), None, "no model is claimed to run");
    let _ = rig.done.recv_timeout(WAIT);

    let mut next = rig.recognizer.connect().await.expect("loads again");
    assert!(rig.recognizer.device().is_some(), "loaded again");
    next.send_frame(&FRAME).await.expect("sends");
    next.send_frame(&FRAME).await.expect("sends");
    assert_eq!(next.next_event().await, Ok(partial("hello")));
    next.close().await;

    let loads: Vec<Value> = rig
        .metrics()
        .await
        .into_iter()
        .filter(|line| line["event"] == "desktop_speech_load")
        .map(|line| line["outcome"].clone())
        .collect();
    assert_eq!(loads, ["ok", "ok"]);
}

#[tokio::test]
async fn a_failed_reload_leaves_no_model_claimed_to_run() {
    let rig = rig("reload-fails", fails_at_once, None, 0);
    let mut lost = rig.recognizer.connect().await.expect("connects");
    lost.send_frame(&FRAME).await.expect("sends");
    lost.next_event().await.expect_err("the capture fails");

    let error = rig.recognizer.connect().await.err().expect("no model left");
    assert_eq!(error.to_string(), "The fake source has no model left.");
    assert_eq!(rig.recognizer.device(), None);
}

#[tokio::test]
async fn the_load_reports_how_many_encoder_nodes_the_gpu_took() {
    let gpu = Gpu {
        name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
        api: "Vulkan",
    };
    let placement = Placement {
        encoder: Nodes {
            on_gpu: 4_950,
            total: 5_000,
        },
    };
    let rig = rig_on(
        "placement",
        Device::Gpu(gpu),
        Some(placement),
        dictation,
        None,
        0,
    );
    let mut session = rig.recognizer.connect().await.expect("connects");
    session.send_frame(&FRAME).await.expect("sends");
    session.close().await;

    let lines = rig.metrics().await;
    let load = lines
        .iter()
        .find(|line| line["event"] == "desktop_speech_load")
        .expect("a load line");
    for (key, value) in [("encoder_nodes_on_gpu", 4_950), ("encoder_nodes", 5_000)] {
        assert_eq!(load[key], value, "{key}");
    }
}

#[tokio::test]
async fn a_load_goes_on_after_the_dictation_that_began_it_gives_up_and_a_failed_one_is_not_left_downloading()
 {
    let downloading = Step::Downloading(
        Download {
            export: Export::Cpu,
            size: CatalogSpeech::Nemotron160ms.assets().size(),
        },
        40,
    );
    for failures in [0, 1] {
        let rig = rig(&format!("detached-{failures}"), dictation, None, failures);
        let release = Arc::new(Notify::new());
        *rig.recognizer
            .source()
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((downloading, Arc::clone(&release)));
        let mut steps = rig
            .recognizer
            .loading()
            .expect("a local recognizer shows its load");
        tokio::select! {
            _ = rig.recognizer.connect() => panic!("the download has not finished"),
            seen = tokio::time::timeout(WAIT, steps.wait_for(|step| *step == downloading)) => {
                seen.expect("the download is shown in time").map(|_| ()).expect("steps");
            }
        }
        // The dictation stopped waiting, which dropped its connect.
        release.notify_one();
        if failures == 0 {
            let session = tokio::time::timeout(WAIT, rig.recognizer.connect())
                .await
                .expect("in time")
                .expect("the model the first load fetched");
            session.cancel().await;
            assert_eq!(rig.recognizer.source().started.load(Ordering::SeqCst), 1);
            assert!(rig.recognizer.device().is_some());
        } else {
            tokio::time::timeout(WAIT, steps.wait_for(|step| *step == Step::Reading))
                .await
                .expect("in time")
                .expect("steps");
            assert!(rig.recognizer.device().is_none());
        }
    }
}

#[tokio::test]
async fn everyone_waiting_for_a_load_gets_its_outcome_instead_of_starting_another() {
    let rig = rig("shared-load", dictation, None, 1);
    let release = Arc::new(Notify::new());
    *rig.recognizer
        .source()
        .held
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((Step::Reading, Arc::clone(&release)));
    let (preload, dictation, ()) = tokio::join!(
        rig.recognizer.load(),
        async { rig.recognizer.connect().await.err() },
        async {
            tokio::task::yield_now().await;
            release.notify_one();
        }
    );
    let failed = "Could not download the speech model.";
    assert_eq!(preload.expect_err("the load fails").to_string(), failed);
    assert_eq!(
        dictation.expect("the dictation fails too").to_string(),
        failed
    );
    assert_eq!(rig.recognizer.source().started.load(Ordering::SeqCst), 1);
}

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
    Adapter, Adapters, Device, Export, Fallback, Gpu, Nodes, Placement, Provider, Step,
};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::{LocalRecognizer, ModelSource, SpeechModel, SpeechStream};
use thalvi_desktop::metrics::{RecordKinds, Recorder};
use thalvi_desktop::models::{CatalogSpeech, ModelStore};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerSession};
use thalvi_desktop::report::UserError;
use thalvi_desktop::transducer::{SpeechChoice, TransducerSource};

const WAIT: Duration = Duration::from_secs(5);
const FRAME: Frame = [0; FRAME_SAMPLES];

type Reply = Result<Vec<RecognizerEvent>, RecognizerError>;

/// Answers frame `n` with `script(n)`; the first frame waits for `gate` when one is given.
struct Fake {
    device: Device,
    placement: Option<Placement>,
    script: fn(usize) -> Reply,
    gate: Mutex<Option<mpsc::Receiver<()>>>,
    seen: Arc<AtomicUsize>,
    done: mpsc::Sender<()>,
}

/// Fails to load this many times, then provides the [`Fake`]s it was given, one for each load.
struct Source {
    failures: AtomicUsize,
    fakes: Mutex<VecDeque<Fake>>,
}

impl ModelSource for Source {
    type Model = Fake;

    fn load(
        &self,
        _step: impl Fn(Step) + Send + Sync,
    ) -> impl Future<Output = Result<Fake, RecognizerError>> + Send {
        let failing = self
            .failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        std::future::ready(if failing {
            Err(RecognizerError::said(
                "Could not download the speech model.",
            ))
        } else {
            self.fakes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| RecognizerError::said("The fake source has no model left."))
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

    fn device(&self) -> Device {
        self.device.clone()
    }

    fn placement(&self) -> Option<Placement> {
        self.placement
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
    done_tx: mpsc::Sender<()>,
}

fn rig(
    name: &str,
    script: fn(usize) -> Reply,
    gate: Option<mpsc::Receiver<()>>,
    failures: usize,
) -> Rig {
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
        device,
        placement,
        script,
        gate: Mutex::new(gate),
        seen: Arc::clone(&seen),
        done: done_tx.clone(),
    };
    Rig {
        recognizer: LocalRecognizer::new(
            Source {
                failures: AtomicUsize::new(failures),
                fakes: Mutex::new(VecDeque::from([fake])),
            },
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
            });
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
        (
            "windows",
            Device::Cpu(Fallback::WindowsUnsupported),
            serde_json::json!({"device": "cpu", "fallback": "windows_unsupported"}),
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
    let (release, gate) = mpsc::channel();
    let rig = rig("error", fails_on_the_third_frame, Some(gate), 0);
    let mut session = rig.recognizer.connect().await.expect("connects");
    // Queue every frame before the model can fail, so no send meets a stopped worker.
    for _ in 0..6 {
        session.send_frame(&FRAME).await.expect("sends");
    }
    let _ = release.send(());
    assert_eq!(session.next_event().await, Ok(partial("hel")));
    let error = session.next_event().await.expect_err("fails");
    assert_eq!(error.to_string(), "The speech model stopped working.");
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
async fn a_catalog_model_that_cannot_be_provided_fails_with_the_model_errors_own_headline() {
    let blocker = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("local-model-blocker");
    std::fs::write(&blocker, b"a file where the model folder should be").expect("writes");
    let source = TransducerSource::new(
        ModelStore::in_dir(blocker.join("models")),
        SpeechChoice::Catalog(CatalogSpeech::Nemotron160ms),
        &WordList::default(),
        Adapters::default(),
    );

    let error = source
        .load(|_| {})
        .await
        .err()
        .expect("no folder to fetch into");

    assert_eq!(error.headline(), "Can't save the model file");
    assert!(
        error
            .to_string()
            .starts_with("Could not save the speech model nemotron_160ms.encoder.int8.onnx in "),
        "{error}"
    );
}

#[tokio::test]
async fn a_gpu_plan_that_cannot_save_its_files_tries_the_cpu_files_next_and_each_load_checks_the_adapter_once()
 {
    let blocker = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("local-gpu-blocker");
    std::fs::write(&blocker, b"a file where the model folder should be").expect("writes");
    let checks = Arc::new(AtomicUsize::new(0));
    let adapters = Adapters::checking({
        let checks = Arc::clone(&checks);
        move || {
            checks.fetch_add(1, Ordering::SeqCst);
            Adapter::Discrete(Gpu {
                name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
                api: "Vulkan",
            })
        }
    });
    let source = TransducerSource::new(
        ModelStore::in_dir(blocker.join("models")),
        SpeechChoice::Catalog(CatalogSpeech::Unified160ms),
        &WordList::default(),
        adapters,
    );
    let steps = Mutex::new(Vec::new());

    let error = source
        .load(|step| steps.lock().expect("steps").push(step))
        .await
        .err()
        .expect("no folder to fetch into");

    assert_eq!(
        *steps.lock().expect("steps"),
        [Step::Fetching(Export::Gpu), Step::Fetching(Export::Cpu)]
    );
    assert!(
        error
            .to_string()
            .starts_with("Could not save the speech model unified_160ms.encoder.int8.onnx in "),
        "the error is the CPU files': {error}"
    );
    assert_eq!(checks.load(Ordering::SeqCst), 1);
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
async fn the_load_reports_how_many_nodes_of_each_network_the_gpu_took() {
    let gpu = Gpu {
        name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
        api: "Vulkan",
    };
    let nodes = |on_gpu, total| Nodes { on_gpu, total };
    let placement = Placement {
        encoder: nodes(4_950, 5_000),
        decoder: nodes(12, 30),
        joiner: nodes(6, 6),
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
    for (key, value) in [
        ("encoder_nodes_on_gpu", 4_950),
        ("encoder_nodes", 5_000),
        ("decoder_nodes_on_gpu", 12),
        ("decoder_nodes", 30),
        ("joiner_nodes_on_gpu", 6),
        ("joiner_nodes", 6),
    ] {
        assert_eq!(load[key], value, "{key}");
    }
}

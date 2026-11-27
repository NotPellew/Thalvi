//! Owns the Whisper Turbo helper process and the recognizer that transcribes each finished capture through it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::future::Future;
use std::io::{self, BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle as Thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Notify, OnceCell, oneshot, watch};
use tokio::task::JoinHandle;

use crate::chosen::Preload;
use crate::device::{self, Device, Download, Export, Fallback, Gpu, Step};
use crate::frame::Frame;
use crate::metrics::{Event, Metrics};
use crate::models::{ModelStore, WHISPER_TURBO, WhisperAssets};
use crate::phrases::WordList;
use crate::recognizer::{
    Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, TranscriptionFailure,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use self::linux as native;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use self::windows as native;

/// How long the helper has to load the model and answer its health check.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(25);
/// What a request is given besides one second per second of audio.
const BASE_TIMEOUT: Duration = Duration::from_secs(20);
/// Audio a Vulkan capture must hold before its first preview: a shorter one is a click or a breath.
const PREVIEW_MIN_AUDIO: Duration = Duration::from_secs(1);
/// New audio that must arrive before the next preview starts: each one re-reads the whole capture on the GPU.
const PREVIEW_STEP: Duration = Duration::from_millis(800);
/// What a preview is given: it only shows text, so one that is slow is dropped for the next.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the engine waits past a request's own timeout, so that timeout's error is the one shown.
const FINAL_MARGIN: Duration = Duration::from_secs(1);
const SAMPLE_RATE: u32 = 16_000;
const BOUNDARY: &str = "thalvi-whisper-boundary-4c1e9a7d2b";
/// Lines of the helper's output that are kept: enough for the load's own report, which names the device.
const KEPT_LINES: usize = 200;
/// Lines of it that an error shows.
const SHOWN_LINES: usize = 6;
/// The exit code of a Windows process that could not find a DLL it links.
const STATUS_DLL_NOT_FOUND: u32 = 0xC000_0135;

/// Which build of the helper runs the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhisperBackend {
    Cpu,
    Vulkan,
}

impl WhisperBackend {
    pub const ALL: [Self; 2] = [Self::Cpu, Self::Vulkan];

    /// The name used in config.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "whisper_turbo_cpu",
            Self::Vulkan => "whisper_turbo_vulkan",
        }
    }

    /// The backend called `name` in config.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|backend| backend.name() == name)
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Cpu => "Whisper Turbo \u{b7} CPU (experimental)",
            Self::Vulkan => "Whisper Turbo \u{b7} Vulkan GPU (experimental)",
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Cpu => {
                "Experimental. OpenAI's Whisper Turbo, run by a helper that Thalvi starts. Text appears only after you stop speaking. On the CPU it is slow: the text can take as long as you spoke, or longer. The language is detected automatically; only English and German have been tried."
            }
            Self::Vulkan => {
                "Experimental. OpenAI's Whisper Turbo, run by a helper that Thalvi starts on your graphics card through Vulkan. Text appears while you speak, because the capture so far is read again on the GPU about every second, which is extra GPU work; the final text replaces it after you stop. Its speed on your GPU is untested. If no GPU is found, it says so; pick the CPU entry then. The language is detected automatically; only English and German have been tried."
            }
        }
    }

    /// Whether text is shown while the user speaks, which only a GPU keeps up with.
    fn previews(self) -> bool {
        self == Self::Vulkan
    }

    fn folder(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Vulkan => "vulkan",
        }
    }

    fn device_arguments(self) -> &'static [&'static str] {
        match self {
            Self::Cpu => &["-ng"],
            Self::Vulkan => &["-dev", "0"],
        }
    }
}

/// What one helper is started with.
#[derive(Clone, Debug)]
pub struct Launch {
    pub backend: WhisperBackend,
    pub executable: PathBuf,
    pub model: PathBuf,
    pub vad: PathBuf,
    pub port: u16,
    pub threads: usize,
}

impl Launch {
    fn arguments(&self) -> Vec<OsString> {
        let mut arguments: Vec<OsString> = vec!["-m".into(), self.model.clone().into()];
        // A 400 ms pad keeps word onsets the default 30 ms cut; timestamps (no `-nt`) keep the
        // decoder from skipping sentences after the first 30 s window (#481 measurements).
        arguments.extend(["--vad".into(), "-vm".into(), self.vad.clone().into()]);
        arguments.extend(["-vp".into(), "400".into()]);
        arguments.extend(["--host".into(), "127.0.0.1".into()]);
        arguments.extend(["--port".into(), self.port.to_string().into()]);
        arguments.extend(["-t".into(), self.threads.to_string().into()]);
        // Greedy single candidate (`-bo 1`) and `-nlp` keep `verbose_json` language detection free.
        arguments.extend(["-bo".into(), "1".into(), "-nlp".into()]);
        arguments.extend(self.backend.device_arguments().iter().map(OsString::from));
        arguments
    }
}

/// The threads the helper computes with: half the logical ones, between one and eight.
fn helper_threads(logical: usize) -> usize {
    (logical / 2).clamp(1, 8)
}

/// How a helper that ended, ended: the exit code, if it had one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit(pub Option<i32>);

/// Why a helper could not be provided. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error(
        "This Thalvi build has no Whisper helper (expected {}). Pick another model under Settings \u{2192} Recognition.",
        .0.display()
    )]
    Missing(PathBuf),
    #[error("Thalvi could not start the Whisper helper: {0}")]
    Spawn(#[source] io::Error),
    #[error("Thalvi could not find a free port on this computer for the Whisper helper: {0}")]
    Port(#[source] io::Error),
    #[error("{}", exit_message(*.backend, *.exit, .tail))]
    Exited {
        backend: WhisperBackend,
        exit: Exit,
        /// The last lines the helper wrote.
        tail: Vec<String>,
    },
    #[error("The Whisper helper did not become ready in {} seconds.", READY_TIMEOUT.as_secs())]
    NotReady,
    #[error("The Whisper helper started but could not transcribe its first request: {0}")]
    WarmUp(String),
    #[error(
        "The Vulkan helper found no usable GPU, so Whisper would not run on one. Pick Whisper Turbo \u{b7} CPU under Settings \u{2192} Recognition."
    )]
    NoGpu,
}

impl HelperError {
    /// The helper could not take the port it was given, which another program took after it was found free.
    fn port_taken(&self) -> bool {
        matches!(self, Self::Exited { tail, .. } if tail.iter().any(|line| line.contains("couldn't bind")))
    }
}

/// What the user is told when the helper ended before it was ready.
fn exit_message(backend: WhisperBackend, exit: Exit, tail: &[String]) -> String {
    let dll_not_found = exit.0.map(i32::cast_unsigned) == Some(STATUS_DLL_NOT_FOUND);
    if backend == WhisperBackend::Vulkan && dll_not_found {
        return "vulkan-1.dll was not found; it comes with the graphics driver. Pick Whisper Turbo \u{b7} CPU under Settings \u{2192} Recognition."
            .to_owned();
    }
    let how = exit
        .0
        .map_or_else(String::new, |code| format!(" (exit code {code})"));
    let last = if tail.is_empty() {
        "It wrote nothing.".to_owned()
    } else {
        format!("Its last messages: {}", tail.join(" | "))
    };
    format!("The Whisper helper stopped while starting{how}. {last}")
}

/// The device the helper's own report says it uses: the CPU for the CPU helper, and for the Vulkan helper the
/// GPU it names, or an error when it found none, so a GPU is never claimed that does not run the model.
fn device_of(backend: WhisperBackend, output: &[String]) -> Result<Device, HelperError> {
    if backend == WhisperBackend::Cpu {
        return Ok(Device::Cpu(Fallback::CpuChoice));
    }
    let index = output
        .iter()
        .find_map(|line| {
            line.rsplit_once("using Vulkan")?
                .1
                .strip_suffix(" backend")?
                .parse::<usize>()
                .ok()
        })
        .ok_or(HelperError::NoGpu)?;
    let listed = format!("ggml_vulkan: {index} = ");
    let name = output
        .iter()
        .find_map(|line| line.strip_prefix(&listed))
        .and_then(|rest| rest.split(" | ").next())
        .map_or_else(|| format!("Vulkan device {index}"), str::to_owned);
    Ok(Device::Gpu(Gpu {
        name,
        api: "Vulkan",
    }))
}

/// A running helper, as far as the recognizer can see it.
pub trait Process: Send + Sync + 'static {
    /// What the helper has written to its error output so far, oldest first.
    fn output(&self) -> Vec<String>;

    /// How it ended, once it has, with all it wrote readable.
    fn exit(&self) -> Option<Exit>;

    /// Ends it now and waits until it is gone.
    fn kill(&self);
}

/// Starts helpers; the real one starts `whisper-server`, and tests script one.
pub trait Launcher: Send + Sync + 'static {
    type Process: Process;

    /// Starts a helper that listens on `launch.port`.
    ///
    /// # Errors
    /// Fails when the executable is missing or cannot start.
    fn launch(&self, launch: &Launch) -> Result<Self::Process, HelperError>;
}

/// Starts the packaged `whisper-server`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessLauncher;

/// A `whisper-server` process that Thalvi owns; it ends with its owner.
pub struct HelperProcess {
    child: Mutex<Child>,
    output: Arc<Mutex<VecDeque<String>>>,
    reader: Mutex<Option<Thread<()>>>,
}

impl Launcher for ProcessLauncher {
    type Process = HelperProcess;

    fn launch(&self, launch: &Launch) -> Result<HelperProcess, HelperError> {
        let mut command = Command::new(&launch.executable);
        command
            .args(launch.arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        native::prepare(&mut command);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                HelperError::Missing(launch.executable.clone())
            } else {
                HelperError::Spawn(error)
            }
        })?;
        if let Err(error) = native::bind(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HelperError::Spawn(error));
        }
        let output = Arc::new(Mutex::new(VecDeque::new()));
        let reader = child
            .stderr
            .take()
            .map(|stderr| read_lines(stderr, Arc::clone(&output)))
            .transpose();
        match reader {
            Ok(reader) => Ok(HelperProcess {
                child: Mutex::new(child),
                output,
                reader: Mutex::new(reader),
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(HelperError::Spawn(error))
            }
        }
    }
}

/// Keeps the last lines the helper writes, on a thread that ends when the helper does.
fn read_lines(stderr: ChildStderr, kept: Arc<Mutex<VecDeque<String>>>) -> io::Result<Thread<()>> {
    std::thread::Builder::new()
        .name("thalvi-whisper-log".to_owned())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut kept = locked(&kept);
                if kept.len() == KEPT_LINES {
                    kept.pop_front();
                }
                kept.push_back(line);
            }
        })
}

impl HelperProcess {
    /// The operating system's number of the process.
    #[must_use]
    pub fn id(&self) -> u32 {
        locked(&self.child).id()
    }

    fn read_all_output(&self) {
        if let Some(reader) = locked(&self.reader).take() {
            let _ = reader.join();
        }
    }
}

impl Process for HelperProcess {
    fn output(&self) -> Vec<String> {
        locked(&self.output).iter().cloned().collect()
    }

    fn exit(&self) -> Option<Exit> {
        let status = locked(&self.child).try_wait().ok().flatten()?;
        self.read_all_output();
        Some(Exit(status.code()))
    }

    fn kill(&self) {
        let mut child = locked(&self.child);
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A helper that has answered, and the device it reported.
struct Running<P: Process> {
    process: P,
    port: u16,
    device: Device,
    client: reqwest::Client,
}

impl<P: Process> Running<P> {
    fn alive(&self) -> bool {
        self.process.exit().is_none()
    }
}

impl<P: Process> Drop for Running<P> {
    fn drop(&mut self) {
        self.process.kill();
    }
}

/// How a request to the helper failed.
enum Failure {
    Timeout,
    Unreachable,
    Status(u16),
    BadReply,
}

impl Failure {
    fn error(&self) -> RecognizerError {
        let (failure, what) = match self {
            Self::Timeout => (
                TranscriptionFailure::Timeout,
                "took too long to transcribe this capture".to_owned(),
            ),
            Self::Unreachable => (
                TranscriptionFailure::Unreachable,
                "could not be reached".to_owned(),
            ),
            Self::Status(code) => (
                TranscriptionFailure::HttpStatus,
                format!("answered HTTP {code}"),
            ),
            Self::BadReply => (
                TranscriptionFailure::BadReply,
                "returned an unreadable transcription".to_owned(),
            ),
        };
        RecognizerError::Transcription {
            failure,
            message: format!(
                "Thalvi's Whisper helper {what}. Nothing was delivered. It starts again for the next capture."
            ),
        }
    }
}

/// 16 kHz mono 16-bit WAV of `samples`.
fn wav(samples: &[i16]) -> Vec<u8> {
    let length = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&length.saturating_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&length.to_le_bytes());
    out.extend(samples.iter().copied().flat_map(i16::to_le_bytes));
    out
}

/// The multipart body of an inference request: the form `fields`, then the audio as `file`.
fn multipart(fields: &[(&str, &str)], wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"capture.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// What a request for a capture of `frames` frames is given: twenty seconds and a second per second of audio.
fn request_timeout(frames: usize) -> Duration {
    BASE_TIMEOUT + Duration::from_secs_f64(audio_seconds(frames))
}

/// How long `frames` frames of audio last.
fn audio_time(frames: usize) -> Duration {
    let samples = u64::try_from(frames * crate::frame::FRAME_SAMPLES).unwrap_or(u64::MAX);
    Duration::from_micros(samples.saturating_mul(1_000_000) / u64::from(SAMPLE_RATE))
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a capture has far fewer frames than f64 counts exactly"
)]
fn audio_seconds(frames: usize) -> f64 {
    (frames * crate::frame::FRAME_SAMPLES) as f64 / f64::from(SAMPLE_RATE)
}

/// What one inference answered: normalized text and any detected language name.
struct Transcript {
    text: String,
    language: Option<String>,
}

/// The text of one inference, with its lines run together, plus the detected language if reported.
async fn infer(
    client: &reqwest::Client,
    port: u16,
    fields: &[(&str, &str)],
    wav: &[u8],
    timeout: Duration,
) -> Result<Transcript, Failure> {
    let request = client
        .post(format!("http://127.0.0.1:{port}/inference"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart(fields, wav));
    let answer = async {
        let response = request.send().await.map_err(|_| Failure::Unreachable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Failure::Status(status.as_u16()));
        }
        response
            .json::<Value>()
            .await
            .map_err(|_| Failure::BadReply)
    };
    let answer = tokio::time::timeout(timeout, answer)
        .await
        .map_err(|_| Failure::Timeout)??;
    let text = answer
        .get("text")
        .and_then(Value::as_str)
        .ok_or(Failure::BadReply)?;
    let language = answer
        .get("language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|language| !language.is_empty() && !language.eq_ignore_ascii_case("auto"))
        .map(str::to_owned);
    Ok(Transcript {
        text: text.split_whitespace().collect::<Vec<_>>().join(" "),
        language,
    })
}

async fn healthy(client: &reqwest::Client, port: u16) -> bool {
    client
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(HEALTH_TIMEOUT)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

type Outcome<P> = Option<Result<Arc<Running<P>>, RecognizerError>>;

/// Where the recognizer's helper is.
enum Slot<P: Process> {
    Idle,
    /// A start is under way, which every caller that needs the helper waits for.
    Starting(watch::Receiver<Outcome<P>>),
    Ready(Arc<Running<P>>),
}

struct Shared<L: Launcher> {
    launcher: L,
    backend: WhisperBackend,
    /// The folder with a `cpu` and a `vulkan` folder of helpers.
    helpers: PathBuf,
    store: ModelStore,
    metrics: Metrics,
    name: String,
    /// The word list as the helper's prompt, `, `-joined; `None` when it is empty.
    prompt: Option<Arc<str>>,
    steps: watch::Sender<Step>,
    /// The model and voice detector, once their checksums have been verified, which a restart need not repeat.
    files: OnceCell<(PathBuf, PathBuf)>,
    slot: Mutex<Slot<L::Process>>,
    /// Set when the recognizer is dropped, which stops a start under way.
    closed: watch::Sender<bool>,
    assets: WhisperAssets<'static>,
}

/// Recognizes each finished capture through a Whisper helper that this recognizer starts and owns.
pub struct WhisperRecognizer<L: Launcher = ProcessLauncher> {
    shared: Arc<Shared<L>>,
}

impl WhisperRecognizer {
    /// `helpers` holds the packaged `cpu` and `vulkan` folders; nothing is downloaded or started until
    /// [`Preload::load`] or the first dictation.
    #[must_use]
    pub fn new(
        backend: WhisperBackend,
        helpers: &Path,
        store: ModelStore,
        metrics: Metrics,
        phrases: &WordList,
    ) -> Self {
        Self::with_launcher(
            ProcessLauncher,
            WHISPER_TURBO,
            backend,
            helpers,
            store,
            metrics,
            phrases,
        )
    }
}

impl<L: Launcher> WhisperRecognizer<L> {
    fn with_launcher(
        launcher: L,
        assets: WhisperAssets<'static>,
        backend: WhisperBackend,
        helpers: &Path,
        store: ModelStore,
        metrics: Metrics,
        phrases: &WordList,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                launcher,
                backend,
                helpers: helpers.to_owned(),
                store,
                metrics,
                name: backend.name().to_owned(),
                prompt: (!phrases.is_empty()).then(|| Arc::from(phrases.hint())),
                steps: watch::channel(Step::Reading).0,
                files: OnceCell::new(),
                slot: Mutex::new(Slot::Idle),
                closed: watch::channel(false).0,
                assets,
            }),
        }
    }
}

impl<L: Launcher> Drop for WhisperRecognizer<L> {
    fn drop(&mut self) {
        self.shared.closed.send_replace(true);
    }
}

impl<L: Launcher> Shared<L> {
    fn tell(&self, step: Step) {
        self.steps
            .send_if_modified(|current| std::mem::replace(current, step) != step);
    }

    /// The helper, started now unless one is ready or starting.
    fn begin(self: &Arc<Self>) -> watch::Receiver<Outcome<L::Process>> {
        let mut slot = locked(&self.slot);
        match &*slot {
            Slot::Starting(outcome) => return outcome.clone(),
            Slot::Ready(running) if running.alive() => {
                return watch::channel(Some(Ok(Arc::clone(running)))).1;
            }
            Slot::Idle | Slot::Ready(_) => {}
        }
        let (done, outcome) = watch::channel(None);
        if *self.closed.borrow() {
            done.send_replace(Some(Err(RecognizerError::said(
                "The Whisper helper was stopped with its settings.",
            ))));
            return outcome;
        }
        *slot = Slot::Starting(outcome.clone());
        drop(slot);
        let shared = Arc::clone(self);
        let mut closed = self.closed.subscribe();
        tokio::spawn(async move {
            let started = tokio::select! {
                started = shared.start() => started.map(Arc::new),
                _ = closed.wait_for(|closed| *closed) => {
                    Err(RecognizerError::said("The Whisper helper was stopped with its settings."))
                }
            };
            *locked(&shared.slot) = match &started {
                Ok(running) => Slot::Ready(Arc::clone(running)),
                Err(_) => Slot::Idle,
            };
            done.send_replace(Some(started));
        });
        outcome
    }

    async fn running(self: &Arc<Self>) -> Result<Arc<Running<L::Process>>, RecognizerError> {
        let mut outcome = self.begin();
        let ended = outcome
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|ended| ended.clone());
        ended.unwrap_or_else(|| {
            Err(RecognizerError::said(
                "Starting the Whisper helper stopped unexpectedly.",
            ))
        })
    }

    /// Ends a helper whose request was abandoned or failed, which it may still be working on, and
    /// starts the next one, so a new capture never waits behind the old.
    fn discard(self: &Arc<Self>, running: &Arc<Running<L::Process>>) {
        running.process.kill();
        let mut slot = locked(&self.slot);
        if matches!(&*slot, Slot::Ready(current) if Arc::ptr_eq(current, running)) {
            *slot = Slot::Idle;
        }
        drop(slot);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let _entered = runtime.enter();
            drop(self.begin());
        }
    }

    /// Downloads the model and the voice detector if the store lacks them, and checks them once.
    async fn fetch(&self) -> Result<&(PathBuf, PathBuf), RecognizerError> {
        self.files.get_or_try_init(|| self.download()).await
    }

    async fn download(&self) -> Result<(PathBuf, PathBuf), RecognizerError> {
        let assets = self.assets;
        let download = Download {
            export: Export::Cpu,
            size: assets.size(),
        };
        self.tell(if self.store.is_cached(&assets).await {
            Step::Reading
        } else {
            Step::Fetching(download)
        });
        let step = |step| self.tell(step);
        self.store
            .fetch_speech(&assets, device::downloading(download, &step))
            .await?;
        Ok((self.store.path(&assets.model), self.store.path(&assets.vad)))
    }

    /// Fetches the files and starts a helper, which a taken port gets one more try at.
    async fn start(&self) -> Result<Running<L::Process>, RecognizerError> {
        let started = Instant::now();
        let result = self.bring_up().await;
        self.tell(Step::Reading);
        let fields = json!({"model": self.name, "engine": "whisper_helper"});
        self.metrics.metric(
            Event::SpeechLoad,
            if result.is_ok() { "ok" } else { "error" },
            Some(started.elapsed()),
            match &result {
                Ok(running) => running.device.tag(fields),
                Err(_) => fields,
            },
        );
        result
    }

    async fn bring_up(&self) -> Result<Running<L::Process>, RecognizerError> {
        let (model, vad) = self.fetch().await?;
        let executable = self
            .helpers
            .join(self.backend.folder())
            .join(format!("whisper-server{}", std::env::consts::EXE_SUFFIX));
        let client = reqwest::Client::builder().no_proxy().build().map_err(|_| {
            RecognizerError::said("Thalvi could not prepare to talk to its Whisper helper.")
        })?;
        let threads = helper_threads(std::thread::available_parallelism().map_or(1, usize::from));
        let mut tried = 0;
        loop {
            let launch = Launch {
                backend: self.backend,
                executable: executable.clone(),
                model: model.clone(),
                vad: vad.clone(),
                port: free_port()?,
                threads,
            };
            match self.try_start(&launch, &client).await {
                Err(error) if tried == 0 && error.port_taken() => tried += 1,
                started => {
                    return started.map_err(|error| RecognizerError::said(error.to_string()));
                }
            }
        }
    }

    async fn try_start(
        &self,
        launch: &Launch,
        client: &reqwest::Client,
    ) -> Result<Running<L::Process>, HelperError> {
        let process = self.launcher.launch(launch)?;
        let began = tokio::time::Instant::now();
        loop {
            if let Some(exit) = process.exit() {
                let output = process.output();
                let from = output.len().saturating_sub(SHOWN_LINES);
                return Err(HelperError::Exited {
                    backend: launch.backend,
                    exit,
                    tail: output[from..].to_vec(),
                });
            }
            if healthy(client, launch.port).await {
                break;
            }
            if began.elapsed() > READY_TIMEOUT {
                return Err(HelperError::NotReady);
            }
            tokio::time::sleep(POLL).await;
        }
        // The Vulkan helper compiles its pipelines on its first request, which would otherwise be the
        // user's; on the CPU the same request only costs a 30 s window's encoding.
        if launch.backend == WhisperBackend::Vulkan {
            let silence = wav(&vec![0; SAMPLE_RATE as usize]);
            let fields = [
                ("response_format", "json"),
                ("language", "auto"),
                ("vad", "false"),
            ];
            infer(client, launch.port, &fields, &silence, READY_TIMEOUT)
                .await
                .map_err(|failure| HelperError::WarmUp(describe(&failure)))?;
        }
        Ok(Running {
            device: device_of(launch.backend, &process.output())?,
            process,
            port: launch.port,
            client: client.clone(),
        })
    }
}

fn describe(failure: &Failure) -> String {
    match failure {
        Failure::Timeout => "it took too long".to_owned(),
        Failure::Unreachable => "it could not be reached".to_owned(),
        Failure::Status(code) => format!("it answered HTTP {code}"),
        Failure::BadReply => "its answer was unreadable".to_owned(),
    }
}

fn free_port() -> Result<u16, RecognizerError> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| RecognizerError::said(HelperError::Port(error).to_string()))
}

impl<L: Launcher> Preload for WhisperRecognizer<L> {
    fn name(&self) -> &str {
        &self.shared.name
    }

    fn steps(&self) -> watch::Receiver<Step> {
        self.shared.steps.subscribe()
    }

    fn device(&self) -> Option<Device> {
        match &*locked(&self.shared.slot) {
            Slot::Ready(running) if running.alive() => Some(running.device.clone()),
            Slot::Idle | Slot::Starting(_) | Slot::Ready(_) => None,
        }
    }

    fn load(&self) -> Pin<Box<dyn Future<Output = Result<(), RecognizerError>> + Send + '_>> {
        Box::pin(async { self.shared.running().await.map(|_| ()) })
    }
}

impl<L: Launcher> Recognizer for WhisperRecognizer<L> {
    type Session = WhisperSession<L>;

    async fn connect(&self) -> Result<WhisperSession<L>, RecognizerError> {
        let running = self.shared.running().await?;
        Ok(WhisperSession {
            shared: Arc::clone(&self.shared),
            running,
            frames: Vec::new(),
            finished_frames: None,
            finished_signal: Notify::new(),
            request: Request::Buffering,
            preview: None,
            previewed_frames: 0,
            shown: String::new(),
            language: None,
        })
    }

    fn loading(&self) -> Option<watch::Receiver<Step>> {
        Some(self.shared.steps.subscribe())
    }
}

/// A request to the helper that is under way.
struct Sent {
    reply: oneshot::Receiver<Result<Transcript, RecognizerError>>,
    task: JoinHandle<()>,
    at: Instant,
}

enum Request {
    Buffering,
    Sent(Sent),
    Answered { ok: bool },
}

/// Why audio is sent to the helper.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Provisional text while the user speaks.
    Preview,
    /// The text of the whole capture, once it has ended.
    Final,
}

impl Reading {
    fn timeout(self, frames: usize) -> Duration {
        match self {
            Self::Preview => PREVIEW_TIMEOUT,
            Self::Final => request_timeout(frames),
        }
    }
}

/// One capture, buffered until it ends and then transcribed in one request; on a GPU it is also read while it
/// lasts, for the provisional text the overlay shows.
pub struct WhisperSession<L: Launcher = ProcessLauncher> {
    shared: Arc<Shared<L>>,
    running: Arc<Running<L::Process>>,
    frames: Vec<Frame>,
    /// The frames buffered when the capture ended, once it has.
    finished_frames: Option<usize>,
    finished_signal: Notify,
    request: Request,
    /// The one preview in flight; none is started once the capture has ended.
    preview: Option<Sent>,
    /// The frames the last preview started with.
    previewed_frames: usize,
    /// The text of the last preview shown.
    shown: String,
    /// The language the latest non-empty preview detected, which the final pass reuses.
    language: Option<String>,
}

fn stopped() -> RecognizerError {
    RecognizerError::said("Speech transcription stopped unexpectedly. Nothing was delivered.")
}

impl<L: Launcher> Shared<L> {
    fn transcription_metric(
        &self,
        running: &Running<L::Process>,
        outcome: &str,
        at: Instant,
        reading: Reading,
        reused_language: bool,
        seconds: Option<f64>,
    ) {
        let mut fields = json!({"engine": "whisper_helper", "model": self.name});
        if let Some(seconds) = seconds {
            fields["audio_seconds"] = json!(seconds);
        }
        if reading == Reading::Preview {
            fields["preview"] = json!(true);
        }
        if reused_language {
            fields["preview_language"] = json!(true);
        }
        self.metrics.metric(
            Event::LocalStt,
            outcome,
            Some(at.elapsed()),
            running.device.tag(fields),
        );
    }
}

impl<L: Launcher> WhisperSession<L> {
    /// Transcribes `frames` in the background.
    fn send(&self, frames: Vec<Frame>, reading: Reading) -> Sent {
        let (sender, reply) = oneshot::channel();
        let running = Arc::clone(&self.running);
        let shared = Arc::clone(&self.shared);
        let prompt = self.shared.prompt.clone();
        let (format, lang) = match reading {
            Reading::Preview => ("verbose_json", "auto".to_owned()),
            Reading::Final => (
                "json",
                self.language.as_deref().unwrap_or("auto").to_owned(),
            ),
        };
        let reused_language = reading == Reading::Final && self.language.is_some();
        let at = Instant::now();
        let task = tokio::spawn(async move {
            let seconds = audio_seconds(frames.len());
            let result = if frames.is_empty() {
                Ok(Transcript {
                    text: String::new(),
                    language: None,
                })
            } else {
                let audio = wav(frames.as_flattened());
                let mut fields = vec![("response_format", format), ("language", lang.as_str())];
                if let Some(prompt) = &prompt {
                    // VAD keeps the prompt out of a capture with no speech, which whisper would echo.
                    fields.push(("vad", "true"));
                    fields.push(("prompt", prompt));
                }
                let timeout = reading.timeout(frames.len());
                infer(&running.client, running.port, &fields, &audio, timeout).await
            };
            shared.transcription_metric(
                &running,
                if result.is_ok() { "ok" } else { "error" },
                at,
                reading,
                reused_language,
                Some(seconds),
            );
            let _ = sender.send(result.map_err(|failure| failure.error()));
        });
        Sent { reply, task, at }
    }

    /// Starts a preview of the whole capture so far when none is running and enough new audio has arrived.
    fn start_preview(&mut self) {
        if !self.shared.backend.previews()
            || self.finished_frames.is_some()
            || self.preview.is_some()
            || audio_time(self.frames.len()) < PREVIEW_MIN_AUDIO
            || audio_time(self.frames.len() - self.previewed_frames) < PREVIEW_STEP
        {
            return;
        }
        self.previewed_frames = self.frames.len();
        self.preview = Some(self.send(self.frames.clone(), Reading::Preview));
    }

    /// Drops the preview in flight and tells whether the helper was still working on it.
    fn stop_preview(&mut self) -> bool {
        let Some(preview) = self.preview.take() else {
            return false;
        };
        if preview.task.is_finished() {
            return false;
        }
        preview.task.abort();
        self.shared.transcription_metric(
            &self.running,
            "cancelled",
            preview.at,
            Reading::Preview,
            false,
            Some(audio_seconds(self.previewed_frames)),
        );
        true
    }

    /// The text of the whole capture, requested once it has ended.
    async fn final_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        if matches!(self.request, Request::Buffering) {
            let frames = std::mem::take(&mut self.frames);
            self.request = Request::Sent(self.send(frames, Reading::Final));
        }
        let Request::Sent(Sent { reply, .. }) = &mut self.request else {
            return Err(stopped());
        };
        let answer = reply.await;
        let ok = matches!(answer, Ok(Ok(_)));
        self.request = Request::Answered { ok };
        match answer {
            Ok(Ok(transcript)) => Ok(RecognizerEvent::Final(transcript.text)),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(stopped()),
        }
    }
}

impl<L: Launcher> RecognizerSession for WhisperSession<L> {
    fn buffers_audio(&self) -> bool {
        true
    }

    fn final_wait(&self) -> Duration {
        request_timeout(self.finished_frames.unwrap_or(self.frames.len())) + FINAL_MARGIN
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Marks the final frame count, aborts any preview and wakes next_event without awaiting."
    )]
    async fn finish_audio(&mut self) -> Result<(), RecognizerError> {
        self.finished_frames = Some(self.frames.len());
        self.stop_preview();
        self.finished_signal.notify_one();
        Ok(())
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Buffers one frame and can spawn a preview; next_event awaits the reply."
    )]
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        self.frames.push(*frame);
        self.start_preview();
        Ok(())
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        loop {
            if self.finished_frames.is_some() {
                return self.final_event().await;
            }
            let Some(preview) = &mut self.preview else {
                self.finished_signal.notified().await;
                continue;
            };
            tokio::select! {
                () = self.finished_signal.notified() => {}
                answer = &mut preview.reply => {
                    self.preview = None;
                    let text = if let Ok(Ok(transcript)) = answer {
                        if !transcript.text.is_empty()
                            && let Some(language) = transcript.language
                        {
                            self.language = Some(language);
                        }
                        Some(transcript.text)
                    } else {
                        None
                    };
                    self.start_preview();
                    if let Some(text) = text
                        && !text.is_empty()
                        && text != self.shown
                    {
                        self.shown.clone_from(&text);
                        return Ok(RecognizerEvent::Partial(text));
                    }
                }
            }
        }
    }

    async fn cancel(self) {}

    async fn close(self) {}
}

impl<L: Launcher> Drop for WhisperSession<L> {
    fn drop(&mut self) {
        let mut busy = self.stop_preview();
        match &self.request {
            Request::Buffering | Request::Answered { ok: true } => {}
            Request::Answered { ok: false } => busy = true,
            Request::Sent(Sent { task, at, .. }) => {
                task.abort();
                self.shared.transcription_metric(
                    &self.running,
                    "cancelled",
                    *at,
                    Reading::Final,
                    self.language.is_some(),
                    None,
                );
                busy = true;
            }
        }
        if busy {
            self.shared.discard(&self.running);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    use super::*;
    use crate::frame::FRAME_SAMPLES;
    use crate::metrics::{RecordKinds, Recorder};
    use crate::models::{Asset, ModelKind};

    const WAIT: Duration = Duration::from_secs(5);

    const MODEL: &[u8] = b"model-bytes";
    const VAD: &[u8] = b"vad-bytes";

    const FAKE_FILES: WhisperAssets<'static> = WhisperAssets {
        model: Asset {
            kind: ModelKind::Speech,
            file_name: "fake-model.bin",
            url: "http://127.0.0.1:9/never",
            size: 11,
            sha256: "357e5d6fafa34d27360fec24b4326d3534905e33c6acdee60198fb078b7b79e5",
        },
        vad: Asset {
            kind: ModelKind::Speech,
            file_name: "fake-vad.bin",
            url: "http://127.0.0.1:9/never",
            size: 9,
            sha256: "49af96d41995405c952b35d88ebff042313536d73c5bc50c3f53ffca77374d67",
        },
    };

    /// A folder that is removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static COUNT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "thalvi-whisper-{tag}-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("models")).unwrap();
            Self(dir)
        }

        /// A scratch folder whose model store already holds the fake files.
        fn with_files(tag: &str) -> Self {
            let scratch = Self::new(tag);
            std::fs::write(scratch.0.join("models").join("fake-model.bin"), MODEL).unwrap();
            std::fs::write(scratch.0.join("models").join("fake-vad.bin"), VAD).unwrap();
            scratch
        }

        fn store(&self) -> ModelStore {
            ModelStore::in_dir(self.0.join("models"))
        }

        fn helpers(&self) -> PathBuf {
            self.0.join("helpers")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn within<T>(what: impl Future<Output = T>) -> T {
        tokio::time::timeout(WAIT, what)
            .await
            .expect("the awaited event happened in time")
    }

    #[test]
    #[ignore = "the child process the helper tests start; it needs THALVI_WHISPER_TEST_CHILD"]
    fn helper_process_fixture() {
        use std::io::{Read, Write};

        let address = std::env::var_os("THALVI_WHISPER_TEST_CHILD")
            .expect("the helper tests start this test with THALVI_WHISPER_TEST_CHILD set");
        let mut stream = std::net::TcpStream::connect(address.to_str().unwrap()).unwrap();
        stream.set_read_timeout(Some(WAIT * 4)).unwrap();
        for number in 0..KEPT_LINES + 2 {
            eprintln!("helper line {number}");
        }
        stream.write_all(&std::process::id().to_le_bytes()).unwrap();
        let _ = stream.read(&mut [0]);
        std::process::exit(7);
    }

    fn helper_command(address: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "whisper::tests::helper_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("THALVI_WHISPER_TEST_CHILD", address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        native::prepare(&mut command);
        command
    }

    async fn helper_child() -> (Child, tokio::net::TcpStream, u32) {
        let listener = within(tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let child = helper_command(&listener.local_addr().unwrap().to_string())
            .spawn()
            .unwrap();
        native::bind(&child).unwrap();
        let (mut stream, _) = within(listener.accept()).await.unwrap();
        let pid = within(stream.read_u32_le()).await.unwrap();
        assert_eq!(pid, child.id());
        (child, stream, pid)
    }

    fn helper_process(mut child: Child) -> HelperProcess {
        let output = Arc::default();
        let reader = read_lines(child.stderr.take().unwrap(), Arc::clone(&output)).unwrap();
        HelperProcess {
            child: Mutex::new(child),
            output,
            reader: Mutex::new(Some(reader)),
        }
    }

    #[tokio::test]
    async fn a_real_helper_reports_its_pid_exit_code_and_last_output() {
        let (child, mut stream, pid) = helper_child().await;
        let process = helper_process(child);
        assert_eq!(process.id(), pid);
        assert_eq!(process.exit(), None);
        within(stream.write_all(&[1])).await.unwrap();
        let exit = within(async {
            loop {
                if let Some(exit) = process.exit() {
                    break exit;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(exit, Exit(Some(7)));
        assert_eq!(
            process.output(),
            (2..KEPT_LINES + 2)
                .map(|number| format!("helper line {number}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_helpers_exit_waits_for_its_last_output_to_be_read() {
        let (child, mut stream, _) = helper_child().await;
        let output = Arc::new(Mutex::new(VecDeque::new()));
        let (release, read) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("thalvi-whisper-test-log".to_owned())
            .spawn({
                let output = Arc::clone(&output);
                move || {
                    read.recv().unwrap();
                    locked(&output).push_back("last message".to_owned());
                }
            })
            .unwrap();
        let process = Arc::new(HelperProcess {
            child: Mutex::new(child),
            output,
            reader: Mutex::new(Some(reader)),
        });
        within(stream.write_all(&[1])).await.unwrap();
        within(async {
            while locked(&process.child).try_wait().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let (checking, checked) = oneshot::channel();
        let mut exit = tokio::task::spawn_blocking({
            let process = Arc::clone(&process);
            move || {
                checking.send(()).unwrap();
                process.exit()
            }
        });
        within(checked).await.unwrap();
        let checked = tokio::time::timeout(Duration::from_millis(100), &mut exit).await;
        let waited = checked.is_err();
        release.send(()).unwrap();
        let ended = match checked {
            Ok(ended) => ended.unwrap(),
            Err(_) => within(exit).await.unwrap(),
        };
        assert!(
            waited,
            "exit must wait for the log reader before reporting the exit code"
        );
        assert_eq!(ended, Some(Exit(Some(7))));
        assert_eq!(process.output(), ["last message"]);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_helper_is_killed_when_the_thread_that_started_it_ends() {
        use std::os::unix::process::ExitStatusExt;

        let listener = within(tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let mut child = std::thread::spawn(move || helper_command(&address).spawn().unwrap())
            .join()
            .unwrap();
        let ended = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if ended.is_err() {
            child.kill().unwrap();
        }
        let status = ended.expect("the helper ended with the thread that started it");
        assert_eq!(status.signal(), Some(9), "SIGKILL, not {status}");
    }

    #[tokio::test]
    async fn killing_or_dropping_a_real_helper_ends_its_process() {
        for drop_owner in [false, true] {
            let (child, mut stream, _) = helper_child().await;
            let process = helper_process(child);
            if drop_owner {
                drop(process);
            } else {
                process.kill();
            }
            let ended = tokio::time::timeout(WAIT, stream.read(&mut [0])).await;
            drop(stream);
            assert!(
                matches!(ended, Ok(Ok(0)))
                    || matches!(&ended, Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionReset),
                "drop={drop_owner}: the helper closed its connection when it ended: {ended:?}"
            );
        }
    }

    #[test]
    fn dropping_a_running_helper_ends_a_process_that_has_no_destructor() {
        struct PassiveProcess(Arc<AtomicBool>);

        impl Process for PassiveProcess {
            fn output(&self) -> Vec<String> {
                Vec::new()
            }

            fn exit(&self) -> Option<Exit> {
                None
            }

            fn kill(&self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let killed = Arc::new(AtomicBool::new(false));
        let running = Running {
            process: PassiveProcess(Arc::clone(&killed)),
            port: 1234,
            device: Device::Cpu(Fallback::CpuChoice),
            client: reqwest::Client::new(),
        };
        drop(running);
        assert!(killed.load(Ordering::SeqCst));
    }

    /// What the fake helper's server received in one inference request.
    #[derive(Clone, Debug)]
    struct Seen {
        fields: Vec<(String, String)>,
        wav: Vec<u8>,
    }

    impl Seen {
        fn parse(body: &[u8]) -> Self {
            let text = String::from_utf8_lossy(body);
            let mut fields = Vec::new();
            for part in text.split(&format!("--{BOUNDARY}")).skip(1) {
                if let Some(name) = part
                    .split("name=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .filter(|name| *name != "file")
                    && let Some(value) = part.split("\r\n\r\n").nth(1)
                {
                    fields.push((name.to_owned(), value.trim_end_matches("\r\n").to_owned()));
                }
            }
            let marker = b"Content-Type: audio/wav\r\n\r\n";
            let start = body
                .windows(marker.len())
                .position(|window| window == marker)
                .unwrap()
                + marker.len();
            let end = body.len() - format!("\r\n--{BOUNDARY}--\r\n").len();
            Self {
                fields,
                wav: body[start..end].to_vec(),
            }
        }

        fn field(&self, name: &str) -> Option<&str> {
            self.fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
        }

        fn samples(&self) -> Vec<i16> {
            self.wav[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair))
                .collect()
        }
    }

    /// How the fake helper answers a capture's request.
    enum Reply {
        Text(&'static str),
        WithLanguage(&'static str, &'static str),
        Status(u16),
        Garbage,
        /// Answers once the test says so, unless its helper was killed: a dead process answers nothing.
        Held(&'static str),
    }

    struct Server {
        health_calls: AtomicUsize,
        unready: usize,
        health_release: Mutex<Option<Arc<Notify>>>,
        seen: Mutex<Vec<Seen>>,
        seen_count: watch::Sender<usize>,
        replies: Mutex<VecDeque<Reply>>,
        warm_up: Mutex<Option<Reply>>,
        release: Notify,
        killed: Arc<AtomicBool>,
    }

    async fn health(State(server): State<Arc<Server>>) -> StatusCode {
        let call = server.health_calls.fetch_add(1, Ordering::SeqCst);
        let release = locked(&server.health_release).take();
        if let Some(release) = release {
            release.notified().await;
        }
        if call < server.unready {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        }
    }

    async fn inference(State(server): State<Arc<Server>>, body: Bytes) -> (StatusCode, String) {
        let seen = Seen::parse(&body);
        let warm_up = seen.field("vad") == Some("false");
        locked(&server.seen).push(seen);
        server.seen_count.send_modify(|count| *count += 1);
        let reply = if warm_up {
            locked(&server.warm_up).take().unwrap_or(Reply::Text(""))
        } else {
            locked(&server.replies)
                .pop_front()
                .unwrap_or(Reply::Text(""))
        };
        let ok = |text: &str| (StatusCode::OK, json!({"text": text}).to_string());
        match reply {
            Reply::Text(text) => ok(text),
            Reply::WithLanguage(text, language) => (
                StatusCode::OK,
                json!({"text": text, "language": language}).to_string(),
            ),
            Reply::Status(code) => (StatusCode::from_u16(code).unwrap(), String::new()),
            Reply::Garbage => (StatusCode::OK, "not json".to_owned()),
            Reply::Held(text) => {
                server.release.notified().await;
                if server.killed.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                ok(text)
            }
        }
    }

    /// What one launch of the fake does.
    #[derive(Default)]
    struct Plan {
        /// Health checks answered 503 before the first 200.
        unready: usize,
        health_release: Option<Arc<Notify>>,
        output: Vec<&'static str>,
        /// The helper has ended with this code as soon as it starts.
        exits_with: Option<i32>,
        replies: Vec<Reply>,
        warm_up: Option<Reply>,
    }

    /// The test's view of one helper the fake started.
    struct Helper {
        launch: Launch,
        server: Arc<Server>,
        killed: Arc<AtomicBool>,
        exit: Arc<Mutex<Option<Exit>>>,
    }

    impl Helper {
        fn seen(&self) -> Vec<Seen> {
            locked(&self.server.seen).clone()
        }

        async fn seen_requests(&self, count: usize) {
            within(
                self.server
                    .seen_count
                    .subscribe()
                    .wait_for(|seen| *seen >= count),
            )
            .await
            .unwrap();
        }
    }

    #[derive(Clone)]
    struct Fake {
        plans: Arc<Mutex<VecDeque<Plan>>>,
        helpers: Arc<Mutex<Vec<Arc<Helper>>>>,
        launched: Arc<watch::Sender<usize>>,
    }

    struct FakeProcess {
        output: Vec<String>,
        exit: Arc<Mutex<Option<Exit>>>,
        server: Mutex<Option<tokio::task::JoinHandle<()>>>,
        killed: Arc<AtomicBool>,
    }

    impl Fake {
        fn new(plans: impl IntoIterator<Item = Plan>) -> Self {
            Self {
                plans: Arc::new(Mutex::new(plans.into_iter().collect())),
                helpers: Arc::default(),
                launched: Arc::new(watch::channel(0).0),
            }
        }

        fn helper(&self, index: usize) -> Arc<Helper> {
            Arc::clone(&locked(&self.helpers)[index])
        }

        fn launches(&self) -> usize {
            locked(&self.helpers).len()
        }

        async fn launched(&self, count: usize) {
            within(
                self.launched
                    .subscribe()
                    .wait_for(|launched| *launched >= count),
            )
            .await
            .unwrap();
        }
    }

    impl Launcher for Fake {
        type Process = FakeProcess;

        fn launch(&self, launch: &Launch) -> Result<FakeProcess, HelperError> {
            let mut plans = locked(&self.plans);
            let listener = match plans.front().and_then(|plan| plan.exits_with) {
                Some(_) => None,
                None => match std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, launch.port)) {
                    // Another test's socket took the free port first; the real helper exits so, and is retried.
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                        return Ok(FakeProcess::port_taken());
                    }
                    listener => Some(listener.unwrap()),
                },
            };
            let plan = plans.pop_front().unwrap_or_default();
            drop(plans);
            let killed = Arc::new(AtomicBool::new(false));
            let server = Arc::new(Server {
                health_calls: AtomicUsize::new(0),
                unready: plan.unready,
                health_release: Mutex::new(plan.health_release),
                seen: Mutex::default(),
                seen_count: watch::channel(0).0,
                replies: Mutex::new(plan.replies.into()),
                warm_up: Mutex::new(plan.warm_up),
                release: Notify::new(),
                killed: Arc::clone(&killed),
            });
            let exit = Arc::new(Mutex::new(plan.exits_with.map(|code| Exit(Some(code)))));
            let handle = listener.map(|listener| {
                listener.set_nonblocking(true).unwrap();
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let app = Router::new()
                    .route("/health", get(health))
                    .route("/inference", post(inference))
                    .with_state(Arc::clone(&server));
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                })
            });
            locked(&self.helpers).push(Arc::new(Helper {
                launch: launch.clone(),
                server,
                killed: Arc::clone(&killed),
                exit: Arc::clone(&exit),
            }));
            self.launched.send_modify(|launched| *launched += 1);
            Ok(FakeProcess {
                output: plan.output.into_iter().map(str::to_owned).collect(),
                exit,
                server: Mutex::new(handle),
                killed,
            })
        }
    }

    impl FakeProcess {
        fn port_taken() -> Self {
            Self {
                output: vec!["couldn't bind to server socket: hostname=127.0.0.1".to_owned()],
                exit: Arc::new(Mutex::new(Some(Exit(Some(1))))),
                server: Mutex::new(None),
                killed: Arc::default(),
            }
        }
    }

    impl Process for FakeProcess {
        fn output(&self) -> Vec<String> {
            self.output.clone()
        }

        fn exit(&self) -> Option<Exit> {
            *locked(&self.exit)
        }

        fn kill(&self) {
            self.killed.store(true, Ordering::SeqCst);
            if let Some(server) = locked(&self.server).take() {
                server.abort();
            }
            locked(&self.exit).get_or_insert(Exit(None));
        }
    }

    impl Drop for FakeProcess {
        fn drop(&mut self) {
            self.kill();
        }
    }

    fn recognizer(
        fake: &Fake,
        backend: WhisperBackend,
        scratch: &Scratch,
    ) -> WhisperRecognizer<Fake> {
        recognizer_with(
            fake,
            backend,
            scratch,
            FAKE_FILES,
            Recorder::disabled().metrics(),
            &no_words(),
        )
    }

    fn no_words() -> WordList {
        WordList::new(Vec::new()).unwrap()
    }

    /// A word list of `words`, in the order given.
    fn list(words: &[&str]) -> WordList {
        WordList::new(words.iter().map(|word| (*word).to_owned()).collect()).unwrap()
    }

    fn recognizer_with(
        fake: &Fake,
        backend: WhisperBackend,
        scratch: &Scratch,
        assets: WhisperAssets<'static>,
        metrics: Metrics,
        phrases: &WordList,
    ) -> WhisperRecognizer<Fake> {
        WhisperRecognizer::with_launcher(
            fake.clone(),
            assets,
            backend,
            &scratch.helpers(),
            scratch.store(),
            metrics,
            phrases,
        )
    }

    fn frame(value: i16) -> Frame {
        [value; FRAME_SAMPLES]
    }

    /// Sends `frames` to a new session, ends it and takes its one event.
    async fn dictate<L: Launcher>(
        recognizer: &WhisperRecognizer<L>,
        frames: &[Frame],
    ) -> Result<RecognizerEvent, RecognizerError> {
        let mut session = within(recognizer.connect()).await.unwrap();
        for frame in frames {
            session.send_frame(frame).await.unwrap();
        }
        session.finish_audio().await.unwrap();
        within(session.next_event()).await
    }

    const GPU_OUTPUT: [&str; 4] = [
        "ggml_vulkan: Found 2 Vulkan devices:",
        "ggml_vulkan: 0 = NVIDIA GeForce RTX 5070 Ti (NVIDIA) | uma: 0 | fp16: 1 | warp size: 32",
        "ggml_vulkan: 1 = AMD Ryzen 7 9800X3D 8-Core Processor (RADV RAPHAEL_MENDOCINO) (radv) | uma: 1",
        "whisper_backend_init_gpu: using Vulkan0 backend",
    ];

    #[test]
    fn the_helper_is_started_with_the_pinned_arguments_for_each_backend() {
        for (backend, last) in [
            (WhisperBackend::Cpu, vec!["-ng"]),
            (WhisperBackend::Vulkan, vec!["-dev", "0"]),
        ] {
            let launch = Launch {
                backend,
                executable: "whisper-server".into(),
                model: "/m/model.bin".into(),
                vad: "/m/vad.bin".into(),
                port: 4242,
                threads: 6,
            };
            let mut expected = vec![
                "-m",
                "/m/model.bin",
                "--vad",
                "-vm",
                "/m/vad.bin",
                "-vp",
                "400",
                "--host",
                "127.0.0.1",
                "--port",
                "4242",
                "-t",
                "6",
                "-bo",
                "1",
                "-nlp",
            ];
            expected.extend(last);
            assert_eq!(launch.arguments(), expected, "{backend:?}");
        }
    }

    #[test]
    fn the_helper_gets_half_the_logical_threads_between_one_and_eight() {
        for (logical, threads) in [(1, 1), (2, 1), (4, 2), (8, 4), (16, 8), (64, 8)] {
            assert_eq!(
                helper_threads(logical),
                threads,
                "{logical} logical threads"
            );
        }
    }

    #[test]
    fn a_request_gets_twenty_seconds_and_a_second_per_second_of_audio() {
        for (frames, seconds) in [(0, 20), (50, 24), (750, 80)] {
            assert_eq!(
                request_timeout(frames),
                Duration::from_secs(seconds),
                "{frames} frames"
            );
        }
    }

    #[test]
    fn a_helper_that_ended_before_it_was_ready_is_explained_in_plain_words() {
        let dll_not_found = i32::from_ne_bytes(STATUS_DLL_NOT_FOUND.to_ne_bytes());
        let tail = vec!["first".to_owned(), "second".to_owned()];
        for (case, backend, exit, tail, expected) in [
            (
                "a Vulkan helper without the loader",
                WhisperBackend::Vulkan,
                Exit(Some(dll_not_found)),
                tail.as_slice(),
                "vulkan-1.dll was not found; it comes with the graphics driver. Pick Whisper Turbo \u{b7} CPU under Settings \u{2192} Recognition.",
            ),
            (
                "the same code from the CPU helper is not about Vulkan",
                WhisperBackend::Cpu,
                Exit(Some(dll_not_found)),
                tail.as_slice(),
                "The Whisper helper stopped while starting (exit code -1073741515). Its last messages: first | second",
            ),
            (
                "an ordinary exit",
                WhisperBackend::Cpu,
                Exit(Some(1)),
                tail.as_slice(),
                "The Whisper helper stopped while starting (exit code 1). Its last messages: first | second",
            ),
            (
                "a helper that was killed and wrote nothing",
                WhisperBackend::Cpu,
                Exit(None),
                &[][..],
                "The Whisper helper stopped while starting. It wrote nothing.",
            ),
        ] {
            assert_eq!(exit_message(backend, exit, tail), expected, "{case}");
        }
    }

    #[test]
    fn the_device_is_the_one_the_helper_reports_and_a_gpu_is_never_claimed_without_one() {
        let lines = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| (*line).to_owned())
                .collect::<Vec<_>>()
        };
        let second = lines(&[
            "ggml_vulkan: 0 = First (a) | uma: 0",
            "ggml_vulkan: 1 = Second (b) | uma: 1",
            "whisper_backend_init_gpu: using Vulkan1 backend",
        ]);
        let unnamed = lines(&["whisper_backend_init_gpu: using Vulkan0 backend"]);
        let none = lines(&[
            "ggml_vulkan: 0 = Software (llvmpipe) | uma: 1",
            "whisper_backend_init_gpu: no GPU found",
        ]);
        let gpu = |name: &str| {
            Ok(Device::Gpu(Gpu {
                name: name.to_owned(),
                api: "Vulkan",
            }))
        };
        for (case, backend, output, expected) in [
            (
                "the CPU helper",
                WhisperBackend::Cpu,
                lines(&GPU_OUTPUT),
                Ok(Device::Cpu(Fallback::CpuChoice)),
            ),
            (
                "the first of two devices",
                WhisperBackend::Vulkan,
                lines(&GPU_OUTPUT),
                gpu("NVIDIA GeForce RTX 5070 Ti (NVIDIA)"),
            ),
            (
                "the device the helper says it uses",
                WhisperBackend::Vulkan,
                second,
                gpu("Second (b)"),
            ),
            (
                "a device the helper did not list",
                WhisperBackend::Vulkan,
                unnamed,
                gpu("Vulkan device 0"),
            ),
            ("no GPU", WhisperBackend::Vulkan, none, Err(())),
        ] {
            let found = device_of(backend, &output).map_err(|error| {
                assert!(matches!(error, HelperError::NoGpu), "{case}");
            });
            assert_eq!(found, expected, "{case}");
        }
    }

    #[test]
    fn a_whisper_name_is_a_model_the_settings_window_lists() {
        for backend in WhisperBackend::ALL {
            assert_eq!(WhisperBackend::named(backend.name()), Some(backend));
        }
        assert_eq!(WhisperBackend::named("whisper_turbo"), None);
    }

    #[tokio::test]
    async fn the_helper_is_ready_once_it_answers_health_and_a_vulkan_one_after_a_warm_up_without_voice_detection()
     {
        for (backend, output, warm_ups) in [
            (WhisperBackend::Cpu, Vec::new(), 0),
            (WhisperBackend::Vulkan, GPU_OUTPUT.to_vec(), 1),
        ] {
            let scratch = Scratch::with_files("start");
            let fake = Fake::new([Plan {
                unready: 3,
                output,
                ..Plan::default()
            }]);
            let recognizer = recognizer(&fake, backend, &scratch);
            assert_eq!(
                recognizer.device(),
                None,
                "nothing starts before it is asked for"
            );
            assert_eq!(fake.launches(), 0);

            within(recognizer.load()).await.unwrap();

            let helper = fake.helper(0);
            assert!(
                helper.server.health_calls.load(Ordering::SeqCst) >= 4,
                "health was waited for"
            );
            let seen = helper.seen();
            assert_eq!(
                seen.len(),
                warm_ups,
                "{backend:?}: warm-ups and nothing else"
            );
            for warm_up in &seen {
                assert_eq!(warm_up.field("vad"), Some("false"));
                assert!(warm_up.samples().iter().all(|sample| *sample == 0));
                assert_eq!(warm_up.samples().len(), 16_000, "one second");
            }
            assert_eq!(helper.launch.model, scratch.store().path(&FAKE_FILES.model));
            assert_eq!(helper.launch.vad, scratch.store().path(&FAKE_FILES.vad));
            assert_eq!(
                helper.launch.executable,
                scratch
                    .helpers()
                    .join(backend.folder())
                    .join(format!("whisper-server{}", std::env::consts::EXE_SUFFIX))
            );
        }
    }

    #[tokio::test]
    async fn a_failed_vulkan_warm_up_explains_why_the_helper_cannot_start() {
        for (reply, reason) in [
            (Reply::Status(503), "it answered HTTP 503"),
            (Reply::Garbage, "its answer was unreadable"),
        ] {
            let scratch = Scratch::with_files("warm-up-failed");
            let fake = Fake::new([Plan {
                warm_up: Some(reply),
                ..vulkan(Vec::new())
            }]);
            let recognizer = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
            let error = within(recognizer.load()).await.unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "The Whisper helper started but could not transcribe its first request: {reason}"
                )
            );
            assert_eq!(recognizer.device(), None);
            assert!(fake.helper(0).killed.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn a_helper_that_ends_before_it_is_ready_is_reported_with_its_last_messages_and_tried_again_next_time()
     {
        let scratch = Scratch::with_files("early-exit");
        let fake = Fake::new([Plan {
            exits_with: Some(3),
            output: vec!["loading", "ggml: out of memory"],
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);

        let error = within(recognizer.load()).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "The Whisper helper stopped while starting (exit code 3). Its last messages: loading | ggml: out of memory"
        );
        assert_eq!(recognizer.device(), None);

        within(recognizer.load()).await.unwrap();
        assert_eq!(fake.launches(), 2, "the next load starts another helper");
    }

    #[tokio::test]
    async fn a_port_another_program_took_is_tried_again_once() {
        let taken = || Plan {
            exits_with: Some(1),
            output: vec!["couldn't bind to server socket: hostname=127.0.0.1 port=4242"],
            ..Plan::default()
        };
        for (case, plans, loads, launches) in [
            ("taken once", vec![taken()], true, 2),
            ("taken twice", vec![taken(), taken()], false, 2),
        ] {
            let scratch = Scratch::with_files("port");
            let fake = Fake::new(plans);
            let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
            assert_eq!(within(recognizer.load()).await.is_ok(), loads, "{case}");
            assert_eq!(fake.launches(), launches, "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_helper_that_never_becomes_healthy_explains_its_readiness_timeout() {
        let scratch = Scratch::with_files("not-ready");
        let fake = Fake::new([Plan {
            unready: usize::MAX,
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let error = tokio::time::timeout(READY_TIMEOUT + HEALTH_TIMEOUT + WAIT, recognizer.load())
            .await
            .expect("the helper's readiness deadline ends it first")
            .unwrap_err();
        assert_eq!(error.to_string(), HelperError::NotReady.to_string());
        assert_eq!(recognizer.device(), None);
        assert_eq!(fake.launches(), 1);
        assert!(fake.helper(0).killed.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn readiness_at_the_deadline_still_gets_one_more_health_check() {
        let scratch = Scratch::with_files("ready-at-deadline");
        let release = Arc::new(Notify::new());
        let fake = Fake::new([Plan {
            unready: 1,
            health_release: Some(Arc::clone(&release)),
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let launch = Launch {
            backend: WhisperBackend::Cpu,
            executable: scratch
                .helpers()
                .join("cpu")
                .join(format!("whisper-server{}", std::env::consts::EXE_SUFFIX)),
            model: scratch.store().path(&FAKE_FILES.model),
            vad: scratch.store().path(&FAKE_FILES.vad),
            port: free_port().unwrap(),
            threads: 1,
        };
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let loading = recognizer.shared.try_start(&launch, &client);
        tokio::pin!(loading);
        let began = Instant::now();
        within(async {
            tokio::select! {
                _ = &mut loading => panic!("loading ended before the first health check answered"),
                () = async {
                    while fake.launches() == 0
                        || fake.helper(0).server.health_calls.load(Ordering::SeqCst) == 0
                    {
                        assert!(began.elapsed() < WAIT, "the first health check arrived within five wall-clock seconds");
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        })
        .await;
        tokio::time::advance(READY_TIMEOUT).await;
        release.notify_one();
        assert!(
            futures_util::poll!(&mut loading).is_pending(),
            "a health check at exactly the readiness deadline can still be retried"
        );
        tokio::time::resume();
        let running = within(loading).await.unwrap();
        assert_eq!(running.device, Device::Cpu(Fallback::CpuChoice));
        assert_eq!(fake.launches(), 1);
        assert_eq!(fake.helper(0).server.health_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_vulkan_helper_names_its_gpu_and_without_one_the_user_is_told_to_pick_the_cpu_entry()
     {
        let scratch = Scratch::with_files("vulkan");
        let fake = Fake::new([
            Plan {
                output: GPU_OUTPUT.to_vec(),
                ..Plan::default()
            },
            Plan {
                output: vec![
                    "ggml_vulkan: 0 = Software (llvmpipe) | uma: 1",
                    "whisper_backend_init_gpu: no GPU found",
                ],
                ..Plan::default()
            },
        ]);
        let with_gpu = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
        within(with_gpu.load()).await.unwrap();
        assert_eq!(
            with_gpu.device(),
            Some(Device::Gpu(Gpu {
                name: "NVIDIA GeForce RTX 5070 Ti (NVIDIA)".to_owned(),
                api: "Vulkan"
            }))
        );

        let without = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
        let error = within(without.load()).await.unwrap_err();
        assert!(
            error.to_string().contains("Pick Whisper Turbo \u{b7} CPU"),
            "{error}"
        );
        assert_eq!(without.device(), None);
        assert!(
            fake.helper(1).killed.load(Ordering::SeqCst),
            "a helper nobody can use does not stay"
        );
    }

    #[tokio::test]
    async fn a_helper_that_is_not_in_the_build_is_a_message_the_user_can_act_on() {
        const CHILD: &str = "THALVI_WHISPER_MISSING_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "whisper::tests::a_helper_that_is_not_in_the_build_is_a_message_the_user_can_act_on",
                ])
                .env(CHILD, "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            native::prepare(&mut command);
            let mut child = tokio::process::Command::from(command)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let ended = tokio::time::timeout(WAIT, child.wait()).await;
            if ended.is_err() {
                child.kill().await.unwrap();
            }
            assert!(
                ended
                    .expect("missing-helper startup ends within five seconds")
                    .unwrap()
                    .success(),
                "the missing-helper message assertion passed in the child"
            );
            return;
        }
        let scratch = Scratch::with_files("missing");
        let recognizer = WhisperRecognizer::with_launcher(
            ProcessLauncher,
            FAKE_FILES,
            WhisperBackend::Vulkan,
            &scratch.helpers(),
            scratch.store(),
            Recorder::disabled().metrics(),
            &no_words(),
        );
        let error = within(recognizer.load()).await.unwrap_err();
        let expected = scratch
            .helpers()
            .join("vulkan")
            .join(format!("whisper-server{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(
            error.to_string(),
            format!(
                "This Thalvi build has no Whisper helper (expected {}). Pick another model under Settings \u{2192} Recognition.",
                expected.display()
            )
        );
    }

    #[tokio::test]
    async fn the_model_is_downloaded_into_the_store_with_the_loading_step_showing_it() {
        let scratch = Scratch::new("download");
        let release = Arc::new(Notify::new());
        let app = Router::new()
            .route(
                "/model",
                get({
                    let release = Arc::clone(&release);
                    async move || {
                        release.notified().await;
                        MODEL.to_vec()
                    }
                }),
            )
            .route("/vad", get(async || VAD.to_vec()));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url =
            |path: &str| -> &'static str { Box::leak(format!("{base}{path}").into_boxed_str()) };
        let assets = WhisperAssets {
            model: Asset {
                url: url("/model"),
                ..FAKE_FILES.model
            },
            vad: Asset {
                url: url("/vad"),
                ..FAKE_FILES.vad
            },
        };
        let fake = Fake::new([]);
        let recognizer = recognizer_with(
            &fake,
            WhisperBackend::Cpu,
            &scratch,
            assets,
            Recorder::disabled().metrics(),
            &no_words(),
        );
        let mut steps = recognizer.steps();
        let loading_steps = recognizer.loading().expect("captures can follow the load");

        let loading = recognizer.load();
        tokio::pin!(loading);
        within(async {
            tokio::select! {
                _ = &mut loading => panic!("the load ended before the download was released"),
                step = steps.wait_for(|step| matches!(step, Step::Fetching(_) | Step::Downloading(..))) => {
                    let expected = Download {
                        export: Export::Cpu,
                        size: assets.size(),
                    };
                    assert!(matches!(
                        *step.unwrap(),
                        Step::Fetching(download) | Step::Downloading(download, _) if download == expected
                    ));
                }
            }
        })
        .await;
        assert_eq!(*loading_steps.borrow(), *steps.borrow());
        assert!(!scratch.store().is_cached(&assets).await);
        release.notify_one();
        within(&mut loading).await.unwrap();

        assert!(
            scratch.store().is_cached(&assets).await,
            "both files are in the store"
        );
        assert_eq!(*steps.borrow(), Step::Reading, "the download step is over");
        assert_eq!(*loading_steps.borrow(), Step::Reading);
        assert_eq!(fake.launches(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn a_capture_gets_a_second_past_its_request_timeout_for_the_final_even_after_the_upload_starts()
     {
        let scratch = Scratch::with_files("final-wait");
        for (frames, seconds) in [(0, 21), (50, 25), (750, 81)] {
            let fake = Fake::new([Plan {
                replies: vec![Reply::Text("ok")],
                ..Plan::default()
            }]);
            let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
            let mut session = within(recognizer.connect()).await.unwrap();
            for _ in 0..frames {
                session.send_frame(&frame(1)).await.unwrap();
            }
            session.finish_audio().await.unwrap();
            let wait = Duration::from_secs(seconds);
            assert_eq!(session.final_wait(), wait, "{frames} frames buffered");
            within(session.next_event()).await.unwrap();
            assert_eq!(session.final_wait(), wait, "{frames} frames uploaded");
        }
    }

    #[tokio::test]
    async fn a_capture_is_sent_as_one_16_khz_wav_with_automatic_language_and_no_prompt_and_comes_back_as_one_final()
     {
        let scratch = Scratch::with_files("capture");
        let fake = Fake::new([Plan {
            replies: vec![Reply::Text(" Priyanka   Vasquez.\n")],
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let mut session = within(recognizer.connect()).await.unwrap();
        assert!(session.buffers_audio());
        let frames = [frame(1), frame(2), frame(3)];
        for frame in &frames {
            session.send_frame(frame).await.unwrap();
        }
        session.finish_audio().await.unwrap();
        assert_eq!(
            within(session.next_event()).await.unwrap(),
            RecognizerEvent::Final("Priyanka Vasquez.".to_owned())
        );

        let seen = fake.helper(0).seen();
        assert_eq!(
            seen.len(),
            1,
            "the capture alone; the CPU helper is not warmed up"
        );
        let capture = &seen[0];
        assert_eq!(
            capture.fields,
            [
                ("response_format".to_owned(), "json".to_owned()),
                ("language".to_owned(), "auto".to_owned())
            ]
        );
        assert_eq!(&capture.wav[..4], b"RIFF");
        assert_eq!(
            u16::from_le_bytes([capture.wav[22], capture.wav[23]]),
            1,
            "mono"
        );
        assert_eq!(
            u32::from_le_bytes(capture.wav[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(
            u32::from_le_bytes(capture.wav[28..32].try_into().unwrap()),
            32_000,
            "bytes per second"
        );
        assert_eq!(
            u16::from_le_bytes(capture.wav[32..34].try_into().unwrap()),
            2,
            "bytes per sample frame"
        );
        assert_eq!(
            u16::from_le_bytes([capture.wav[34], capture.wav[35]]),
            16,
            "bits"
        );
        assert_eq!(capture.samples(), frames.as_flattened());
        assert_eq!(
            u32::from_le_bytes(capture.wav[4..8].try_into().unwrap()) as usize,
            capture.wav.len() - 8,
            "RIFF length"
        );
        assert_eq!(
            u32::from_le_bytes(capture.wav[40..44].try_into().unwrap()) as usize,
            capture.wav.len() - 44,
            "audio data length"
        );
    }

    #[tokio::test]
    async fn the_word_list_is_the_finals_and_a_previews_prompt_and_an_empty_list_sends_none() {
        for (case, words, prompt) in [
            ("an empty list", Vec::new(), None),
            ("a list", vec!["Cybrex2", "Thalvi"], Some("Cybrex2, Thalvi")),
        ] {
            let scratch = Scratch::with_files("prompt");
            let fake = Fake::new([vulkan(vec![Reply::Text("a"), Reply::Text("the final")])]);
            let recognizer = recognizer_with(
                &fake,
                WhisperBackend::Vulkan,
                &scratch,
                FAKE_FILES,
                Recorder::disabled().metrics(),
                &list(&words),
            );
            let mut session = within(recognizer.connect()).await.unwrap();
            send_all(&mut session, &numbered(13)).await;
            assert_eq!(
                within(session.next_event()).await.unwrap(),
                partial("a"),
                "{case}"
            );
            session.finish_audio().await.unwrap();
            assert_eq!(
                within(session.next_event()).await.unwrap(),
                the_final("the final"),
                "{case}"
            );

            let seen = fake.helper(0).seen();
            assert_eq!(
                seen.len(),
                3,
                "{case}: the warm-up, the preview and the final"
            );
            assert_eq!(seen[0].field("vad"), Some("false"), "{case}");
            assert_eq!(seen[0].field("prompt"), None, "{case}: the warm-up");
            for (capture, format) in [(&seen[1], "verbose_json"), (&seen[2], "json")] {
                let mut expected = vec![
                    ("response_format".to_owned(), format.to_owned()),
                    ("language".to_owned(), "auto".to_owned()),
                ];
                if let Some(prompt) = prompt {
                    expected.push(("vad".to_owned(), "true".to_owned()));
                    expected.push(("prompt".to_owned(), prompt.to_owned()));
                }
                assert_eq!(capture.fields, expected, "{case} ({format})");
            }
        }
    }

    #[tokio::test]
    async fn silence_is_a_normal_empty_final() {
        let scratch = Scratch::with_files("silence");
        let fake = Fake::new([Plan {
            replies: vec![Reply::Text(""), Reply::Text(" \n")],
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        for _ in 0..2 {
            assert_eq!(
                dictate(&recognizer, &[frame(0)]).await.unwrap(),
                RecognizerEvent::Final(String::new())
            );
        }
        assert_eq!(fake.launches(), 1, "silence does not cost the helper");
    }

    #[tokio::test]
    async fn cancelling_a_request_in_flight_kills_the_helper_and_the_next_capture_gets_a_fresh_one_with_only_its_own_text()
     {
        let scratch = Scratch::with_files("cancel");
        let fake = Fake::new([
            Plan {
                replies: vec![Reply::Held("stale text")],
                ..Plan::default()
            },
            Plan {
                replies: vec![Reply::Text("second text")],
                ..Plan::default()
            },
        ]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let mut first = within(recognizer.connect()).await.unwrap();
        first.send_frame(&frame(7)).await.unwrap();
        first.finish_audio().await.unwrap();
        let stuck = fake.helper(0);
        tokio::select! {
            event = first.next_event() => panic!("the held request answered: {event:?}"),
            () = stuck.seen_requests(1) => {}
        }

        first.cancel().await;
        assert!(
            stuck.killed.load(Ordering::SeqCst),
            "the helper that was busy is gone"
        );
        stuck.server.release.notify_waiters();

        let event = dictate(&recognizer, &[frame(9), frame(9)]).await.unwrap();
        assert_eq!(event, RecognizerEvent::Final("second text".to_owned()));
        assert_eq!(fake.launches(), 2);
        let fresh = fake.helper(1).seen();
        assert_eq!(fresh.len(), 1, "the new capture only");
        assert_eq!(fresh[0].samples().len(), 2 * FRAME_SAMPLES);
        assert_eq!(
            stuck.seen().len(),
            1,
            "nothing reached the old helper afterwards"
        );
    }

    #[tokio::test]
    async fn the_restart_after_a_cancel_shows_as_loading_until_the_new_helper_is_ready() {
        let scratch = Scratch::with_files("restart");
        let fake = Fake::new([
            Plan {
                replies: vec![Reply::Held("stale")],
                ..Plan::default()
            },
            Plan {
                unready: 2,
                ..Plan::default()
            },
        ]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let mut session = within(recognizer.connect()).await.unwrap();
        session.send_frame(&frame(1)).await.unwrap();
        session.finish_audio().await.unwrap();
        let stuck = fake.helper(0);
        tokio::select! {
            event = session.next_event() => panic!("the held request answered: {event:?}"),
            () = stuck.seen_requests(1) => {}
        }
        session.close().await;
        fake.launched(2).await;
        assert_eq!(
            recognizer.device(),
            None,
            "the new helper is not ready yet or only just so"
        );
        within(recognizer.load()).await.unwrap();
        assert_eq!(recognizer.device(), Some(Device::Cpu(Fallback::CpuChoice)));
        assert_eq!(
            fake.launches(),
            2,
            "the connect waited for the restart instead of starting a third"
        );
    }

    #[tokio::test]
    async fn a_helper_that_exited_is_started_again_for_the_next_capture_without_reading_the_model_files()
     {
        for exit in [Exit(Some(139)), Exit(None)] {
            let scratch = Scratch::with_files("died");
            let fake = Fake::new([
                Plan::default(),
                Plan {
                    replies: vec![Reply::Text("again")],
                    ..Plan::default()
                },
            ]);
            let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
            within(recognizer.load()).await.unwrap();
            std::fs::remove_dir_all(scratch.0.join("models")).unwrap();
            *locked(&fake.helper(0).exit) = Some(exit);
            assert_eq!(
                recognizer.device(),
                None,
                "a helper that ended is not a loaded model"
            );

            let event = dictate(&recognizer, &[frame(1)]).await.unwrap();
            assert_eq!(event, RecognizerEvent::Final("again".to_owned()));
            assert_eq!(fake.launches(), 2);
        }
    }

    #[tokio::test]
    async fn a_failed_request_is_an_error_saying_nothing_was_delivered_and_the_helper_is_replaced()
    {
        for (case, reply, failure) in [
            (
                "an error status",
                Reply::Status(500),
                TranscriptionFailure::HttpStatus,
            ),
            (
                "an unreadable answer",
                Reply::Garbage,
                TranscriptionFailure::BadReply,
            ),
        ] {
            let scratch = Scratch::with_files("failed");
            let fake = Fake::new([Plan {
                replies: vec![reply],
                ..Plan::default()
            }]);
            let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
            let error = dictate(&recognizer, &[frame(1)]).await.unwrap_err();
            let RecognizerError::Transcription {
                failure: found,
                message,
            } = error
            else {
                panic!("{case}: {error:?}");
            };
            assert_eq!(found, failure, "{case}");
            assert!(
                message.contains("Nothing was delivered."),
                "{case}: {message}"
            );
            fake.launched(2).await;
            assert!(fake.helper(0).killed.load(Ordering::SeqCst), "{case}");
        }
    }

    #[tokio::test]
    async fn a_request_that_never_answers_ends_as_a_timeout() {
        let scratch = Scratch::with_files("timeout");
        let fake = Fake::new([Plan {
            replies: vec![Reply::Held("never")],
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let mut session = within(recognizer.connect()).await.unwrap();
        session.send_frame(&frame(1)).await.unwrap();
        session.finish_audio().await.unwrap();
        let helper = fake.helper(0);
        tokio::select! {
            event = session.next_event() => panic!("the held request answered: {event:?}"),
            () = helper.seen_requests(1) => {}
        }
        tokio::time::pause();
        tokio::time::advance(session.final_wait()).await;
        tokio::time::resume();
        let error = within(session.next_event()).await.unwrap_err();
        assert!(
            matches!(&error, RecognizerError::Transcription { failure: TranscriptionFailure::Timeout, message }
                if message.contains("took too long") && message.contains("Nothing was delivered.")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn dropping_the_recognizer_ends_its_helper() {
        let scratch = Scratch::with_files("drop");
        let fake = Fake::new([]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        within(recognizer.load()).await.unwrap();
        let helper = fake.helper(0);
        assert!(!helper.killed.load(Ordering::SeqCst));
        drop(recognizer);
        assert!(helper.killed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_start_under_way_ends_with_its_recognizer_and_takes_its_helper_along() {
        let scratch = Scratch::with_files("closed-while-starting");
        let fake = Fake::new([Plan {
            unready: usize::MAX,
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let loading = tokio::spawn({
            let shared = Arc::clone(&recognizer.shared);
            async move { shared.running().await.map(|_| ()) }
        });
        fake.launched(1).await;
        let helper = fake.helper(0);

        drop(recognizer);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !helper.killed.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the helper was ended with the recognizer, not left to give up on its own");
        assert!(within(loading).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn the_metrics_record_the_start_with_its_device_and_each_transcription_with_its_audio() {
        let scratch = Scratch::with_files("metrics");
        let recorder = Recorder::in_dir(
            RecordKinds {
                metrics: true,
                text: false,
                audio: false,
            },
            scratch.0.join("recordings"),
        )
        .unwrap();
        let fake = Fake::new([Plan {
            replies: vec![Reply::Text("hi")],
            ..Plan::default()
        }]);
        let recognizer = recognizer_with(
            &fake,
            WhisperBackend::Cpu,
            &scratch,
            FAKE_FILES,
            recorder.metrics(),
            &no_words(),
        );
        dictate(&recognizer, &vec![frame(1); 25]).await.unwrap();
        recorder.finish().await;

        let written =
            std::fs::read_to_string(scratch.0.join("recordings").join("metrics.jsonl")).unwrap();
        let lines: Vec<Value> = written
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let event = |name: &str| lines.iter().find(|line| line["event"] == name).unwrap();
        let load = event("desktop_speech_load");
        assert_eq!(load["outcome"], "ok");
        assert_eq!(load["model"], "whisper_turbo_cpu");
        assert_eq!(load["device"], "cpu");
        assert!(load["duration_ms"].is_number());
        let transcription = event("desktop_local_stt");
        assert_eq!(transcription["outcome"], "ok");
        assert_eq!(transcription["engine"], "whisper_helper");
        assert_eq!(transcription["audio_seconds"], 2.0);
        assert!(transcription["duration_ms"].is_number());
    }

    fn vulkan(replies: Vec<Reply>) -> Plan {
        Plan {
            output: GPU_OUTPUT.to_vec(),
            replies,
            ..Plan::default()
        }
    }

    /// `count` frames that differ from each other, so that what a request carried says which they were.
    fn numbered(count: usize) -> Vec<Frame> {
        (1..=count)
            .map(|number| frame(i16::try_from(number).unwrap()))
            .collect()
    }

    async fn send_all<L: Launcher>(session: &mut WhisperSession<L>, frames: &[Frame]) {
        for frame in frames {
            session.send_frame(frame).await.unwrap();
        }
    }

    fn partial(text: &str) -> RecognizerEvent {
        RecognizerEvent::Partial(text.to_owned())
    }

    fn the_final(text: &str) -> RecognizerEvent {
        RecognizerEvent::Final(text.to_owned())
    }

    #[test]
    fn only_the_vulkan_entry_says_it_shows_text_while_you_speak() {
        for (backend, shows, says) in [
            (
                WhisperBackend::Cpu,
                false,
                "Text appears only after you stop speaking",
            ),
            (WhisperBackend::Vulkan, true, "Text appears while you speak"),
        ] {
            let description = backend.description();
            assert_eq!(backend.previews(), shows, "{backend:?}");
            assert!(description.contains(says), "{backend:?}: {description}");
            assert_eq!(description.contains("extra GPU work"), shows, "{backend:?}");
        }
    }

    #[tokio::test]
    async fn a_vulkan_capture_previews_everything_said_so_far_once_a_second_and_every_800_ms_and_only_its_final_ends_it()
     {
        let scratch = Scratch::with_files("previews");
        let fake = Fake::new([vulkan(vec![
            Reply::Text("a"),
            Reply::Text(" a "),
            Reply::Text("a b"),
            Reply::Text(""),
            Reply::Text("a b c"),
            Reply::Text("the whole capture"),
        ])]);
        let recognizer = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
        let frames = numbered(53);
        let mut session = within(recognizer.connect()).await.unwrap();
        let mut events = Vec::new();

        send_all(&mut session, &frames[..13]).await;
        send_all(&mut session, &frames[13..23]).await;
        events.push(within(session.next_event()).await.unwrap());
        send_all(&mut session, &frames[23..33]).await;
        events.push(within(session.next_event()).await.unwrap());
        send_all(&mut session, &frames[33..53]).await;
        events.push(within(session.next_event()).await.unwrap());
        session.finish_audio().await.unwrap();
        events.push(within(session.next_event()).await.unwrap());

        assert_eq!(
            events,
            [
                partial("a"),
                partial("a b"),
                partial("a b c"),
                the_final("the whole capture")
            ],
            "a repeated text and an empty one are not shown again"
        );
        let seen = fake.helper(0).seen();
        let capture: Vec<&Seen> = seen.iter().skip(1).collect();
        assert_eq!(
            capture
                .iter()
                .map(|seen| seen.samples().len())
                .collect::<Vec<_>>(),
            [13, 23, 33, 43, 53, 53].map(|count| count * FRAME_SAMPLES),
            "the first preview waits for a second of audio, then one starts per 800 ms of new audio, one at a time, and the final comes last"
        );
        for (index, seen) in capture.iter().enumerate() {
            let format = if index + 1 == capture.len() {
                "json"
            } else {
                "verbose_json"
            };
            assert_eq!(
                seen.fields,
                [
                    ("response_format".to_owned(), format.to_owned()),
                    ("language".to_owned(), "auto".to_owned())
                ]
            );
            assert_eq!(
                seen.samples(),
                frames.as_flattened()[..seen.samples().len()],
                "each request is the capture from its start"
            );
        }
    }

    #[tokio::test]
    async fn every_preview_detects_the_language_and_the_final_reuses_the_latest_one_of_its_capture()
    {
        let scratch = Scratch::with_files("language-latest");
        let recorder = Recorder::in_dir(
            RecordKinds {
                metrics: true,
                text: false,
                audio: false,
            },
            scratch.0.join("recordings"),
        )
        .unwrap();
        let fake = Fake::new([vulkan(vec![
            Reply::WithLanguage("Hello", "english"),
            Reply::WithLanguage("Hallo", "german"),
            Reply::WithLanguage("", "french"),
            Reply::WithLanguage("hm", "auto"),
            Reply::Text("Hallo Welt."),
            Reply::WithLanguage("", "french"),
            Reply::Text("Zwei."),
            Reply::WithLanguage("Hi", "english"),
            Reply::Text("Hi."),
        ])]);
        let recognizer = recognizer_with(
            &fake,
            WhisperBackend::Vulkan,
            &scratch,
            FAKE_FILES,
            recorder.metrics(),
            &no_words(),
        );
        let frames = numbered(43);

        let mut first = within(recognizer.connect()).await.unwrap();
        send_all(&mut first, &frames[..13]).await;
        send_all(&mut first, &frames[13..23]).await;
        assert_eq!(within(first.next_event()).await.unwrap(), partial("Hello"));
        send_all(&mut first, &frames[23..33]).await;
        assert_eq!(within(first.next_event()).await.unwrap(), partial("Hallo"));
        send_all(&mut first, &frames[33..43]).await;
        assert_eq!(within(first.next_event()).await.unwrap(), partial("hm"));
        first.finish_audio().await.unwrap();
        assert_eq!(
            within(first.next_event()).await.unwrap(),
            the_final("Hallo Welt.")
        );

        let mut second = within(recognizer.connect()).await.unwrap();
        send_all(&mut second, &frames[..13]).await;
        fake.helper(0).seen_requests(7).await;
        second.finish_audio().await.unwrap();
        assert_eq!(
            within(second.next_event()).await.unwrap(),
            the_final("Zwei.")
        );

        let mut third = within(recognizer.connect()).await.unwrap();
        send_all(&mut third, &frames[..13]).await;
        assert_eq!(within(third.next_event()).await.unwrap(), partial("Hi"));
        third.finish_audio().await.unwrap();
        assert_eq!(within(third.next_event()).await.unwrap(), the_final("Hi."));
        recorder.finish().await;

        let seen = fake.helper(0).seen();
        let pairs: Vec<(Option<&str>, Option<&str>)> = seen[1..]
            .iter()
            .map(|request| (request.field("response_format"), request.field("language")))
            .collect();
        assert_eq!(
            pairs,
            [
                (Some("verbose_json"), Some("auto")),
                (Some("verbose_json"), Some("auto")),
                (Some("verbose_json"), Some("auto")),
                (Some("verbose_json"), Some("auto")),
                (Some("json"), Some("german")),
                (Some("verbose_json"), Some("auto")),
                (Some("json"), Some("auto")),
                (Some("verbose_json"), Some("auto")),
                (Some("json"), Some("english")),
            ],
            "previews always detect; a later language replaces an earlier one, an empty or auto answer keeps it, and the next capture starts with none"
        );

        let written =
            std::fs::read_to_string(scratch.0.join("recordings").join("metrics.jsonl")).unwrap();
        let reused: Vec<bool> = written
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|line| line["event"] == "desktop_local_stt" && line["preview"].is_null())
            .map(|line| line["preview_language"].as_bool().unwrap_or(false))
            .collect();
        assert_eq!(
            reused,
            [true, false, true],
            "a final says when it reused a preview's language"
        );
    }

    #[tokio::test]
    async fn a_cpu_capture_is_never_previewed() {
        let scratch = Scratch::with_files("cpu-no-previews");
        let fake = Fake::new([Plan {
            replies: vec![Reply::Text("only the final")],
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let frames = numbered(60);
        let mut abandoned = within(recognizer.connect()).await.unwrap();
        send_all(&mut abandoned, &frames).await;
        abandoned.cancel().await;
        assert!(
            !fake.helper(0).killed.load(Ordering::SeqCst),
            "no request was in flight, so the helper is kept"
        );

        let mut session = within(recognizer.connect()).await.unwrap();
        send_all(&mut session, &frames).await;
        session.finish_audio().await.unwrap();
        assert_eq!(
            within(session.next_event()).await.unwrap(),
            the_final("only the final")
        );
        let seen = fake.helper(0).seen();
        assert_eq!(seen.len(), 1, "the final alone");
        assert_eq!(seen[0].samples(), frames.as_flattened());
        assert_eq!(fake.launches(), 1);
    }

    #[tokio::test]
    async fn a_preview_that_fails_shows_nothing_keeps_the_helper_and_the_next_preview_and_the_final_go_on()
     {
        for (case, failing) in [
            ("an error status", Reply::Status(500)),
            ("an unreadable answer", Reply::Garbage),
        ] {
            let scratch = Scratch::with_files("preview-fails");
            let fake = Fake::new([vulkan(vec![
                failing,
                Reply::Text("recovered"),
                Reply::Text("the final"),
            ])]);
            let recognizer = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
            let frames = numbered(23);
            let mut session = within(recognizer.connect()).await.unwrap();
            send_all(&mut session, &frames).await;
            assert_eq!(
                within(session.next_event()).await.unwrap(),
                partial("recovered"),
                "{case}"
            );
            session.finish_audio().await.unwrap();
            assert_eq!(
                within(session.next_event()).await.unwrap(),
                the_final("the final"),
                "{case}"
            );
            assert_eq!(fake.launches(), 1, "{case}: the helper was kept");
            assert!(!fake.helper(0).killed.load(Ordering::SeqCst), "{case}");
        }
    }

    #[tokio::test]
    async fn a_preview_that_never_answers_is_dropped_after_five_seconds_and_the_next_one_goes_on() {
        let scratch = Scratch::with_files("preview-timeout");
        let fake = Fake::new([vulkan(vec![
            Reply::Held("never"),
            Reply::Text("recovered"),
            Reply::Text("the final"),
        ])]);
        let recognizer = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
        let frames = numbered(23);
        let mut session = within(recognizer.connect()).await.unwrap();
        send_all(&mut session, &frames).await;
        fake.helper(0).seen_requests(2).await;
        let began = tokio::time::Instant::now();
        tokio::time::pause();
        tokio::time::advance(PREVIEW_TIMEOUT).await;
        tokio::time::resume();
        assert_eq!(
            within(session.next_event()).await.unwrap(),
            partial("recovered")
        );
        assert!(
            (PREVIEW_TIMEOUT..BASE_TIMEOUT).contains(&began.elapsed()),
            "{:?}",
            began.elapsed()
        );
        session.finish_audio().await.unwrap();
        assert_eq!(
            within(session.next_event()).await.unwrap(),
            the_final("the final")
        );
        assert_eq!(fake.launches(), 1, "the helper was kept");
    }

    #[tokio::test]
    async fn the_end_of_the_capture_aborts_a_preview_in_flight_and_nothing_it_says_or_starts_later_is_shown()
     {
        let scratch = Scratch::with_files("finish-aborts");
        let fake = Fake::new([vulkan(vec![
            Reply::Held("a late preview"),
            Reply::Text("the final"),
            Reply::Held("a preview that must not start"),
        ])]);
        let recognizer = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
        let frames = numbered(18);
        let mut session = within(recognizer.connect()).await.unwrap();
        send_all(&mut session, &frames[..13]).await;
        let helper = fake.helper(0);
        helper.seen_requests(2).await;
        send_all(&mut session, &frames[13..]).await;
        session.finish_audio().await.unwrap();
        tokio::select! {
            biased;
            event = session.next_event() => panic!("the final answered at once: {event:?}"),
            () = std::future::ready(()) => {}
        }
        send_all(&mut session, &numbered(30)).await;

        assert_eq!(
            within(session.next_event()).await.unwrap(),
            the_final("the final")
        );
        helper.server.release.notify_waiters();
        assert!(
            within(session.next_event()).await.is_err(),
            "no event follows the final, and a late preview is not one"
        );
        let seen = helper.seen();
        assert_eq!(
            seen.len(),
            3,
            "the warm-up, the preview and the final: frames after the end start no preview"
        );
        assert_eq!(
            seen[2].samples(),
            frames.as_flattened(),
            "the final has all the frames"
        );
        drop(session);
        assert!(
            !helper.killed.load(Ordering::SeqCst),
            "the preview was ended with the capture, so nothing is left to replace the helper for"
        );
    }

    #[tokio::test]
    async fn abandoning_a_capture_during_a_preview_replaces_the_helper_and_the_next_capture_gets_only_its_own_text()
     {
        for how in ["cancel", "close", "drop"] {
            let scratch = Scratch::with_files("preview-abandoned");
            let fake = Fake::new([
                vulkan(vec![Reply::Held("stale preview")]),
                vulkan(vec![Reply::Text("second text")]),
            ]);
            let recognizer = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
            let mut first = within(recognizer.connect()).await.unwrap();
            send_all(&mut first, &numbered(13)).await;
            let stuck = fake.helper(0);
            stuck.seen_requests(2).await;

            match how {
                "cancel" => first.cancel().await,
                "close" => first.close().await,
                _ => drop(first),
            }
            assert!(
                stuck.killed.load(Ordering::SeqCst),
                "{how}: the busy helper is gone"
            );
            stuck.server.release.notify_waiters();

            let event = dictate(&recognizer, &[frame(9), frame(9)]).await.unwrap();
            assert_eq!(event, the_final("second text"), "{how}");
            assert_eq!(fake.launches(), 2, "{how}");
            let fresh = fake.helper(1).seen();
            assert_eq!(
                fresh.len(),
                2,
                "{how}: the warm-up and the new capture only"
            );
            assert_eq!(fresh[1].samples().len(), 2 * FRAME_SAMPLES, "{how}");
        }
    }

    #[tokio::test]
    async fn abandoning_a_capture_whose_preview_has_answered_keeps_the_helper() {
        let scratch = Scratch::with_files("preview-answered");
        let fake = Fake::new([vulkan(vec![Reply::Text("one")])]);
        let recognizer = recognizer(&fake, WhisperBackend::Vulkan, &scratch);
        let mut session = within(recognizer.connect()).await.unwrap();
        send_all(&mut session, &numbered(13)).await;
        assert_eq!(within(session.next_event()).await.unwrap(), partial("one"));
        session.cancel().await;
        assert!(!fake.helper(0).killed.load(Ordering::SeqCst));
        assert_eq!(fake.launches(), 1);
    }

    #[tokio::test]
    async fn each_preview_is_recorded_as_a_local_transcription_marked_as_one_and_the_final_is_not()
    {
        let scratch = Scratch::with_files("preview-metrics");
        let recorder = Recorder::in_dir(
            RecordKinds {
                metrics: true,
                text: false,
                audio: false,
            },
            scratch.0.join("recordings"),
        )
        .unwrap();
        let fake = Fake::new([vulkan(vec![
            Reply::Text("one"),
            Reply::Held("late"),
            Reply::Text("done"),
        ])]);
        let recognizer = recognizer_with(
            &fake,
            WhisperBackend::Vulkan,
            &scratch,
            FAKE_FILES,
            recorder.metrics(),
            &no_words(),
        );
        let frames = numbered(23);
        let mut session = within(recognizer.connect()).await.unwrap();
        send_all(&mut session, &frames).await;
        assert_eq!(within(session.next_event()).await.unwrap(), partial("one"));
        fake.helper(0).seen_requests(3).await;
        session.finish_audio().await.unwrap();
        assert_eq!(
            within(session.next_event()).await.unwrap(),
            the_final("done")
        );
        recorder.finish().await;

        let written =
            std::fs::read_to_string(scratch.0.join("recordings").join("metrics.jsonl")).unwrap();
        let lines: Vec<Value> = written
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|line: &Value| line["event"] == "desktop_local_stt")
            .collect();
        let summary = |preview: bool| -> Vec<(String, f64)> {
            lines
                .iter()
                .filter(|line| line["preview"].as_bool().unwrap_or(false) == preview)
                .map(|line| {
                    assert!(line["duration_ms"].is_number());
                    (
                        line["outcome"].as_str().unwrap().to_owned(),
                        line["audio_seconds"].as_f64().unwrap(),
                    )
                })
                .collect()
        };
        assert_eq!(
            summary(true),
            [("ok".to_owned(), 1.04), ("cancelled".to_owned(), 1.84)]
        );
        assert_eq!(summary(false), [("ok".to_owned(), 1.84)]);
    }

    /// Tests that run the packaged helper and the real model, which CI does not have: each is ignored, and
    /// fails when run without `THALVI_WHISPER_HELPERS` (the folder with `cpu` and `vulkan`),
    /// `THALVI_WHISPER_MODELS` (holding `ggml-large-v3-turbo-q5_0.bin` and `ggml-silero-v6.2.0.bin`) and
    /// `THALVI_WHISPER_FIXTURES` (16 kHz mono WAV files) naming them. Run them with
    /// `cargo test -p thalvi-desktop --lib whisper::tests::real -- --ignored`.
    #[cfg(target_os = "linux")]
    mod real {
        use super::*;

        async fn within<T>(what: impl Future<Output = T>) -> T {
            tokio::time::timeout(READY_TIMEOUT * 2 + WAIT, what)
                .await
                .expect("the real helper answered within its loading and warm-up deadlines")
        }

        async fn dictate(
            recognizer: &WhisperRecognizer,
            frames: &[Frame],
        ) -> Result<RecognizerEvent, RecognizerError> {
            let mut session = within(recognizer.connect()).await.unwrap();
            send_all(&mut session, frames).await;
            session.finish_audio().await.unwrap();
            tokio::time::timeout(session.final_wait() + WAIT, session.next_event())
                .await
                .expect("the real helper answered within the capture's deadline")
        }

        struct Files {
            helpers: PathBuf,
            models: PathBuf,
            fixtures: PathBuf,
        }

        impl Files {
            fn from_env() -> Self {
                let var = |name: &str| {
                    PathBuf::from(
                        std::env::var_os(name)
                            .unwrap_or_else(|| panic!("set {name} to run this test")),
                    )
                };
                Self {
                    helpers: var("THALVI_WHISPER_HELPERS"),
                    models: var("THALVI_WHISPER_MODELS"),
                    fixtures: var("THALVI_WHISPER_FIXTURES"),
                }
            }

            fn store(&self, scratch: &Scratch) -> ModelStore {
                for asset in [WHISPER_TURBO.model, WHISPER_TURBO.vad] {
                    std::os::unix::fs::symlink(
                        self.models.join(asset.file_name),
                        scratch.0.join("models").join(asset.file_name),
                    )
                    .unwrap();
                }
                scratch.store()
            }

            fn frames(&self, name: &str) -> Vec<Frame> {
                let bytes = std::fs::read(self.fixtures.join(name)).unwrap();
                let mut at = 12;
                while &bytes[at..at + 4] != b"data" {
                    at +=
                        8 + u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
                }
                let samples: Vec<i16> = bytes[at + 8..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| i16::from_le_bytes(*pair))
                    .collect();
                samples
                    .chunks(FRAME_SAMPLES)
                    .map(|chunk| {
                        let mut frame = [0; FRAME_SAMPLES];
                        frame[..chunk.len()].copy_from_slice(chunk);
                        frame
                    })
                    .collect()
            }
        }

        fn alive(pid: u32) -> bool {
            Path::new(&format!("/proc/{pid}")).exists()
        }

        fn pid(recognizer: &WhisperRecognizer) -> u32 {
            match &*locked(&recognizer.shared.slot) {
                Slot::Ready(running) => running.process.id(),
                Slot::Idle | Slot::Starting(_) => panic!("the helper is not running"),
            }
        }

        #[tokio::test]
        #[ignore = "needs THALVI_WHISPER_HELPERS, THALVI_WHISPER_MODELS and THALVI_WHISPER_FIXTURES: cargo test -p thalvi-desktop --lib whisper::tests::real -- --ignored"]
        async fn the_real_helper_transcribes_a_spoken_name_and_hears_nothing_in_silence() {
            let files = Files::from_env();
            let scratch = Scratch::new("real");
            let backend = if std::env::var_os("THALVI_WHISPER_VULKAN").is_some() {
                WhisperBackend::Vulkan
            } else {
                WhisperBackend::Cpu
            };
            let recognizer = WhisperRecognizer::new(
                backend,
                &files.helpers,
                files.store(&scratch),
                Recorder::disabled().metrics(),
                &no_words(),
            );
            let Ok(RecognizerEvent::Final(spoken)) =
                dictate(&recognizer, &files.frames("en-names.wav")).await
            else {
                panic!("the capture did not end in a final text");
            };
            assert!(spoken.contains("Priyanka Vasquez"), "{spoken}");
            let device = recognizer.device().expect("the helper is loaded");
            eprintln!("{}: {}", backend.name(), device.line(true));
            assert_eq!(
                matches!(device, Device::Gpu(_)),
                backend == WhisperBackend::Vulkan
            );
            assert_eq!(
                dictate(&recognizer, &files.frames("silence-3s.wav"))
                    .await
                    .unwrap(),
                RecognizerEvent::Final(String::new())
            );
        }

        /// The owner's clip says "Qwen. Qwen 3.8.": the list spells its name right, and neither five
        /// seconds of digital silence nor of room noise may come back as list words.
        #[tokio::test]
        #[ignore = "needs THALVI_WHISPER_HELPERS, THALVI_WHISPER_MODELS and THALVI_WHISPER_FIXTURES: cargo test -p thalvi-desktop --lib whisper::tests::real -- --ignored"]
        async fn the_real_helper_spells_a_listed_name_and_neither_silence_nor_room_noise_becomes_list_words()
         {
            let files = Files::from_env();
            let backend = if std::env::var_os("THALVI_WHISPER_VULKAN").is_some() {
                WhisperBackend::Vulkan
            } else {
                WhisperBackend::Cpu
            };
            let words = list(&[
                "Qwen",
                "Cybrex2",
                "Thalvi",
                "Kubernetes",
                "Grafana",
                "Postgres",
                "Ansible",
                "Nexus",
                "Pellegrino",
                "Serde",
            ]);
            let listed = Scratch::new("real-word-list");
            let with_list = WhisperRecognizer::new(
                backend,
                &files.helpers,
                files.store(&listed),
                Recorder::disabled().metrics(),
                &words,
            );
            let heard = |case: &str, event: Result<RecognizerEvent, RecognizerError>| match event {
                Ok(RecognizerEvent::Final(text)) => text,
                other => panic!("{case}: {other:?}"),
            };
            let spoken = heard(
                "the name with the list",
                dictate(&with_list, &files.frames("qwen.wav")).await,
            );
            assert!(spoken.contains("Qwen"), "{spoken}");

            let unlisted = Scratch::new("real-word-list-plain");
            let plain = WhisperRecognizer::new(
                backend,
                &files.helpers,
                files.store(&unlisted),
                Recorder::disabled().metrics(),
                &no_words(),
            );
            let without = heard(
                "the name without the list",
                dictate(&plain, &files.frames("qwen.wav")).await,
            );
            drop(plain);
            eprintln!("Qwen 3.8: with the list: {spoken} | without the list: {without}");

            for (case, fixture) in [
                ("five seconds of digital silence", "silence-5s.wav"),
                ("five seconds of room noise", "room-noise-5s.wav"),
            ] {
                let noise = heard(case, dictate(&with_list, &files.frames(fixture)).await);
                eprintln!("{case}: {noise:?}");
                for word in words.phrases() {
                    assert!(
                        !noise.to_lowercase().contains(&word.to_lowercase()),
                        "{case} came back as the list word {word:?}: {noise:?}"
                    );
                }
            }
        }

        /// A 54 s capture fed at ten times real time: the GPU's previews grow with it and the final follows.
        #[tokio::test]
        #[ignore = "needs THALVI_WHISPER_HELPERS, THALVI_WHISPER_MODELS and THALVI_WHISPER_FIXTURES and THALVI_WHISPER_VULKAN: cargo test -p thalvi-desktop --lib whisper::tests::real -- --ignored"]
        async fn the_real_vulkan_helper_previews_a_capture_that_is_still_being_spoken() {
            let files = Files::from_env();
            assert!(
                std::env::var_os("THALVI_WHISPER_VULKAN").is_some(),
                "set THALVI_WHISPER_VULKAN to run this test on the GPU"
            );
            let scratch = Scratch::new("real-previews");
            let recognizer = WhisperRecognizer::new(
                WhisperBackend::Vulkan,
                &files.helpers,
                files.store(&scratch),
                Recorder::disabled().metrics(),
                &no_words(),
            );
            let mut session = within(recognizer.connect()).await.unwrap();
            let mut frames = files.frames("say-long.wav").into_iter();
            let mut pace = tokio::time::interval(Duration::from_millis(8));
            let mut partials = Vec::new();
            let mut ended = None;
            let spoken = within(async {
                loop {
                    tokio::select! {
                        biased;
                        event = session.next_event() => match event.unwrap() {
                            RecognizerEvent::Partial(text) => {
                                eprintln!("preview {}: {text}", partials.len());
                                partials.push(text);
                            }
                            RecognizerEvent::Final(text) => break text,
                            RecognizerEvent::Ended => {}
                        },
                        _ = pace.tick(), if ended.is_none() => {
                            if let Some(frame) = frames.next() {
                                session.send_frame(&frame).await.unwrap();
                            } else {
                                session.finish_audio().await.unwrap();
                                ended = Some(Instant::now());
                            }
                        },
                    }
                }
            })
            .await;
            assert!(partials.len() >= 2, "{partials:?}");
            let (first, last) = (&partials[0], &partials[partials.len() - 1]);
            assert!(last.len() > first.len(), "the previews grow: {partials:?}");
            assert_ne!(spoken, "");
            let released = ended.expect("the capture ended before its final");
            eprintln!(
                "{} previews; the final came {:?} after the capture ended: {spoken}",
                partials.len(),
                released.elapsed()
            );
        }

        /// The app is killed without running a destructor: the test runs itself twice, and the first run
        /// aborts as soon as the helper is up.
        #[tokio::test]
        #[ignore = "needs THALVI_WHISPER_HELPERS, THALVI_WHISPER_MODELS and THALVI_WHISPER_FIXTURES: cargo test -p thalvi-desktop --lib whisper::tests::real -- --ignored"]
        async fn the_real_helper_does_not_outlive_an_app_that_aborts() {
            const CHILD: &str = "THALVI_WHISPER_ABORTING_CHILD";
            let files = Files::from_env();
            let scratch = Scratch::new("real-abort");
            let recognizer = WhisperRecognizer::new(
                WhisperBackend::Cpu,
                &files.helpers,
                files.store(&scratch),
                Recorder::disabled().metrics(),
                &no_words(),
            );
            if std::env::var_os(CHILD).is_some() {
                within(recognizer.load()).await.unwrap();
                println!("HELPER_PID={}", pid(&recognizer));
                std::process::abort();
            }
            drop(recognizer);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "whisper::tests::real::the_real_helper_does_not_outlive_an_app_that_aborts",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let printed = String::from_utf8_lossy(&output.stdout);
            let helper: u32 = printed
                .lines()
                .find_map(|line| line.strip_prefix("HELPER_PID="))
                .unwrap_or_else(|| panic!("the aborting run printed no helper: {printed}"))
                .parse()
                .unwrap();
            assert!(!output.status.success(), "the child aborted");
            within(async {
                while alive(helper) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
        }
    }
}

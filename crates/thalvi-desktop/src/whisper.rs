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
use crate::device::{self, Device, Export, Fallback, Gpu, Step};
use crate::frame::Frame;
use crate::metrics::{Event, Metrics};
use crate::models::{ModelStore, WHISPER_TURBO, WhisperAssets};
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
                "Experimental. OpenAI's Whisper Turbo, run by a helper that Thalvi starts. Text appears only after you stop speaking, and the word list is ignored. On the CPU it is slow: the text can take as long as you spoke, or longer. The language is detected automatically; only English and German have been tried."
            }
            Self::Vulkan => {
                "Experimental. OpenAI's Whisper Turbo, run by a helper that Thalvi starts on your graphics card through Vulkan. Text appears only after you stop speaking, and the word list is ignored. Its speed on your GPU is untested. If no GPU is found, it says so; pick the CPU entry then. The language is detected automatically; only English and German have been tried."
            }
        }
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

#[expect(
    clippy::cast_precision_loss,
    reason = "a capture has far fewer frames than f64 counts exactly"
)]
fn audio_seconds(frames: usize) -> f64 {
    (frames * crate::frame::FRAME_SAMPLES) as f64 / f64::from(SAMPLE_RATE)
}

/// The text of one inference, with its lines run together.
async fn infer(
    client: &reqwest::Client,
    port: u16,
    fields: &[(&str, &str)],
    wav: &[u8],
    timeout: Duration,
) -> Result<String, Failure> {
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
    Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
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
    ) -> Self {
        Self::with_launcher(
            ProcessLauncher,
            WHISPER_TURBO,
            backend,
            helpers,
            store,
            metrics,
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
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                launcher,
                backend,
                helpers: helpers.to_owned(),
                store,
                metrics,
                name: backend.name().to_owned(),
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
        self.tell(if self.store.is_cached(&assets).await {
            Step::Reading
        } else {
            Step::Fetching(Export::Cpu)
        });
        let step = |step| self.tell(step);
        self.store
            .fetch_speech(&assets, device::downloading(Export::Cpu, &step))
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
        let began = Instant::now();
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
        // The Vulkan helper compiles its pipelines on its first request, which would otherwise be the user's.
        let silence = wav(&vec![0; SAMPLE_RATE as usize]);
        let fields = [
            ("response_format", "json"),
            ("language", "auto"),
            ("vad", "false"),
        ];
        infer(client, launch.port, &fields, &silence, READY_TIMEOUT)
            .await
            .map_err(|failure| HelperError::WarmUp(describe(&failure)))?;
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
            finished: false,
            finished_signal: Notify::new(),
            request: Request::Buffering,
        })
    }

    fn loading(&self) -> Option<watch::Receiver<Step>> {
        Some(self.shared.steps.subscribe())
    }
}

enum Request {
    Buffering,
    Sent {
        reply: oneshot::Receiver<Result<String, RecognizerError>>,
        task: JoinHandle<()>,
        at: Instant,
    },
    Answered {
        ok: bool,
    },
}

/// One capture, buffered until it ends and then transcribed in one request.
pub struct WhisperSession<L: Launcher = ProcessLauncher> {
    shared: Arc<Shared<L>>,
    running: Arc<Running<L::Process>>,
    frames: Vec<Frame>,
    finished: bool,
    finished_signal: Notify,
    request: Request,
}

fn stopped() -> RecognizerError {
    RecognizerError::said("Speech transcription stopped unexpectedly. Nothing was delivered.")
}

impl<L: Launcher> WhisperSession<L> {
    fn send_request(&mut self) -> Request {
        let (sender, reply) = oneshot::channel();
        let running = Arc::clone(&self.running);
        let shared = Arc::clone(&self.shared);
        let frames = std::mem::take(&mut self.frames);
        let at = Instant::now();
        let task = tokio::spawn(async move {
            let seconds = audio_seconds(frames.len());
            let result = if frames.is_empty() {
                Ok(String::new())
            } else {
                let audio = wav(frames.as_flattened());
                let fields = [("response_format", "json"), ("language", "auto")];
                let timeout = request_timeout(frames.len());
                infer(&running.client, running.port, &fields, &audio, timeout).await
            };
            shared.metrics.metric(
                Event::LocalStt,
                if result.is_ok() { "ok" } else { "error" },
                Some(at.elapsed()),
                running.device.tag(json!({
                    "engine": "whisper_helper",
                    "model": shared.name,
                    "audio_seconds": seconds,
                })),
            );
            let _ = sender.send(result.map_err(|failure| failure.error()));
        });
        Request::Sent { reply, task, at }
    }
}

impl<L: Launcher> RecognizerSession for WhisperSession<L> {
    fn buffers_audio(&self) -> bool {
        true
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Finishing only updates buffered capture state."
    )]
    async fn finish_audio(&mut self) -> Result<(), RecognizerError> {
        self.finished = true;
        self.finished_signal.notify_one();
        Ok(())
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Frames are buffered locally without I/O."
    )]
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        self.frames.push(*frame);
        Ok(())
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        if !self.finished {
            self.finished_signal.notified().await;
        }
        if matches!(self.request, Request::Buffering) {
            self.request = self.send_request();
        }
        let Request::Sent { reply, .. } = &mut self.request else {
            return Err(stopped());
        };
        let answer = reply.await;
        let ok = matches!(answer, Ok(Ok(_)));
        self.request = Request::Answered { ok };
        match answer {
            Ok(Ok(text)) => Ok(RecognizerEvent::Final(text)),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(stopped()),
        }
    }

    async fn cancel(self) {}

    async fn close(self) {}
}

impl<L: Launcher> Drop for WhisperSession<L> {
    fn drop(&mut self) {
        match &self.request {
            Request::Buffering | Request::Answered { ok: true } => {}
            Request::Answered { ok: false } => self.shared.discard(&self.running),
            Request::Sent { task, at, .. } => {
                task.abort();
                self.shared.metrics.metric(
                    Event::LocalStt,
                    "cancelled",
                    Some(at.elapsed()),
                    self.running.device.tag(json!({
                        "engine": "whisper_helper",
                        "model": self.shared.name,
                    })),
                );
                self.shared.discard(&self.running);
            }
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
    use tokio::sync::Notify;

    use super::*;
    use crate::frame::FRAME_SAMPLES;
    use crate::metrics::{RecordKinds, Recorder};
    use crate::models::{Asset, ModelKind};

    const WAIT: Duration = Duration::from_secs(120);

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
        Status(u16),
        Garbage,
        /// Answers once the test says so, unless its helper was killed: a dead process answers nothing.
        Held(&'static str),
    }

    struct Server {
        health_calls: AtomicUsize,
        unready: usize,
        seen: Mutex<Vec<Seen>>,
        seen_count: watch::Sender<usize>,
        replies: Mutex<VecDeque<Reply>>,
        release: Notify,
        killed: Arc<AtomicBool>,
    }

    async fn health(State(server): State<Arc<Server>>) -> StatusCode {
        if server.health_calls.fetch_add(1, Ordering::SeqCst) < server.unready {
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
            Reply::Text("")
        } else {
            locked(&server.replies)
                .pop_front()
                .unwrap_or(Reply::Text(""))
        };
        let ok = |text: &str| (StatusCode::OK, json!({"text": text}).to_string());
        match reply {
            Reply::Text(text) => ok(text),
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
        output: Vec<&'static str>,
        /// The helper has ended with this code as soon as it starts.
        exits_with: Option<i32>,
        replies: Vec<Reply>,
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
            let plan = locked(&self.plans).pop_front().unwrap_or_default();
            let killed = Arc::new(AtomicBool::new(false));
            let server = Arc::new(Server {
                health_calls: AtomicUsize::new(0),
                unready: plan.unready,
                seen: Mutex::default(),
                seen_count: watch::channel(0).0,
                replies: Mutex::new(plan.replies.into()),
                release: Notify::new(),
                killed: Arc::clone(&killed),
            });
            let exit = Arc::new(Mutex::new(plan.exits_with.map(|code| Exit(Some(code)))));
            let handle = plan.exits_with.is_none().then(|| {
                let listener =
                    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, launch.port)).unwrap();
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
        )
    }

    fn recognizer_with(
        fake: &Fake,
        backend: WhisperBackend,
        scratch: &Scratch,
        assets: WhisperAssets<'static>,
        metrics: Metrics,
    ) -> WhisperRecognizer<Fake> {
        WhisperRecognizer::with_launcher(
            fake.clone(),
            assets,
            backend,
            &scratch.helpers(),
            scratch.store(),
            metrics,
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
    async fn the_helper_is_ready_only_after_it_answers_health_and_one_warm_up_without_voice_detection()
     {
        let scratch = Scratch::with_files("start");
        let fake = Fake::new([Plan {
            unready: 3,
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
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
        assert_eq!(seen.len(), 1, "one warm-up and nothing else");
        assert_eq!(seen[0].field("vad"), Some("false"));
        assert!(seen[0].samples().iter().all(|sample| *sample == 0));
        assert_eq!(seen[0].samples().len(), 16_000, "one second");
        assert_eq!(recognizer.device(), Some(Device::Cpu(Fallback::CpuChoice)));
        assert_eq!(helper.launch.model, scratch.store().path(&FAKE_FILES.model));
        assert_eq!(helper.launch.vad, scratch.store().path(&FAKE_FILES.vad));
        assert_eq!(
            helper.launch.executable,
            scratch
                .helpers()
                .join("cpu")
                .join(format!("whisper-server{}", std::env::consts::EXE_SUFFIX))
        );
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
        let scratch = Scratch::with_files("missing");
        let recognizer = WhisperRecognizer::with_launcher(
            ProcessLauncher,
            FAKE_FILES,
            WhisperBackend::Vulkan,
            &scratch.helpers(),
            scratch.store(),
            Recorder::disabled().metrics(),
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
        );
        let mut steps = recognizer.steps();

        let loading = recognizer.load();
        tokio::pin!(loading);
        within(async {
            tokio::select! {
                _ = &mut loading => panic!("the load ended before the download was released"),
                step = steps.wait_for(|step| matches!(step, Step::Fetching(_) | Step::Downloading(..))) => {
                    assert!(matches!(*step.unwrap(), Step::Fetching(Export::Cpu) | Step::Downloading(Export::Cpu, _)));
                }
            }
        })
        .await;
        assert!(!scratch.store().is_cached(&assets).await);
        release.notify_one();
        within(&mut loading).await.unwrap();

        assert!(
            scratch.store().is_cached(&assets).await,
            "both files are in the store"
        );
        assert_eq!(
            *recognizer.steps().borrow(),
            Step::Reading,
            "the download step is over"
        );
        assert_eq!(fake.launches(), 1);
        server.abort();
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
        assert_eq!(seen.len(), 2, "the warm-up and the capture");
        let capture = &seen[1];
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
            u16::from_le_bytes([capture.wav[34], capture.wav[35]]),
            16,
            "bits"
        );
        assert_eq!(capture.samples(), frames.as_flattened());
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
            () = stuck.seen_requests(2) => {}
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
        assert_eq!(fresh.len(), 2, "a warm-up and the new capture only");
        assert_eq!(fresh[1].samples().len(), 2 * FRAME_SAMPLES);
        assert_eq!(
            stuck.seen().len(),
            2,
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
            () = stuck.seen_requests(2) => {}
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
    async fn a_helper_that_exited_is_started_again_for_the_next_capture() {
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
        *locked(&fake.helper(0).exit) = Some(Exit(Some(139)));
        assert_eq!(
            recognizer.device(),
            None,
            "a helper that ended is not a loaded model"
        );

        let event = dictate(&recognizer, &[frame(1)]).await.unwrap();
        assert_eq!(event, RecognizerEvent::Final("again".to_owned()));
        assert_eq!(fake.launches(), 2);
    }

    #[tokio::test]
    async fn a_restart_does_not_read_the_model_files_again() {
        let scratch = Scratch::with_files("verified-once");
        let fake = Fake::new([]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        within(recognizer.load()).await.unwrap();
        std::fs::remove_dir_all(scratch.0.join("models")).unwrap();
        *locked(&fake.helper(0).exit) = Some(Exit(None));

        within(recognizer.load()).await.unwrap();
        assert_eq!(fake.launches(), 2);
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

    #[tokio::test(start_paused = true)]
    async fn a_request_that_never_answers_ends_as_a_timeout() {
        let scratch = Scratch::with_files("timeout");
        let fake = Fake::new([Plan {
            replies: vec![Reply::Held("never")],
            ..Plan::default()
        }]);
        let recognizer = recognizer(&fake, WhisperBackend::Cpu, &scratch);
        let mut session = recognizer.connect().await.unwrap();
        session.send_frame(&frame(1)).await.unwrap();
        session.finish_audio().await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(120), session.next_event())
            .await
            .expect("the request's own timeout ends it first")
            .unwrap_err();
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

    /// Tests that run the packaged helper and the real model, which CI does not have: each is skipped, with
    /// its reason printed, unless `THALVI_WHISPER_HELPERS` (the folder with `cpu` and `vulkan`),
    /// `THALVI_WHISPER_MODELS` (holding `ggml-large-v3-turbo-q5_0.bin` and `ggml-silero-v6.2.0.bin`) and
    /// `THALVI_WHISPER_FIXTURES` (16 kHz mono WAV files) name them.
    #[cfg(target_os = "linux")]
    mod real {
        use super::*;

        struct Files {
            helpers: PathBuf,
            models: PathBuf,
            fixtures: PathBuf,
        }

        impl Files {
            fn from_env(test: &str) -> Option<Self> {
                let var = |name| std::env::var_os(name).map(PathBuf::from);
                let found = (|| {
                    Some(Self {
                        helpers: var("THALVI_WHISPER_HELPERS")?,
                        models: var("THALVI_WHISPER_MODELS")?,
                        fixtures: var("THALVI_WHISPER_FIXTURES")?,
                    })
                })();
                if found.is_none() {
                    eprintln!(
                        "{test} skipped: set THALVI_WHISPER_HELPERS, THALVI_WHISPER_MODELS and THALVI_WHISPER_FIXTURES to run it against the real helper"
                    );
                }
                found
            }

            /// A store that links the model files, so that nothing is copied or downloaded.
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
        async fn the_real_helper_transcribes_a_spoken_name_and_hears_nothing_in_silence() {
            let Some(files) = Files::from_env(
                "the_real_helper_transcribes_a_spoken_name_and_hears_nothing_in_silence",
            ) else {
                return;
            };
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

        #[tokio::test]
        async fn the_real_helper_is_gone_when_its_recognizer_is() {
            let Some(files) = Files::from_env("the_real_helper_is_gone_when_its_recognizer_is")
            else {
                return;
            };
            let scratch = Scratch::new("real-drop");
            let recognizer = WhisperRecognizer::new(
                WhisperBackend::Cpu,
                &files.helpers,
                files.store(&scratch),
                Recorder::disabled().metrics(),
            );
            recognizer.load().await.unwrap();
            let helper = pid(&recognizer);
            assert!(alive(helper));
            drop(recognizer);
            assert!(!alive(helper), "no whisper-server is left");
        }

        /// The app is killed without running a destructor: the test runs itself twice, and the first run
        /// aborts as soon as the helper is up.
        #[tokio::test]
        async fn the_real_helper_does_not_outlive_an_app_that_aborts() {
            const CHILD: &str = "THALVI_WHISPER_ABORTING_CHILD";
            let name = "the_real_helper_does_not_outlive_an_app_that_aborts";
            let Some(files) = Files::from_env(name) else {
                return;
            };
            let scratch = Scratch::new("real-abort");
            let recognizer = WhisperRecognizer::new(
                WhisperBackend::Cpu,
                &files.helpers,
                files.store(&scratch),
                Recorder::disabled().metrics(),
            );
            if std::env::var_os(CHILD).is_some() {
                recognizer.load().await.unwrap();
                println!("HELPER_PID={}", pid(&recognizer));
                std::process::abort();
            }
            drop(recognizer);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("whisper::tests::real::{name}"),
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

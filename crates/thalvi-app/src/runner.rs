//! Runs the capture engine on its own thread and follows the app's control messages.

use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use thalvi_desktop::audio::{Frames, InputGate, Microphone};
use thalvi_desktop::capture::{CaptureRoute, EndingPauses, Endings};
use thalvi_desktop::chosen::{Chosen, Preload};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::delivery::{Bound, Delivery, Finished};
use thalvi_desktop::device::Step;
use thalvi_desktop::draft::{Draft, Drafts};
use thalvi_desktop::engine::{
    CaptureOutcome, CpalCuePlayer, CuePlayer, Engine, EngineHandle, EngineState, FrameSource,
    Parts, StreamingExtra, WakeDetector, WakeTrigger,
};
use thalvi_desktop::entries::LiveKind;
use thalvi_desktop::feed::{Clip, Fed, Feed, FeedEnd, Feeder};
use thalvi_desktop::history::{Capture, Store as History};
use thalvi_desktop::hold::{HoldInput, KeyState, Presses, Refusal};
use thalvi_desktop::host::{Desktop, HoldReport, Host, WindowApp};
use thalvi_desktop::metrics::{Event, Metrics, Recorder};
use thalvi_desktop::models::{self, ListedSpeech, ModelStore};
use thalvi_desktop::recognizer::Recognizer;
use thalvi_desktop::report::{UserError, causes};
use thalvi_desktop::speech_device::SpeechDevices;
use thalvi_desktop::spotter::{KeywordSpotter, OnnxSpotter, SpotterError};
use thalvi_desktop::wake::{WakeConfig, WakeEngine};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, LocalSet};

use crate::status::{
    Failure, FailureKind, Mode, SPOTTER_DOWNLOAD_NOTICE, STARTING_NOTICE, Status, View,
    WAKE_DOWNLOAD_NOTICE, load_notice,
};
use crate::update::Stop;

/// How long a quit waits for the engine to close the microphone and the recognizer.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// What the app asks of the engine.
#[derive(Debug)]
pub enum Control {
    /// Off closes the microphone; on opens it again.
    SetListening(bool),
    /// Toggle the runner-owned listening intent, including while starting.
    ToggleListening,
    /// Cancels the capture in progress, and stops a file being fed.
    Cancel,
    /// Plays a file's audio in place of the microphone's.
    Feed(Feed),
    /// Takes new settings in between captures.
    Apply(Box<DesktopConfig>),
    /// The desktop refused to report the hold key; the notice says so in plain words.
    HoldKeyFailed(String),
    /// The desktop can report the hold keys again.
    HoldKeyWorks,
    /// The status overlay failed, or recovered (`None`). Shown while nothing else has failed.
    Overlay(Option<Failure>),
    /// Stops for an update, but only if nothing is going on.
    RestartIfIdle(Restart),
    /// A download of this catalog model finished; load it if the active entry uses it and it is not loaded.
    ReloadSpeechModel(ListedSpeech),
    /// Stops the local chat child and removes its model between captures.
    RemoveChat(oneshot::Sender<Result<(), String>>),
    /// Cancels any capture, closes the microphone and the recognizer, and stops.
    Quit,
}

/// A request to stop for an update: `prepare` saves what the next start needs, given whether
/// listening is paused, and runs only once the engine has found itself idle.
pub struct Restart {
    prepare: Box<dyn FnOnce(bool) -> Result<(), String> + Send>,
    reply: oneshot::Sender<Stop>,
}

impl Restart {
    pub fn new(
        prepare: impl FnOnce(bool) -> Result<(), String> + Send + 'static,
        reply: oneshot::Sender<Stop>,
    ) -> Self {
        Self {
            prepare: Box::new(prepare),
            reply,
        }
    }
}

impl std::fmt::Debug for Restart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Restart")
    }
}

/// The saved microphone choice, the device actually open, and whether listening is paused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    /// The device named in the saved config; `None` follows the system default.
    pub selected: Option<String>,
    /// The name of the device the opened stream uses; `None` when no stream is open.
    pub active: Option<String>,
    pub paused: bool,
    /// Why the selected device could not be opened while `active` stays open; `None` otherwise.
    pub problem: Option<String>,
}

/// The app's side of the engine thread.
pub struct EngineLink {
    control: mpsc::UnboundedSender<Control>,
    status: watch::Receiver<Status>,
    level: watch::Receiver<f32>,
    audio: watch::Receiver<AudioStatus>,
    feeding: watch::Receiver<bool>,
    stopped: Mutex<std_mpsc::Receiver<()>>,
    key: HoldInput,
    pub last_writing_window: Arc<Mutex<Option<WindowApp>>>,
}

impl EngineLink {
    /// Starts the engine thread; it loads the wake word models before it listens. Listening starts
    /// `paused` when the user had paused it before an update restarted the app.
    ///
    /// # Errors
    /// Fails when the thread or its runtime cannot start.
    pub fn start(
        config: DesktopConfig,
        delivery: Arc<Delivery<Desktop>>,
        history: Arc<History>,
        drafts: Arc<Drafts>,
        devices: Arc<SpeechDevices>,
        paused: bool,
        whisper_helpers: PathBuf,
    ) -> Result<Self, String> {
        let (link, ends, stopped) = Self::wire(&config, paused);
        let (ready_tx, ready) = std_mpsc::channel();
        std::thread::Builder::new()
            .name("thalvi-engine".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| format!("Thalvi could not start: {error}"));
                let runtime = match runtime {
                    Ok(runtime) => runtime,
                    Err(message) => {
                        let _ = ready_tx.send(Err(message));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let rig = DesktopRig {
                    http: reqwest::Client::new(),
                    devices,
                    whisper_helpers,
                };
                LocalSet::new().block_on(
                    &runtime,
                    ends.run(config, paused, rig, delivery, history, drafts),
                );
                finish_runtime(runtime, &stopped);
            })
            .map_err(|error| format!("Thalvi could not start: {error}"))?;
        ready
            .recv()
            .map_err(|_| "Thalvi could not start the engine.".to_owned())??;
        Ok(link)
    }

    fn wire(config: &DesktopConfig, paused: bool) -> (Self, RunnerEnds, std_mpsc::Sender<()>) {
        let last_writing_window = Arc::new(Mutex::new(None));
        let (control, control_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(Status::Loading {
            notice: STARTING_NOTICE.to_owned(),
            downloading: false,
        });
        let (level_tx, level) = watch::channel(0.0);
        let (audio_tx, audio) = watch::channel(AudioStatus {
            selected: config.microphone.clone(),
            active: None,
            paused,
            problem: None,
        });
        let (feeding_tx, feeding) = watch::channel(false);
        let (key, key_state) = HoldInput::new();
        let (stopped_tx, stopped) = std_mpsc::channel();
        let link = Self {
            control,
            status,
            level,
            audio,
            feeding,
            stopped: Mutex::new(stopped),
            key,
            last_writing_window: Arc::clone(&last_writing_window),
        };
        let ends = RunnerEnds {
            control: control_rx,
            status: status_tx,
            level: Arc::new(level_tx),
            audio: audio_tx,
            feeding: Arc::new(feeding_tx),
            key: key_state,
            last_writing_window,
        };
        (link, ends, stopped_tx)
    }

    /// Where the hold key's presses and releases come in.
    #[must_use]
    pub fn hold_input(&self) -> HoldInput {
        self.key.clone()
    }

    pub fn send(&self, control: Control) {
        // The engine thread is gone only while the app quits or after it failed.
        let _ = self.control.send(control);
    }

    /// Tells the user about a hold key problem found after the keys were set up, such as binds
    /// Hyprland dropped.
    pub fn hold_problems(&self) -> impl Fn(HoldReport) + Send + Sync + 'static {
        let control = self.control.clone();
        move |notice| {
            let _ = control.send(match notice {
                HoldReport::Problem(notice) => Control::HoldKeyFailed(notice),
                HoldReport::Working => Control::HoldKeyWorks,
            });
        }
    }

    #[must_use]
    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    #[must_use]
    pub fn status_updates(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    #[must_use]
    pub fn level_updates(&self) -> watch::Receiver<f32> {
        self.level.clone()
    }

    #[must_use]
    pub fn audio(&self) -> AudioStatus {
        self.audio.borrow().clone()
    }

    #[must_use]
    pub fn audio_updates(&self) -> watch::Receiver<AudioStatus> {
        self.audio.clone()
    }

    /// Whether a file's audio is being fed in place of the microphone's.
    #[must_use]
    pub fn feeding_updates(&self) -> watch::Receiver<bool> {
        self.feeding.clone()
    }

    /// Feeds `clip` in place of the microphone and says how it went, once it has.
    pub async fn feed(&self, clip: Clip) -> FeedEnd {
        let (feed, ended) = Feed::new(clip);
        self.send(Control::Feed(feed));
        // The engine is gone only while the app quits or after it failed.
        ended.await.unwrap_or(FeedEnd::Cancelled)
    }

    /// Stops the engine and waits until the microphone and the recognizer are closed.
    ///
    /// Blocks for at most a few seconds.
    pub fn quit(&self) {
        self.send(Control::Quit);
        self.wait_stopped();
    }

    /// Waits until the engine has closed the microphone and the recognizer, for at most a few seconds.
    pub fn wait_stopped(&self) {
        let stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = stopped.recv_timeout(QUIT_TIMEOUT);
    }
}

struct RunnerEnds {
    control: mpsc::UnboundedReceiver<Control>,
    status: watch::Sender<Status>,
    level: Arc<watch::Sender<f32>>,
    audio: watch::Sender<AudioStatus>,
    feeding: Arc<watch::Sender<bool>>,
    key: watch::Receiver<KeyState>,
    last_writing_window: Arc<Mutex<Option<WindowApp>>>,
}

impl RunnerEnds {
    async fn run<S: Rig>(
        self,
        config: DesktopConfig,
        paused: bool,
        rig: S,
        delivery: Arc<Delivery<S::Host>>,
        history: Arc<History>,
        drafts: Arc<Drafts>,
    ) {
        let publisher = Publisher::new(self.status, config.ending_pause);
        publisher.update(|view| view.wake_words = config.wake.enabled());
        Runner::new(
            config,
            delivery,
            self.control,
            publisher,
            self.level,
            self.audio,
            Shared {
                rig,
                key: self.key,
                history,
                drafts,
                feeding: self.feeding,
                paused,
                last_writing_window: self.last_writing_window,
            },
        )
        .run()
        .await;
    }
}

/// Finishes background writes before letting the app exit.
fn finish_runtime(runtime: tokio::runtime::Runtime, stopped: &std_mpsc::Sender<()>) {
    drop(runtime);
    let _ = stopped.send(());
}

/// Loads the wake models, with the status saying so while a file downloads.
async fn load_wake<T>(
    publisher: &Publisher,
    config: &WakeConfig,
    store: &ModelStore,
    load: impl Future<Output = T>,
) -> T {
    let downloads = WakeEngine::needs_download(config, store).await;
    if !downloads {
        println!("Loading the wake word models.");
        return load.await;
    }
    println!("{WAKE_DOWNLOAD_NOTICE}");
    publisher.mode(Mode::WakeDownload);
    let loaded = load.await;
    publisher.mode(Mode::Starting);
    loaded
}

/// Loads the ending spotter next to the wake models. It only double-checks the transcript, so a
/// failure is a notice, shown with the first capture, and dictation goes on without it.
async fn load_spotter(
    publisher: &Publisher,
    endings: &Endings,
    store: &ModelStore,
    recorder: &Recorder,
    load: impl Future<Output = Result<Box<dyn KeywordSpotter>, SpotterError>>,
) -> (Option<Box<dyn KeywordSpotter>>, Option<String>) {
    let started = Instant::now();
    let mut downloads = false;
    for asset in [
        models::SPOTTER.encoder,
        models::SPOTTER.decoder,
        models::SPOTTER.joiner,
        models::SPOTTER.pieces,
    ] {
        downloads |= !store.is_asset_cached(&asset).await;
    }
    if downloads {
        println!("{SPOTTER_DOWNLOAD_NOTICE}");
        publisher.mode(Mode::SpotterDownload);
    }
    let loaded = load.await;
    if downloads {
        publisher.mode(Mode::Starting);
    }
    recorder.metric(
        Event::EndingSpotter,
        if loaded.is_ok() { "ok" } else { "error" },
        Some(started.elapsed()),
        json!({"call": "load", "phrases": endings.entries().count(), "downloaded": downloads}),
    );
    match loaded {
        Ok(spotter) => (Some(spotter), None),
        Err(error) => {
            let notice = format!("{error} Endings still work from the transcript.");
            println!("{notice}");
            (None, Some(notice))
        }
    }
}

/// Derives the status from the view and publishes it when it changes.
pub(crate) struct Publisher {
    view: RefCell<View>,
    out: watch::Sender<Status>,
    /// The failure the last speech model load showed, so a reload clears that and no other.
    load_failure: RefCell<Option<Failure>>,
}

impl Publisher {
    fn new(out: watch::Sender<Status>, pauses: EndingPauses) -> Rc<Self> {
        Rc::new(Self {
            view: RefCell::new(View {
                // The models load before the first capture can start.
                mode: Mode::Starting,
                engine: EngineState::Listening,
                target: None,
                copies: false,
                pauses,
                loading: None,
                wake_words: true,
            }),
            out,
            load_failure: RefCell::new(None),
        })
    }

    fn update(&self, change: impl FnOnce(&mut View)) {
        let mut view = self.view.borrow_mut();
        change(&mut view);
        let status = view.status();
        self.out.send_if_modified(|current| {
            if *current == status {
                return false;
            }
            *current = status;
            true
        });
    }

    fn failed(&self) -> bool {
        matches!(self.view.borrow().mode, Mode::Failed(_))
    }

    /// Whether the status shows an overlay failure, which recovery may clear.
    fn overlay_failed(&self) -> bool {
        matches!(&self.view.borrow().mode, Mode::Failed(failure) if failure.headline == "Status overlay unavailable")
    }

    fn hold_failed(&self) -> bool {
        matches!(&self.view.borrow().mode, Mode::Failed(failure) if failure.headline == "Hold key unavailable")
    }

    fn mode(&self, mode: Mode) {
        self.update(|view| view.mode = mode);
    }

    /// Takes the speech model load failure off the status, if it still shows it, so the next load shows its progress.
    fn load_retried(&self) {
        let Some(failure) = self.load_failure.take() else {
            return;
        };
        self.update(|view| {
            if view.mode == Mode::Failed(failure) {
                view.mode = Mode::Idle;
            }
        });
    }

    /// A capture started at the wake: in `window` when one was bound, on a desktop that `copies`
    /// the text to the clipboard or pastes it.
    fn woke(&self, window: Option<String>, copies: bool) {
        self.update(|view| {
            view.copies = copies;
            // The clipboard is no window to name; the label and the notice say what happens.
            view.target = window.filter(|_| !copies);
        });
    }

    fn loading(&self, load: Option<Step>) {
        self.update(|view| view.loading = load);
    }

    /// Says why a press of the hold key started nothing. A refusal that only repeats an earlier
    /// failure does not hide it, and followers hear of it even when the status is unchanged, so
    /// an overlay that has hidden the failure shows it again.
    fn refused(&self, refusal: Refusal) {
        if refusal == Refusal::Paused || !self.failed() {
            self.mode(Mode::Failed(Failure {
                kind: FailureKind::Other,
                notice: refusal.notice().to_owned(),
                headline: refusal.headline(),
                causes: Vec::new(),
            }));
        }
        self.out.send_modify(|_| {});
    }

    fn notice_sink(self: &Rc<Self>) -> Box<dyn Fn(&str)> {
        let publisher = Rc::clone(self);
        Box::new(move |notice| {
            println!("{notice}");
            if !publisher.failed() {
                publisher.mode(Mode::Failed(Failure {
                    kind: FailureKind::Other,
                    notice: notice.to_owned(),
                    headline: "Dictation notice",
                    causes: Vec::new(),
                }));
            }
        })
    }
}

/// Loads the model, showing each step it takes until it is ready, and a failure as a notice.
async fn load_showing(local: &(impl Preload + ?Sized), publisher: &Publisher) {
    let mut steps = local.steps();
    let load = local.load();
    tokio::pin!(load);
    let loaded = loop {
        tokio::select! {
            loaded = &mut load => break loaded,
            Ok(()) = steps.changed() => {
                let step = *steps.borrow_and_update();
                println!("{}", load_notice(step));
                publisher.loading(Some(step));
            }
        }
    };
    if let Err(error) = &loaded {
        println!("{error}");
    }
    publisher.update(|view| {
        view.loading = None;
        if let Err(error) = loaded
            && !matches!(view.mode, Mode::Paused | Mode::Delivering(_))
        {
            let failure = Failure {
                kind: FailureKind::Other,
                notice: format!("{error} Dictation will try again when you start it."),
                headline: error.headline(),
                causes: causes(&error),
            };
            *publisher.load_failure.borrow_mut() = Some(failure.clone());
            view.mode = Mode::Failed(failure);
        }
    });
}

/// Whether a finished download calls for loading the model again: it is the one in use and not loaded.
fn reload_wanted<P: Preload + ?Sized>(uses_downloaded: bool, local: Option<&P>) -> bool {
    uses_downloaded && local.is_some_and(|local| local.device().is_none())
}

pub(crate) trait Rig: Sized {
    type Recognizer: Recognizer + 'static;
    type Wake: WakeDetector;
    type Cues: CuePlayer + 'static;
    type Input: Input;
    type Host: Host + 'static;

    async fn load(
        &self,
        config: &DesktopConfig,
        gate: &InputGate,
        publisher: &Rc<Publisher>,
    ) -> Result<Loaded<Self>, Failure>;
    fn input(&self, gate: InputGate, device: Option<String>, metrics: Metrics) -> Self::Input;
}

pub(crate) trait Input: Sized {
    type Frames: FrameSource;

    fn active_name(&self) -> Option<&str>;
    fn select(&mut self, device: Option<String>);
    fn pause(&mut self);
    async fn open(self) -> Result<(Self, Result<Self::Frames, Failure>), Failure>;
    async fn replace(self) -> Result<(Self, Result<Self::Frames, Failure>), Failure>;
}

impl Input for Microphone {
    type Frames = Frames;

    fn active_name(&self) -> Option<&str> {
        self.active_name()
    }

    fn select(&mut self, device: Option<String>) {
        self.select(device);
    }

    fn pause(&mut self) {
        self.pause();
    }

    async fn open(mut self) -> Result<(Self, Result<Frames, Failure>), Failure> {
        let (microphone, frames) = tokio::task::spawn_blocking(move || {
            let frames = Microphone::open(&mut self);
            (self, frames)
        })
        .await
        .map_err(|error| audio_thread_crashed(&error))?;
        Ok((microphone, frames.map_err(|error| Failure::of(&error))))
    }

    async fn replace(mut self) -> Result<(Self, Result<Frames, Failure>), Failure> {
        let (microphone, frames) = tokio::task::spawn_blocking(move || {
            let frames = Microphone::replace(&mut self);
            (self, frames)
        })
        .await
        .map_err(|error| audio_thread_crashed(&error))?;
        Ok((microphone, frames.map_err(|error| Failure::of(&error))))
    }
}

pub(crate) struct Loaded<S: Rig> {
    recognizer: S::Recognizer,
    wake: S::Wake,
    cues: S::Cues,
    recorder: Recorder,
    http: reqwest::Client,
    chat: Option<thalvi_desktop::chat::Chat>,
    spotter: Option<Box<dyn KeywordSpotter>>,
    spotter_off: Option<String>,
    extras: Vec<StreamingExtra<S::Recognizer>>,
    local: Option<Arc<dyn Preload>>,
    extra_locals: Vec<Arc<dyn Preload>>,
}

struct DesktopRig {
    http: reqwest::Client,
    devices: Arc<SpeechDevices>,
    whisper_helpers: PathBuf,
}

impl Rig for DesktopRig {
    type Recognizer = Chosen;
    type Wake = WakeEngine;
    type Cues = CpalCuePlayer;
    type Input = Microphone;
    type Host = Desktop;

    async fn load(
        &self,
        config: &DesktopConfig,
        gate: &InputGate,
        publisher: &Rc<Publisher>,
    ) -> Result<Loaded<Self>, Failure> {
        let recorder =
            Recorder::per_user(config.recordings).map_err(|error| Failure::of(&error))?;
        let store = ModelStore::per_user().map_err(|error| Failure::of(&error))?;
        let started = Instant::now();
        let (wake, (spotter, spotter_off)) = tokio::join!(
            load_wake(
                publisher,
                &config.wake,
                &store,
                WakeEngine::load(&config.wake, &store),
            ),
            load_spotter(publisher, &config.endings, &store, &recorder, async {
                OnnxSpotter::load(&config.endings, &store)
                    .await
                    .map(|spotter| Box::new(spotter) as Box<dyn KeywordSpotter>)
            }),
        );
        recorder.metric(
            Event::WakeLoad,
            if wake.is_ok() { "ok" } else { "error" },
            Some(started.elapsed()),
            Value::Null,
        );
        let wake = wake.map_err(|error| Failure::of(&error))?;
        let metrics = recorder.metrics();
        let chosen = |entry, store| {
            Chosen::new(
                entry,
                store,
                self.http.clone(),
                metrics.clone(),
                &config.stt.phrases,
                self.devices.adapters(),
                &self.whisper_helpers,
            )
        };
        let entries = &config.stt.entries;
        let streams: Vec<_> = match entries.final_pass() {
            Some(thalvi_desktop::entries::FinalEntry::Live(entry))
                if !matches!(entry.kind, LiveKind::OpenAi(_)) =>
            {
                vec![entry]
            }
            Some(_) => Vec::new(),
            None => entries.streaming_extras().collect(),
        };
        let extras: Vec<_> = streams
            .into_iter()
            .map(|entry| StreamingExtra {
                entry: entry.clone(),
                recognizer: Arc::new(chosen(entry, store.clone())),
            })
            .collect();
        let extra_locals = extras
            .iter()
            .filter_map(|extra| extra.recognizer.local())
            .collect();
        let chat = (config.writing.as_ref())
            .and_then(thalvi_desktop::writing::Writing::on_computer)
            .map(|requested| thalvi_desktop::chat::Chat::on_computer(store.clone(), requested));
        let recognizer = chosen(config.stt.entries.active_live(), store);
        let local = recognizer.local();
        self.devices.use_recognizer(local.clone());
        Ok(Loaded {
            recognizer,
            wake,
            cues: CpalCuePlayer::new(config.cues, gate.clone()),
            recorder,
            http: self.http.clone(),
            chat,
            spotter,
            spotter_off,
            extras,
            local,
            extra_locals,
        })
    }

    fn input(&self, gate: InputGate, device: Option<String>, metrics: Metrics) -> Microphone {
        Microphone::new(gate, device, metrics)
    }
}

/// Everything one engine needs; replaced when settings change.
struct Session<S: Rig> {
    engine: Engine<S::Recognizer, S::Wake, S::Cues>,
    /// The local recognizer, whose model loads ahead of the first dictation.
    local: Option<Arc<dyn Preload>>,
    /// The on-device recognizers of the streaming extras, loaded ahead the same way.
    extra_locals: Vec<Arc<dyn Preload>>,
    handle: EngineHandle,
    gate: InputGate,
    device: Option<String>,
    metrics: Metrics,
    microphone: Option<S::Input>,
    frames: Option<Fed<<S::Input as Input>::Frames>>,
    feeder: Feeder,
}

impl<S: Rig> Session<S> {
    fn active_device(&self) -> Option<String> {
        self.frames.as_ref()?;
        self.microphone
            .as_ref()
            .and_then(|microphone| microphone.active_name().map(str::to_owned))
    }

    fn close_microphone(&mut self) {
        self.engine.stop_listening();
        self.feeder.cancel();
        self.frames = None;
        if let Some(microphone) = &mut self.microphone {
            microphone.pause();
        }
    }

    async fn open_microphone(&mut self, rig: &S) -> Result<(), Failure> {
        let microphone = self.microphone.take().unwrap_or_else(|| {
            rig.input(self.gate.clone(), self.device.clone(), self.metrics.clone())
        });
        let (microphone, frames) = microphone.open().await?;
        self.microphone = Some(microphone);
        self.frames = Some(self.feeder.wrap(frames?));
        Ok(())
    }

    /// Selects `device` for every later open and, while the microphone is open, opens it beside the
    /// open one and swaps them once it works. A device that cannot be opened leaves the open one
    /// running.
    async fn switch_microphone(&mut self, device: Option<String>) -> Result<(), Failure> {
        self.device.clone_from(&device);
        let Some(mut microphone) = self.microphone.take() else {
            return Ok(());
        };
        microphone.select(device);
        if self.frames.is_none() {
            self.microphone = Some(microphone);
            return Ok(());
        }
        let (microphone, frames) = microphone.replace().await?;
        self.microphone = Some(microphone);
        let frames = frames?;
        let presses = self.engine.stop_listening();
        self.frames = Some(self.feeder.wrap(frames));
        self.engine.resume_listening(presses);
        Ok(())
    }

    /// Flushes the recordings and closes the microphone.
    async fn close(mut self) {
        self.close_microphone();
        self.engine.finish().await;
    }
}

fn audio_thread_crashed(error: &tokio::task::JoinError) -> Failure {
    Failure {
        kind: FailureKind::Other,
        notice: "The microphone's audio thread crashed. Restart Thalvi.".to_owned(),
        headline: "Microphone crashed — restart Thalvi",
        causes: vec![error.to_string()],
    }
}

enum Flow {
    Continue,
    Quit,
}

enum Woke {
    Trigger(Result<WakeTrigger, thalvi_desktop::engine::ListenError>),
    Control(Option<Control>),
    /// A file playing with no microphone has ended, before it woke a capture.
    FileEnded,
}

/// What the app shares with the runner besides its controls.
struct Shared<S: Rig> {
    rig: S,
    last_writing_window: Arc<Mutex<Option<WindowApp>>>,
    /// Whether the hold key is down.
    key: watch::Receiver<KeyState>,
    history: Arc<History>,
    drafts: Arc<Drafts>,
    /// Whether a file's audio is being fed in place of the microphone's.
    feeding: Arc<watch::Sender<bool>>,
    /// Whether listening starts paused.
    paused: bool,
}

struct Runner<S: Rig> {
    config: DesktopConfig,
    session: Option<Session<S>>,
    paused: bool,
    stopping: bool,
    /// Set while listening wants a microphone it does not have: how many reopen attempts failed.
    reopen_failures: Option<u32>,
    /// Why the microphone could not be opened. Kept so a file played instead does not clear it.
    open_failure: Option<Failure>,
    /// Why the selected microphone could not be opened while another stays open.
    microphone_problem: Option<String>,
    /// Settings that arrived during a capture.
    pending: Option<DesktopConfig>,
    /// A refused hold key not yet shown.
    hold_problem: Option<String>,
    /// The app and title of the last window a capture was bound to. Settings offers it when it
    /// cannot read the window that was focused before it opened. The title never leaves this.
    last_writing_window: Arc<Mutex<Option<WindowApp>>>,
    control: mpsc::UnboundedReceiver<Control>,
    publisher: Rc<Publisher>,
    level: Arc<watch::Sender<f32>>,
    audio: watch::Sender<AudioStatus>,
    delivery: Arc<Delivery<S::Host>>,
    history: Arc<History>,
    drafts: Arc<Drafts>,
    feeding: Arc<watch::Sender<bool>>,
    rig: S,
    /// Loads the current engine's speech model.
    loading: Option<JoinHandle<()>>,
    /// The hold key, for presses the engine is not listening for.
    key: Presses,
    key_state: watch::Receiver<KeyState>,
}

impl<S: Rig> Runner<S> {
    fn new(
        config: DesktopConfig,
        delivery: Arc<Delivery<S::Host>>,
        control: mpsc::UnboundedReceiver<Control>,
        publisher: Rc<Publisher>,
        level: Arc<watch::Sender<f32>>,
        audio: watch::Sender<AudioStatus>,
        shared: Shared<S>,
    ) -> Self {
        Self {
            config,
            session: None,
            paused: shared.paused,
            stopping: false,
            reopen_failures: None,
            open_failure: None,
            microphone_problem: None,
            pending: None,
            hold_problem: None,
            last_writing_window: shared.last_writing_window,
            control,
            publisher,
            level,
            audio,
            delivery,
            history: shared.history,
            drafts: shared.drafts,
            feeding: shared.feeding,
            rig: shared.rig,
            loading: None,
            key: Presses::new(shared.key.clone()),
            key_state: shared.key,
        }
    }

    async fn run(mut self) {
        let config = self.config.clone();
        self.start(config).await;
        while let Flow::Continue = Box::pin(self.step()).await {}
        if let Some(loading) = self.loading.take() {
            loading.abort();
        }
        if let Some(session) = self.session.take() {
            session.close().await;
        }
        println!("Stopped.");
    }

    /// Builds the first engine, or fails the status with why it could not be built.
    async fn start(&mut self, config: DesktopConfig) {
        match self.build(&config).await {
            Ok(session) => {
                self.session = Some(session);
                self.load_speech_model();
                self.open_microphone().await;
            }
            Err(failure) => {
                self.fail(failure);
                self.publish_audio();
            }
        }
    }

    /// Fetches and loads the speech model in the background, and shows what it is doing until it is ready.
    /// A failure is a notice; the app keeps listening and dictation tries again.
    fn load_speech_model(&mut self) {
        if let Some(loading) = self.loading.take() {
            loading.abort();
        }
        let local = self
            .session
            .as_ref()
            .and_then(|session| session.local.clone());
        let extras = self
            .session
            .as_ref()
            .map(|session| session.extra_locals.clone())
            .unwrap_or_default();
        let first = local.as_ref().map(|local| *local.steps().borrow());
        if let Some(first) = first {
            println!("{}", load_notice(first));
        }
        self.publisher.loading(first);
        if local.is_none() && extras.is_empty() {
            return;
        }
        let publisher = Rc::clone(&self.publisher);
        self.loading = Some(tokio::task::spawn_local(async move {
            // An extra whose model cannot load fails per capture, with its own notice.
            let preload = async {
                for extra in &extras {
                    if let Err(error) = extra.load().await {
                        println!("{error}");
                    }
                }
            };
            match local {
                Some(local) => {
                    tokio::join!(load_showing(&local, &publisher), preload);
                }
                None => preload.await,
            }
        }));
    }

    /// Loads the model again after its download finished, unless the active entry does not use it or it is loaded.
    fn reload_speech_model(&mut self, downloaded: ListedSpeech) {
        let uses_downloaded = matches!(
            &self.config.stt.entries.active_live().kind,
            LiveKind::OnDevice(active) if downloaded.provides(active)
        );
        let local = self
            .session
            .as_ref()
            .and_then(|session| session.local.as_deref());
        if reload_wanted(uses_downloaded, local) {
            self.publisher.load_retried();
            self.load_speech_model();
        }
    }

    fn fail(&mut self, failure: Failure) {
        println!("{}", failure.notice);
        self.publisher.mode(Mode::Failed(failure));
    }

    async fn build(&self, config: &DesktopConfig) -> Result<Session<S>, Failure> {
        let gate = InputGate::default();
        let Loaded {
            recognizer,
            wake,
            cues,
            recorder,
            http,
            chat,
            spotter,
            spotter_off,
            extras,
            local,
            extra_locals,
        } = self.rig.load(config, &gate, &self.publisher).await?;
        let metrics = recorder.metrics();
        let (engine, handle) = Engine::new(
            Parts {
                recognizer,
                chat,
                wake,
                cues,
                gate: gate.clone(),
                http,
                hold: self.key_state.clone(),
                notice: self.publisher.notice_sink(),
                spotter,
                spotter_off,
                extras,
            },
            config,
            recorder,
        );
        tokio::task::spawn_local(follow(
            handle.state(),
            handle.level(),
            Rc::clone(&self.publisher),
            Arc::clone(&self.level),
            self.audio.subscribe(),
        ));
        println!("Transcribing through {}.", config.speech_source());
        let feeder = Feeder::new(&gate, Arc::clone(&self.feeding));
        Ok(Session {
            engine,
            local,
            extra_locals,
            handle,
            gate,
            device: config.microphone.clone(),
            metrics,
            microphone: None,
            frames: None,
            feeder,
        })
    }

    fn publish_audio(&self) {
        let next = AudioStatus {
            selected: self.config.microphone.clone(),
            active: self.session.as_ref().and_then(Session::active_device),
            paused: self.paused,
            problem: self.microphone_problem.clone(),
        };
        self.audio.send_if_modified(|current| {
            if *current == next {
                return false;
            }
            *current = next;
            true
        });
    }

    fn close_microphone(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.close_microphone();
        }
        self.microphone_problem = None;
        self.level.send_replace(0.0);
        self.publish_audio();
    }

    /// Opens the microphone unless listening is paused; a failure becomes the status.
    async fn open_microphone(&mut self) {
        let mut requests = Requests {
            listening_off: self.paused,
            hold_problem: self.hold_problem.take(),
            ..Requests::default()
        };
        requests.read_queued(&mut self.control);
        self.paused = requests.listening_off;
        self.stopping |= requests.quit;
        self.hold_problem = requests.hold_problem;
        if let Some(model) = requests.reload {
            self.reload_speech_model(model);
        }
        if let Some(config) = requests.pending
            && config != self.config
        {
            self.pending = Some(config);
        }
        if self.stopping || self.pending.is_some() {
            self.publish_audio();
            return;
        }
        if self.paused {
            self.publisher.mode(Mode::Paused);
            self.publish_audio();
            return;
        }
        if self.reopen_failures.is_none() {
            self.publisher.mode(Mode::Starting);
        }
        self.publish_audio();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match session.open_microphone(&self.rig).await {
            Ok(()) => {
                self.reopen_failures = None;
                self.open_failure = None;
                self.publisher.mode(Mode::Idle);
            }
            Err(failure) => self.microphone_failed(failure),
        }
        self.publish_audio();
    }

    /// Shows why listening has no microphone and schedules the next attempt to reopen it.
    fn microphone_failed(&mut self, failure: Failure) {
        if self.reopen_failures.is_none() {
            self.fail(failure.clone());
        } else {
            tracing::warn!(notice = failure.notice, "microphone still unavailable");
        }
        self.open_failure = Some(failure);
        self.reopen_failures = Some(self.reopen_failures.map_or(0, |failures| failures + 1));
    }

    /// A clip is the only input: the microphone is not open.
    fn feed_only(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.frames.is_some() && session.active_device().is_none())
    }

    /// Drops a clip's source once it is finished and puts the microphone failure back.
    fn end_unopened_feed(&mut self) {
        if self
            .session
            .as_ref()
            .and_then(Session::active_device)
            .is_some()
        {
            return;
        }
        if let Some(session) = self.session.as_mut() {
            session.gate.set_enabled(true);
        }
        self.close_microphone();
        if self.paused || self.stopping {
            return;
        }
        if let Some(failure) = self.open_failure.clone() {
            self.fail(failure);
        }
    }

    async fn step(&mut self) -> Flow {
        if self.stopping {
            return Flow::Quit;
        }
        self.show_hold_problem();
        if let Some(config) = self.pending.take() {
            self.apply(config).await;
            return Flow::Continue;
        }
        if self.feed_only() && !*self.feeding.borrow() {
            self.end_unopened_feed();
        }
        let listening = !self.paused
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.frames.is_some());
        if !listening {
            let retry = (!self.paused && self.session.is_some())
                .then_some(self.reopen_failures)
                .flatten()
                .map(backoff);
            self.key.skip();
            return match next(&mut self.control, &mut self.key, retry).await {
                Next::Control(Some(control)) => self.handle(control).await,
                Next::Control(None) => Flow::Quit,
                Next::Reopen => {
                    self.open_microphone().await;
                    Flow::Continue
                }
                Next::Key => {
                    let pressed = pressed(&mut self.key);
                    self.refuse_unheard(pressed);
                    Flow::Continue
                }
            };
        }
        let feed_only = self.feed_only();
        let woke = {
            let Self {
                session,
                control,
                feeding,
                ..
            } = &mut *self;
            let mut file_ended = feeding.subscribe();
            let Some(Session {
                engine,
                frames: Some(frames),
                ..
            }) = session.as_mut()
            else {
                return Flow::Continue;
            };
            if feed_only {
                // A ready wake is taken before the file ending; controls keep their turn over that ending.
                tokio::select! {
                    biased;
                    trigger = engine.listen(frames) => Woke::Trigger(trigger),
                    control = control.recv() => Woke::Control(control),
                    _ = file_ended.changed() => Woke::FileEnded,
                }
            } else {
                tokio::select! {
                    trigger = engine.listen(frames) => Woke::Trigger(trigger),
                    control = control.recv() => Woke::Control(control),
                }
            }
        };
        match woke {
            Woke::Control(Some(control)) => self.handle(control).await,
            Woke::Control(None) => Flow::Quit,
            Woke::FileEnded => {
                self.end_unopened_feed();
                Flow::Continue
            }
            Woke::Trigger(Ok(WakeTrigger::Capture | WakeTrigger::Hold)) => {
                Box::pin(self.capture()).await
            }
            Woke::Trigger(Err(error)) => {
                if self.feed_only() {
                    self.end_unopened_feed();
                    return Flow::Continue;
                }
                tracing::warn!(?error, "listening stopped");
                self.close_microphone();
                // A capture that failed with the microphone has said where its text went.
                if !self.publisher.failed() {
                    self.fail(Failure::of(&error));
                }
                self.reopen_failures = Some(0);
                Flow::Continue
            }
        }
    }

    /// Refuses `count` presses of the hold key that came while no engine was listening.
    fn refuse_unheard(&self, count: u64) {
        if count > 0 {
            let refusal = if self.paused {
                Refusal::Paused
            } else {
                Refusal::MicrophoneClosed
            };
            self.refuse_press(refusal, count);
        }
    }

    /// Says that `count` presses of the hold key started nothing, and why.
    fn refuse_press(&self, refusal: Refusal, count: u64) {
        if let Some(session) = &self.session {
            report_refusal(session.engine.recorder(), refusal, count);
        } else {
            println!("{}", refusal.notice());
        }
        self.publisher.refused(refusal);
    }

    /// Acts on a control message while nothing is being captured.
    async fn handle(&mut self, control: Control) -> Flow {
        match control {
            Control::SetListening(on) => self.set_listening(on).await,
            Control::ToggleListening => self.set_listening(self.paused).await,
            Control::Cancel => {
                if let Some(session) = &self.session {
                    session.feeder.cancel();
                }
            }
            Control::Feed(feed) => match self.session.as_mut() {
                // A missing microphone still takes the file. The device stays closed.
                Some(session) if !self.paused => {
                    if session.frames.is_none() {
                        session.frames = Some(session.feeder.without_microphone());
                    }
                    session.feeder.start(feed);
                }
                _ => feed.refuse("Thalvi is not listening."),
            },
            Control::Apply(config) => self.apply(*config).await,
            Control::HoldKeyFailed(notice) => self.hold_problem = Some(notice),
            Control::HoldKeyWorks => {
                self.hold_problem = None;
                if self.publisher.hold_failed() {
                    self.publisher.mode(Mode::Idle);
                }
            }
            Control::Overlay(failure) => self.overlay(failure),
            Control::ReloadSpeechModel(model) => self.reload_speech_model(model),
            Control::RemoveChat(reply) => {
                let result = match self.session.as_mut() {
                    Some(session) => session
                        .engine
                        .remove_chat()
                        .await
                        .map_err(|error| error.to_string()),
                    None => match ModelStore::per_user() {
                        Ok(store) => thalvi_desktop::chat::remove_model(
                            &store,
                            &thalvi_desktop::chat::Build::Cpu.files(),
                        )
                        .await
                        .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    },
                };
                let _ = reply.send(result);
            }
            Control::RestartIfIdle(restart) => {
                let held = self.key_state.borrow().down;
                return answer_restart(restart, self.paused, held, *self.feeding.borrow());
            }
            Control::Quit => return Flow::Quit,
        }
        Flow::Continue
    }

    /// Shows a refused hold key once nothing else is on the status: the failure stays until the
    /// next capture or resumed listening, like any other.
    /// Shows an overlay failure while the engine itself is fine, and clears it on recovery.
    fn overlay(&mut self, failure: Option<Failure>) {
        match failure {
            Some(failure) if !self.publisher.failed() => self.fail(failure),
            None if self.publisher.overlay_failed() => self.publisher.mode(Mode::Idle),
            _ => {}
        }
    }

    fn show_hold_problem(&mut self) {
        if self.paused || self.publisher.failed() {
            return;
        }
        if let Some(notice) = self.hold_problem.take() {
            self.fail(hold_failure(notice));
        }
    }

    async fn set_listening(&mut self, on: bool) {
        if !on {
            self.pause();
            return;
        }
        self.paused = false;
        self.publish_audio();
        let open = self
            .session
            .as_ref()
            .is_some_and(|session| session.frames.is_some());
        if open {
            self.publisher.mode(Mode::Idle);
        } else if self.session.is_some() {
            self.open_microphone().await;
        } else {
            self.start(self.config.clone()).await;
        }
    }

    fn pause(&mut self) {
        self.paused = true;
        self.reopen_failures = None;
        self.close_microphone();
        self.publisher.mode(Mode::Paused);
    }

    /// Brings the stored history within a changed limit now, rather than at the next capture.
    fn take_history(&self, config: &DesktopConfig) {
        if config.history != self.config.history {
            self.history.trim_in_background(config.history);
        }
    }

    /// Applies `config` to the running engine where possible. A failed rebuild keeps the old engine.
    async fn apply(&mut self, config: DesktopConfig) {
        if config == self.config {
            return;
        }
        // The engine is rebuilt with the microphone it has; a changed one is opened beside it afterwards.
        let wanted = config.microphone.clone();
        let rest = DesktopConfig {
            microphone: self.config.microphone.clone(),
            ..config
        };
        if rest != self.config {
            self.apply_engine(rest).await;
        }
        if wanted != self.config.microphone {
            self.config.microphone = wanted;
            self.switch_microphone().await;
        }
    }

    /// Opens the selected microphone beside the one open, or says why it could not and leaves the
    /// open one running.
    async fn switch_microphone(&mut self) {
        self.microphone_problem = None;
        if let Some(session) = self.session.as_mut()
            && let Err(failure) = session
                .switch_microphone(self.config.microphone.clone())
                .await
        {
            tracing::warn!(
                notice = failure.notice,
                "the selected microphone could not be opened"
            );
            self.microphone_problem = Some(failure.notice);
        }
        self.publish_audio();
    }

    async fn apply_engine(&mut self, config: DesktopConfig) {
        if retunes_only(&self.config, &config) {
            if let Some(session) = self.session.as_mut() {
                session.engine.set_recordings(config.recordings);
                session.engine.set_wake_words(config.wake.enabled());
            }
            self.publisher
                .update(|view| view.wake_words = config.wake.enabled());
            self.take_history(&config);
            self.config = config;
            return;
        }
        let mut presses = if let Some(session) = self.session.as_mut() {
            session.engine.stop_listening()
        } else {
            let mut presses = Presses::new(self.key_state.clone());
            presses.skip();
            presses
        };
        self.rebuild(config).await;
        let count = pressed(&mut presses);
        if let Some(session) = self.session.as_mut()
            && session.frames.is_some()
        {
            session.engine.resume_listening(presses);
        }
        self.refuse_unheard(count);
    }

    /// Closes the microphone and replaces the engine by one built from `config`.
    async fn rebuild(&mut self, config: DesktopConfig) {
        self.close_microphone();
        match self.build(&config).await {
            Ok(session) => {
                if let Some(old) = self.session.replace(session) {
                    old.close().await;
                }
                self.publisher.update(|view| {
                    view.pauses = config.ending_pause;
                    view.wake_words = config.wake.enabled();
                });
                self.take_history(&config);
                self.config = config;
                self.load_speech_model();
                self.open_microphone().await;
            }
            Err(failure) => {
                self.open_microphone().await;
                self.fail(failure);
            }
        }
    }

    /// Takes one capture from the wake word to its delivery.
    #[expect(
        clippy::too_many_lines,
        reason = "one capture's drive, settle and the notes it leaves are one sequence"
    )]
    async fn capture(&mut self) -> Flow {
        let bound = {
            let Some(session) = self.session.as_ref() else {
                return Flow::Continue;
            };
            self.delivery.bind(session.engine.recorder()).await
        };
        let window = self.delivery.writing_target(&bound);
        if let Some(window) = &window {
            *self
                .last_writing_window
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(window.clone());
        }
        self.publisher
            .woke(bound.window(), self.delivery.copies_only());
        let mut requests = Requests::default();
        let (finished, route, ignored, note) = {
            let Self {
                session,
                control,
                delivery,
                history,
                drafts,
                config,
                key,
                publisher,
                hold_problem,
                ..
            } = &mut *self;
            let Some(Session {
                engine,
                handle,
                gate,
                frames: Some(frames),
                feeder,
                ..
            }) = session.as_mut()
            else {
                return Flow::Continue;
            };
            requests.hold_problem = hold_problem.take();
            let outcome = Box::pin(drive(
                engine.capture(frames, window),
                control,
                || {
                    handle.cancel();
                    feeder.cancel();
                },
                &mut requests,
            ))
            .await;
            let mut ignored = engine.take_ignored();
            let note = engine.take_merge_note().or(engine.take_cleanup_note());
            match outcome {
                None => {
                    // The wake word closed the gate and only a capture opens it again.
                    // The control that ended it never reached the cancel callback.
                    gate.set_enabled(true);
                    feeder.cancel();
                    (None, None, ignored, None)
                }
                Some(outcome) => {
                    let route = match &outcome {
                        CaptureOutcome::Accepted(request) => Some(request.route),
                        _ => None,
                    };
                    if let (false, Some(route)) = (requests.ended, route) {
                        publisher.mode(Mode::Delivering(route));
                    }
                    let refused = RefCell::new(Vec::new());
                    let cleanup = engine.take_cleanup();
                    let finished = Settling {
                        delivery,
                        history,
                        drafts,
                        config,
                        recorder: engine.recorder(),
                    }
                    .settle(
                        &outcome,
                        cleanup.as_ref(),
                        requests.ended,
                        &bound,
                        key,
                        |refusal, count| {
                            report_refusal(engine.recorder(), refusal, count);
                            refused.borrow_mut().push(refusal);
                        },
                    )
                    .await;
                    let delivered_route = finished
                        .as_ref()
                        .filter(|finished| {
                            finished.text_delivery
                                == Some(thalvi_desktop::host::Delivered::InWindow)
                        })
                        .and(route);
                    let final_note = engine.settle_final_pass(
                        delivered_route,
                        requests.ended || matches!(outcome, CaptureOutcome::Cancelled { .. }),
                    );
                    ignored.extend(refused.into_inner());
                    let note = match (final_note, note) {
                        (Some(a), Some(b)) => Some(format!("{a} {b}")),
                        (a, b) => a.or(b),
                    };
                    (finished, route, ignored, note)
                }
            }
        };
        if let Some(config) = requests.pending.take() {
            self.pending = Some(config);
        }
        self.hold_problem = requests.hold_problem;
        if requests.quit {
            // Nothing is delivered after a quit; what was dictated is already kept.
            return Flow::Quit;
        }
        if let Some(model) = requests.reload {
            self.reload_speech_model(model);
        }
        self.publisher
            .mode(told(noted(delivered(finished, route), note), &ignored));
        if requests.listening_off {
            self.pause();
        }
        Flow::Continue
    }
}

/// What becomes of a finished capture: its delivery, unless a control ended the capture first,
/// and its place in the history and as the last draft either way.
struct Settling<'a, H: Host + 'static> {
    delivery: &'a Delivery<H>,
    history: &'a Arc<History>,
    drafts: &'a Drafts,
    config: &'a DesktopConfig,
    recorder: &'a Recorder,
}

impl<H: Host + 'static> Settling<'_, H> {
    /// Delivers `outcome` unless `ended`, calling `refuse` for each press of the hold key
    /// meanwhile, and keeps what was dictated once. An ended capture reaches no host and is kept
    /// as cancelled.
    async fn settle(
        &self,
        outcome: &CaptureOutcome,
        cleanup: Option<&(String, String)>,
        ended: bool,
        bound: &Bound<H::Target>,
        key: &mut Presses,
        refuse: impl Fn(Refusal, u64),
    ) -> Option<Finished> {
        let finished = if ended {
            None
        } else {
            // The presses so far were the capture's; a later one finds the user waiting.
            key.skip();
            while_delivering(
                self.delivery
                    .finish(outcome, bound, self.config, self.recorder),
                key,
                refuse,
            )
            .await
        };
        self.keep(outcome, cleanup, ended, finished.as_ref(), bound);
        finished
    }

    fn keep(
        &self,
        outcome: &CaptureOutcome,
        cleanup: Option<&(String, String)>,
        ended: bool,
        finished: Option<&Finished>,
        bound: &Bound<H::Target>,
    ) {
        let cancelled;
        let kept = match outcome {
            CaptureOutcome::Accepted(request) if ended => {
                cancelled = CaptureOutcome::Cancelled {
                    dictated: Some(request.text.clone()),
                };
                &cancelled
            }
            _ => outcome,
        };
        // A task's text is a command: the window it began in and a draft to resend mean nothing.
        let task = matches!(
            outcome,
            CaptureOutcome::Accepted(request) if request.route == CaptureRoute::Task
        );
        let window = (!task).then(|| bound.window()).flatten();
        if let Some(mut capture) = Capture::of(kept, finished, window) {
            if let Some((how, _)) = cleanup {
                capture.cleanup = Some(how.clone());
            }
            self.history
                .record_in_background(self.config.history, capture);
        }
        if !task
            && let Some(draft) =
                Draft::with_raw(kept, finished, bound, cleanup.map(|(_, raw)| raw.clone()))
        {
            self.drafts.offer(draft);
        }
    }
}

/// Adds what the hold key's ignored presses came to to the capture's final `mode`: a result stays
/// as it is, and the user is told besides.
fn told(mode: Mode, ignored: &[Refusal]) -> Mode {
    let Some(first) = ignored.first() else {
        return mode;
    };
    let mut distinct: Vec<Refusal> = Vec::new();
    for refusal in ignored {
        if !distinct.contains(refusal) {
            distinct.push(*refusal);
        }
    }
    let extra = distinct
        .iter()
        .map(|refusal| refusal.notice())
        .collect::<Vec<_>>()
        .join(" ");
    match mode {
        Mode::Done { route, notice } => Mode::Done {
            route,
            notice: format!("{notice} {extra}"),
        },
        Mode::Failed(failure) => Mode::Failed(Failure {
            notice: format!("{} {extra}", failure.notice),
            ..failure
        }),
        _ => Mode::Failed(Failure {
            kind: FailureKind::Other,
            notice: extra,
            headline: first.headline(),
            causes: Vec::new(),
        }),
    }
}

/// Adds what the merge told the user to the capture's final `mode`, which would otherwise hide it.
fn noted(mode: Mode, note: Option<String>) -> Mode {
    match (mode, note) {
        (Mode::Done { route, notice }, Some(note)) => Mode::Done {
            route,
            notice: format!("{note} {notice}"),
        },
        (Mode::Failed(failure), Some(note)) => Mode::Failed(Failure {
            notice: format!("{note} {}", failure.notice),
            ..failure
        }),
        (mode, _) => mode,
    }
}

/// What the status shows once a capture's delivery is over; nothing to show when it had none.
fn delivered(finished: Option<Finished>, route: Option<CaptureRoute>) -> Mode {
    match (finished, route) {
        (Some(finished), Some(route)) if finished.delivered => {
            println!("{}", finished.notice);
            Mode::Done {
                route,
                notice: finished.notice,
            }
        }
        (Some(finished), _) => {
            println!("{}", finished.notice);
            Mode::Failed(Failure {
                notice: finished.notice,
                headline: finished.headline,
                causes: finished.causes,
                kind: FailureKind::Delivery,
            })
        }
        (None, _) => Mode::Idle,
    }
}

/// The presses of the hold key since it was last looked at, however quickly they came and went.
fn pressed(key: &mut Presses) -> u64 {
    u64::from(key.take().is_some()) + key.take_lost()
}

/// Runs `delivery`, calling `refuse` for each press of the hold key meanwhile.
async fn while_delivering<T>(
    delivery: impl Future<Output = T>,
    key: &mut Presses,
    refuse: impl Fn(Refusal, u64),
) -> T {
    let mut delivery = pin!(delivery);
    loop {
        tokio::select! {
            biased;
            Ok(()) = key.changed() => {
                let pressed = pressed(key);
                if pressed > 0 {
                    refuse(Refusal::Delivering, pressed);
                }
            }
            finished = &mut delivery => return finished,
        }
    }
}

fn report_refusal(recorder: &Recorder, refusal: Refusal, count: u64) {
    refusal.record(recorder, count);
    println!("{}", refusal.notice());
}

/// The failure that tells the user the hold key is not working.
fn hold_failure(notice: String) -> Failure {
    Failure {
        kind: FailureKind::Other,
        causes: vec![notice.clone()],
        notice,
        headline: "Hold key unavailable",
    }
}

/// Whether `new` differs from `old` only in settings a running engine takes without a rebuild.
fn retunes_only(old: &DesktopConfig, new: &DesktopConfig) -> bool {
    *old == DesktopConfig {
        recordings: old.recordings,
        history: old.history,
        update_channel: old.update_channel,
        themes: old.themes.clone(),
        wake: new.wake.clone().with_enabled(old.wake.enabled()),
        ..new.clone()
    }
}

/// Answers a request to stop for an update. Only an idle engine agrees, with no hold key down and
/// no clip being fed, and only after `prepare` has saved what the next start needs; a refusal
/// leaves the engine running.
fn answer_restart(restart: Restart, paused: bool, held: bool, feeding: bool) -> Flow {
    let Restart { prepare, reply } = restart;
    if held || feeding {
        let _ = reply.send(Stop::Busy);
        return Flow::Continue;
    }
    match prepare(paused) {
        Ok(()) => {
            let _ = reply.send(Stop::Stopped);
            Flow::Quit
        }
        Err(message) => {
            let _ = reply.send(Stop::Refused(message));
            Flow::Continue
        }
    }
}

/// How long to wait before reopening a microphone that `failures` attempts could not open.
fn backoff(failures: u32) -> Duration {
    const STEPS: [u64; 3] = [1, 2, 5];
    Duration::from_secs(STEPS.get(failures as usize).copied().unwrap_or(5))
}

enum Next {
    Control(Option<Control>),
    Reopen,
    /// The hold key changed.
    Key,
}

/// Waits for the next control or hold key change, or until `retry` has passed.
async fn next(
    control: &mut mpsc::UnboundedReceiver<Control>,
    key: &mut Presses,
    retry: Option<Duration>,
) -> Next {
    let reopen = async {
        match retry {
            Some(delay) => tokio::time::sleep(delay).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        received = control.recv() => Next::Control(received),
        () = reopen => Next::Reopen,
        // A key that cannot change any more is no reason to wake.
        Ok(()) = key.changed() => Next::Key,
    }
}

/// What the user asked for while a capture ran.
#[derive(Default)]
struct Requests {
    /// The latest listening request was off.
    listening_off: bool,
    quit: bool,
    /// A control ended the capture, and nothing later undoes that: the host is not to be acted on.
    ended: bool,
    /// Settings to apply once the capture is over.
    pending: Option<DesktopConfig>,
    /// A refused hold key, to show once the capture is over.
    hold_problem: Option<String>,
    /// A model whose download finished, to load once the capture is over.
    reload: Option<ListedSpeech>,
}

impl Requests {
    /// Notes every message already waiting; true when one of them ends the capture.
    fn read_queued(&mut self, control: &mut mpsc::UnboundedReceiver<Control>) -> bool {
        let mut ends = false;
        while let Ok(received) = control.try_recv() {
            ends |= self.take(Some(received));
        }
        ends
    }

    /// Notes one control message; true when it ends the capture.
    fn take(&mut self, control: Option<Control>) -> bool {
        let ends = self.ends(control);
        self.ended |= ends;
        ends
    }

    fn ends(&mut self, control: Option<Control>) -> bool {
        match control {
            Some(Control::Cancel) => true,
            Some(Control::ToggleListening) => {
                self.listening_off = !self.listening_off;
                self.listening_off
            }
            Some(Control::SetListening(on)) => {
                self.listening_off = !on;
                !on
            }
            Some(Control::Apply(config)) => {
                self.pending = Some(*config);
                false
            }
            Some(Control::HoldKeyFailed(notice)) => {
                self.hold_problem = Some(notice);
                false
            }
            Some(Control::HoldKeyWorks) => {
                self.hold_problem = None;
                false
            }
            Some(Control::Overlay(_)) => false,
            Some(Control::RemoveChat(reply)) => {
                let _ = reply.send(Err(
                    "Wait for the current dictation to finish before removing the chat model."
                        .to_owned(),
                ));
                false
            }
            Some(Control::ReloadSpeechModel(model)) => {
                self.reload = Some(model);
                false
            }
            Some(Control::Feed(feed)) => {
                feed.refuse("Thalvi was busy. Try again in a moment.");
                false
            }
            Some(Control::RestartIfIdle(restart)) => {
                let _ = restart.reply.send(Stop::Busy);
                false
            }
            Some(Control::Quit) | None => {
                self.quit = true;
                true
            }
        }
    }
}

/// Runs `capture` while following the controls; `None` when a control ended it before it
/// started, so it never ran. Every ending control it consumed, even as the capture finished, is
/// kept in `requests.ended`. The engine ignores a cancel that arrives before it starts, so those
/// are read first, and the capture is polled before any later control.
async fn drive<O>(
    capture: impl Future<Output = O>,
    control: &mut mpsc::UnboundedReceiver<Control>,
    cancel: impl Fn(),
    requests: &mut Requests,
) -> Option<O> {
    if requests.read_queued(control) {
        return None;
    }
    let mut capture = pin!(capture);
    loop {
        tokio::select! {
            biased;
            outcome = &mut capture => {
                // What was asked while the capture wound down counts too.
                requests.read_queued(control);
                return Some(outcome);
            }
            received = control.recv(), if !requests.quit => {
                if requests.take(received) {
                    cancel();
                }
            }
        }
    }
}

/// Carries one engine's state and level changes to the publisher until the engine is dropped.
async fn follow(
    mut state: watch::Receiver<EngineState>,
    mut level: watch::Receiver<f32>,
    publisher: Rc<Publisher>,
    level_out: Arc<watch::Sender<f32>>,
    audio: watch::Receiver<AudioStatus>,
) {
    publisher.update(|view| view.engine = state.borrow_and_update().clone());
    loop {
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err() {
                    break;
                }
                let engine = state.borrow_and_update().clone();
                if let EngineState::Capturing { notice: Some(notice), .. } = &engine {
                    println!("{notice}");
                }
                publisher.update(|view| view.engine = engine);
            }
            changed = level.changed() => {
                if changed.is_err() {
                    break;
                }
                let next = *level.borrow_and_update();
                let current = audio.borrow();
                level_out.send_replace(if current.paused || current.active.is_none() { 0.0 } else { next });
            }
        }
    }
}

#[cfg(test)]
mod controls;

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::rc::Rc;
    use std::time::Duration;

    use thalvi_desktop::device::{Download, Export};
    use thalvi_desktop::local::{LocalRecognizer, ModelSource};
    use thalvi_desktop::models::{self, CatalogHead, CatalogSpeech};

    fn cpu_files(model: CatalogSpeech) -> Download {
        Download {
            export: Export::Cpu,
            size: model.assets().size(),
        }
    }

    fn gpu_files() -> Download {
        let Some(assets) = CatalogSpeech::Unified160ms.gpu_assets() else {
            panic!("Nemotron Unified has GPU files");
        };
        Download {
            export: Export::Gpu,
            size: assets.size(),
        }
    }

    use super::*;
    use crate::overlay::Visibility;
    use crate::tray::title;

    #[test]
    fn stopped_is_not_reported_until_background_writes_finish() {
        let (writing, started) = std_mpsc::channel();
        let (release, pending) = std_mpsc::channel();
        let (stopped, status) = std_mpsc::channel();
        let closing = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            drop(runtime.spawn_blocking(move || {
                writing.send(()).unwrap();
                pending.recv_timeout(Duration::from_secs(5)).unwrap();
            }));
            finish_runtime(runtime, &stopped);
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let premature = status.recv_timeout(Duration::from_millis(100));
        release.send(()).unwrap();
        closing.join().unwrap();
        assert!(
            matches!(premature, Err(std_mpsc::RecvTimeoutError::Timeout)),
            "quit acknowledged a pending write"
        );
        status.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_refused_press_tells_followers_even_when_the_failure_is_already_the_status() {
        let (out, mut status) = watch::channel(Status::Paused);
        let publisher = Publisher::new(out, EndingPauses::default());
        let cases = [
            (Refusal::Paused, "Paused — key ignored"),
            (Refusal::Paused, "Paused — key ignored"),
            (Refusal::MicrophoneClosed, "Paused — key ignored"),
        ];
        for (refusal, headline) in cases {
            status.borrow_and_update();
            publisher.refused(refusal);
            assert!(status.has_changed().unwrap(), "{refusal:?}");
            assert!(
                matches!(&*status.borrow(), Status::Failed { headline: shown, .. } if *shown == headline),
                "{refusal:?}"
            );
        }
    }

    #[test]
    fn a_desktop_that_only_copies_names_no_window_and_labels_the_countdown_a_copy() {
        for (copies, target) in [(true, None), (false, Some("kitty"))] {
            let (out, status) = watch::channel(Status::Paused);
            let publisher = Publisher::new(out, EndingPauses::default());
            publisher.mode(Mode::Idle);
            publisher.update(|view| {
                view.engine = EngineState::PendingEnding {
                    route: CaptureRoute::Submit,
                    text: "hi".to_owned(),
                    waiting_for_quiet: false,
                };
            });
            publisher.woke(Some("kitty".to_owned()), copies);
            assert!(
                matches!(
                    &*status.borrow(),
                    Status::Pending { target: shown, copies: counted, .. }
                        if shown.as_deref() == target && *counted == copies
                ),
                "{copies}: {:?}",
                *status.borrow()
            );
        }
    }

    #[tokio::test]
    async fn the_overlay_stays_hidden_while_a_model_on_disk_loads_and_shows_each_download_by_name()
    {
        let cpu = cpu_files(CatalogSpeech::Nemotron160ms);
        let gpu = gpu_files();
        let cases = [
            (
                "from disk",
                Step::Reading,
                false,
                "Loading the speech model.",
            ),
            (
                "the CPU files",
                Step::Fetching(cpu),
                true,
                "Loading the speech model. Downloading about 662 MB.",
            ),
            (
                "the GPU files",
                Step::Fetching(gpu),
                true,
                "Loading the speech model. Downloading the model for the GPU, about 1.3 GB.",
            ),
        ];
        let (out, status) = watch::channel(Status::Paused);
        let publisher = Publisher::new(out, EndingPauses::default());
        let shown = || Visibility::of(&status.borrow(), true) == Visibility::Show;

        publisher.mode(Mode::Starting);
        assert_eq!(title(&status.borrow()), "Thalvi: starting");
        assert!(!shown(), "starting the engine");
        publisher.mode(Mode::Idle);
        for (case, step, overlay, notice) in cases {
            publisher.loading(Some(step));
            assert_eq!(shown(), overlay, "{case}: loading the model");
            assert_eq!(title(&status.borrow()), "Thalvi: starting", "{case}");
            assert_eq!(
                *status.borrow(),
                Status::Loading {
                    notice: notice.to_owned(),
                    downloading: overlay
                },
                "{case}: the notice"
            );
        }

        publisher.loading(None);
        assert!(!shown(), "loaded");
        assert_eq!(title(&status.borrow()), "Thalvi: listening");
    }

    /// Takes the steps it is told to, one at a time, and then fails or loads.
    struct Scripted {
        gate: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        fails: bool,
    }

    struct NoModel;

    struct NoStream;

    impl thalvi_desktop::local::SpeechStream for NoStream {
        fn accept(
            &mut self,
            _frame: &thalvi_desktop::frame::Frame,
        ) -> Result<
            Vec<thalvi_desktop::recognizer::RecognizerEvent>,
            thalvi_desktop::recognizer::RecognizerError,
        > {
            Ok(Vec::new())
        }
    }

    impl thalvi_desktop::local::SpeechModel for NoModel {
        type Stream = NoStream;

        fn device(&self) -> thalvi_desktop::device::Device {
            thalvi_desktop::device::Device::Cpu(thalvi_desktop::device::Fallback::CpuModel)
        }

        fn stream(&self) -> NoStream {
            NoStream
        }
    }

    impl ModelSource for Scripted {
        type Model = NoModel;

        async fn load(
            &self,
            step: impl Fn(Step) + Send + Sync,
        ) -> Result<NoModel, thalvi_desktop::recognizer::RecognizerError> {
            step(Step::Fetching(gpu_files()));
            let gate = self.gate.lock().unwrap().take().unwrap();
            let _ = gate.await;
            step(Step::Fetching(cpu_files(CatalogSpeech::Unified160ms)));
            if self.fails {
                Err(thalvi_desktop::recognizer::RecognizerError::said(
                    "Could not download the speech model.",
                ))
            } else {
                Ok(NoModel)
            }
        }
    }

    #[tokio::test]
    async fn a_model_load_shows_each_step_it_takes_and_then_nothing_or_its_failure() {
        for (case, fails, mode, shown) in [
            ("loads", false, Mode::Idle, Mode::Idle),
            (
                "fails",
                true,
                Mode::Idle,
                Mode::Failed(Failure {
                    kind: FailureKind::Other,
                    notice: "Could not download the speech model. Dictation will try again when you start it."
                        .to_owned(),
                    headline: "Speech recognition failed — Settings",
                    causes: Vec::new(),
                }),
            ),
            ("fails while paused", true, Mode::Paused, Mode::Paused),
        ] {
            let (release, gate) = tokio::sync::oneshot::channel();
            let local = LocalRecognizer::new(
                Scripted {
                    gate: std::sync::Mutex::new(Some(gate)),
                    fails,
                },
                "test",
                Recorder::disabled().metrics(),
            );
            let (out, status) = watch::channel(Status::Paused);
            let publisher = Publisher::new(out, EndingPauses::default());
            publisher.mode(mode.clone());
            let load = load_showing(&local, &publisher);
            tokio::pin!(load);

            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    () = &mut load => panic!("{case}: finished before the GPU download was shown"),
                    () = async {
                        while publisher.view.borrow().loading != Some(Step::Fetching(gpu_files())) {
                            tokio::task::yield_now().await;
                        }
                    } => {}
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{case}: the GPU download is never shown"));
            if mode == Mode::Idle {
                let notice = "Loading the speech model. Downloading the model for the GPU, about 1.3 GB.";
                assert_eq!(
                    *status.borrow(),
                    Status::Loading { notice: notice.to_owned(), downloading: true },
                    "{case}: the step is shown"
                );
            }
            release.send(()).unwrap();
            load.await;
            assert_eq!(publisher.view.borrow().loading, None, "{case}: loading ends");
            assert_eq!(publisher.view.borrow().mode, shown, "{case}");
        }
    }

    /// Fails its first load and loads after that.
    struct FailsOnce(std::sync::atomic::AtomicBool);

    impl ModelSource for FailsOnce {
        type Model = NoModel;

        fn load(
            &self,
            _step: impl Fn(Step) + Send + Sync,
        ) -> impl Future<Output = Result<NoModel, thalvi_desktop::recognizer::RecognizerError>>
        {
            std::future::ready(if self.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Err(thalvi_desktop::recognizer::RecognizerError::said(
                    "Could not download the speech model.",
                ))
            } else {
                Ok(NoModel)
            })
        }
    }

    fn other_failure() -> Mode {
        Mode::Failed(Failure {
            kind: FailureKind::Other,
            notice: "No microphone.".to_owned(),
            headline: "No microphone",
            causes: Vec::new(),
        })
    }

    #[tokio::test]
    async fn a_download_after_a_failed_load_takes_the_failure_off_and_the_reload_shows_loading_then_idle()
     {
        let local = LocalRecognizer::new(
            FailsOnce(std::sync::atomic::AtomicBool::new(true)),
            "test",
            Recorder::disabled().metrics(),
        );
        let (out, status) = watch::channel(Status::Paused);
        let publisher = Publisher::new(out, EndingPauses::default());
        publisher.mode(Mode::Idle);
        load_showing(&local, &publisher).await;
        assert!(
            matches!(*status.borrow(), Status::Failed { .. }),
            "the load failed"
        );
        assert!(reload_wanted(true, Some(&local)), "the model is not loaded");

        publisher.load_retried();
        publisher.loading(Some(Step::Reading));
        assert!(
            matches!(*status.borrow(), Status::Loading { .. }),
            "the failure gives way to loading"
        );
        load_showing(&local, &publisher).await;
        assert_eq!(*status.borrow(), Status::Listening);
        assert!(!reload_wanted(true, Some(&local)), "a loaded model is kept");
    }

    #[tokio::test]
    async fn a_reload_takes_off_only_the_failure_a_model_load_showed() {
        for (case, then, shown) in [
            ("another failure", other_failure(), other_failure()),
            ("paused", Mode::Paused, Mode::Paused),
        ] {
            let local = LocalRecognizer::new(
                FailsOnce(std::sync::atomic::AtomicBool::new(true)),
                "test",
                Recorder::disabled().metrics(),
            );
            let (out, _status) = watch::channel(Status::Paused);
            let publisher = Publisher::new(out, EndingPauses::default());
            publisher.mode(Mode::Idle);
            load_showing(&local, &publisher).await;
            publisher.mode(then);
            publisher.load_retried();
            assert_eq!(publisher.view.borrow().mode, shown, "{case}");
        }
    }

    #[tokio::test]
    async fn a_finished_download_reloads_only_the_active_model_when_it_is_not_loaded() {
        let loaded = LocalRecognizer::new(
            FailsOnce(std::sync::atomic::AtomicBool::new(false)),
            "test",
            Recorder::disabled().metrics(),
        );
        loaded.load().await.unwrap();
        let unloaded = LocalRecognizer::new(
            FailsOnce(std::sync::atomic::AtomicBool::new(false)),
            "test",
            Recorder::disabled().metrics(),
        );
        for (case, uses_downloaded, local, expected) in [
            ("active and not loaded", true, Some(&unloaded), true),
            ("not the active model", false, Some(&unloaded), false),
            ("active and loaded", true, Some(&loaded), false),
            ("no on-device recognizer", true, None, false),
        ] {
            assert_eq!(reload_wanted(uses_downloaded, local), expected, "{case}");
        }
    }

    #[test]
    fn a_download_that_finishes_during_a_capture_is_kept_for_after_it() {
        let mut requests = Requests::default();
        let downloaded = ListedSpeech::Catalog(CatalogSpeech::ALL[0]);
        assert!(!requests.take(Some(Control::ReloadSpeechModel(downloaded))));
        assert_eq!(requests.reload, Some(downloaded));
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// The loading notice now, and whether the overlay shows.
    fn loading_now(status: &watch::Receiver<Status>) -> (String, bool) {
        let status = status.borrow();
        match &*status {
            Status::Loading { notice, .. } => (
                notice.clone(),
                Visibility::of(&status, true) == Visibility::Show,
            ),
            other => panic!("not loading: {other:?}"),
        }
    }

    /// Waits for the next status change, then returns [`loading_now`].
    async fn next_loading(status: &mut watch::Receiver<Status>) -> (String, bool) {
        tokio::time::timeout(WAIT, status.changed())
            .await
            .expect("the status changed")
            .expect("the publisher lives");
        loading_now(status)
    }

    /// Takes the steps it is sent, and loads when the sender is dropped.
    struct Streamed(std::sync::Mutex<Option<mpsc::UnboundedReceiver<Step>>>);

    impl ModelSource for Streamed {
        type Model = NoModel;

        async fn load(
            &self,
            step: impl Fn(Step) + Send + Sync,
        ) -> Result<NoModel, thalvi_desktop::recognizer::RecognizerError> {
            let mut steps = self.0.lock().unwrap().take().unwrap();
            while let Some(next) = steps.recv().await {
                step(next);
            }
            Ok(NoModel)
        }
    }

    /// A load of a model being downloaded, driven by the returned sender, and the status it publishes.
    fn loading_download() -> (
        mpsc::UnboundedSender<Step>,
        Rc<Publisher>,
        JoinHandle<()>,
        watch::Receiver<Status>,
    ) {
        let (out, mut status) = watch::channel(Status::Paused);
        let publisher = Publisher::new(out, EndingPauses::default());
        publisher.mode(Mode::Idle);
        publisher.loading(Some(Step::Fetching(cpu_files(
            CatalogSpeech::Nemotron160ms,
        ))));
        status.mark_unchanged();
        let (steps, received) = mpsc::unbounded_channel();
        let local = Arc::new(LocalRecognizer::new(
            Streamed(std::sync::Mutex::new(Some(received))),
            "test",
            Recorder::disabled().metrics(),
        ));
        let load = tokio::task::spawn_local({
            let publisher = Rc::clone(&publisher);
            async move { load_showing(&local, &publisher).await }
        });
        (steps, publisher, load, status)
    }

    #[tokio::test]
    async fn the_loading_notice_follows_a_download_once_per_percent_and_ends_with_the_model_loading()
     {
        LocalSet::new()
            .run_until(async {
                let (steps, _publisher, load, mut status) = loading_download();
                let cpu = cpu_files(CatalogSpeech::Nemotron160ms);
                let gpu = gpu_files();
                let mut shown = vec![loading_now(&status)];
                for (step, changes) in [
                    (Step::Downloading(cpu, 0), true),
                    (Step::Downloading(cpu, 0), false),
                    (Step::Downloading(cpu, 1), true),
                    (Step::Downloading(cpu, 100), true),
                    (Step::Reading, true),
                    (Step::Fetching(gpu), true),
                    (Step::Downloading(gpu, 50), true),
                    (Step::Reading, true),
                ] {
                    steps.send(step).unwrap();
                    if changes {
                        shown.push(next_loading(&mut status).await);
                    }
                }
                drop(steps);
                tokio::time::timeout(WAIT, load).await.unwrap().unwrap();

                let download = |text: &str| (text.to_owned(), true);
                let reading = ("Loading the speech model.".to_owned(), false);
                assert_eq!(
                    shown,
                    [
                        download("Loading the speech model. Downloading about 662 MB."),
                        download("Downloading the speech model, about 662 MB \u{2014} 0%"),
                        download("Downloading the speech model, about 662 MB \u{2014} 1%"),
                        download("Downloading the speech model, about 662 MB \u{2014} 100%"),
                        reading.clone(),
                        download(
                            "Loading the speech model. Downloading the model for the GPU, about 1.3 GB."
                        ),
                        download("Downloading the model for the GPU, about 1.3 GB \u{2014} 50%"),
                        reading,
                    ]
                );
                assert_eq!(*status.borrow(), Status::Listening);
            })
            .await;
    }

    #[tokio::test]
    async fn switching_away_mid_download_leaves_no_downloading_notice() {
        LocalSet::new()
            .run_until(async {
                let (steps, publisher, load, mut status) = loading_download();
                let cpu = cpu_files(CatalogSpeech::Nemotron160ms);
                steps.send(Step::Downloading(cpu, 40)).unwrap();
                assert_eq!(
                    next_loading(&mut status).await.0,
                    "Downloading the speech model, about 662 MB \u{2014} 40%"
                );

                load.abort();
                assert!(load.await.unwrap_err().is_cancelled());
                publisher.loading(None);
                let late = steps.send(Step::Downloading(cpu, 41));

                assert!(late.is_err(), "the aborted load no longer listens");
                assert_eq!(*status.borrow(), Status::Listening);
            })
            .await;
    }

    #[tokio::test]
    async fn the_overlay_shows_a_wake_download_only_while_a_missing_file_downloads() {
        let config = WakeConfig::default();
        let cached = [
            models::MELSPECTROGRAM,
            models::EMBEDDING,
            CatalogHead::HeyThalvi.asset(),
        ];
        let failed = Mode::Failed(Failure {
            kind: FailureKind::Other,
            notice: "Could not download the model.".to_owned(),
            headline: "Download failed",
            causes: Vec::new(),
        });
        // A retry after a failed download starts from Paused (the tray) or Failed (Settings).
        let starts = [Mode::Starting, Mode::Idle, Mode::Paused, failed];
        for (case, files, downloads) in [
            (
                "cached dictation assets with saved conversation head missing",
                cached.len(),
                false,
            ),
            ("missing dictation head", cached.len() - 1, true),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "thalvi-wake-load-{}-{}",
                case.replace(' ', "-"),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for asset in &cached[..files] {
                std::fs::File::create(dir.join(asset.file_name))
                    .unwrap()
                    .set_len(asset.size)
                    .unwrap();
            }
            let store = ModelStore::in_dir(&dir);
            for (start, load_fails) in starts.iter().flat_map(|mode| [(mode, false), (mode, true)])
            {
                let case = format!("{case}, from {start:?}, load fails: {load_fails}");
                let (out, status) = watch::channel(Status::Paused);
                let publisher = Publisher::new(out, EndingPauses::default());
                publisher.mode(start.clone());
                let before = status.borrow().clone();
                let download = Status::Loading {
                    notice: WAKE_DOWNLOAD_NOTICE.to_owned(),
                    downloading: true,
                };

                let result = load_wake(&publisher, &config, &store, async {
                    if downloads {
                        assert_eq!(*status.borrow(), download, "{case}: while loading");
                        assert_eq!(
                            Visibility::of(&status.borrow(), true),
                            Visibility::Show,
                            "{case}: overlay while loading"
                        );
                    } else {
                        assert_eq!(*status.borrow(), before, "{case}: nothing to announce");
                    }
                    if load_fails {
                        Err("no network")
                    } else {
                        Ok(())
                    }
                })
                .await;

                assert_eq!(result.is_err(), load_fails, "{case}");
                if downloads {
                    assert_eq!(title(&status.borrow()), "Thalvi: starting", "{case}: after");
                } else {
                    assert_eq!(*status.borrow(), before, "{case}: after");
                }
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_press_reaches_the_status_without_another_capture() {
        use thalvi_desktop::engine::fakes::{FakeCuePlayer, FakeFrames, FakeWake};
        use thalvi_desktop::recognizer::FakeRecognizer;

        LocalSet::new().run_until(async {
            let (out, mut status) = watch::channel(Status::Listening);
            let publisher = Publisher::new(out, EndingPauses::default());
            publisher.mode(Mode::Idle);
            let gate = InputGate::default();
            let (mut frames, _feed) = FakeFrames::new(&gate);
            let (wake, wake_control) = FakeWake::new();
            let (cues, _cue_control) = FakeCuePlayer::new();
            let (recognizer, _server) = FakeRecognizer::new();
            let (input, hold) = HoldInput::new();
            let (mut engine, _) = Engine::new(Parts {
                chat: None,
                recognizer, wake, cues, gate, hold,
                http: reqwest::Client::new(),
                notice: publisher.notice_sink(),
                spotter: None,
                spotter_off: None,
                extras: Vec::new(),
            }, &DesktopConfig::from_json_str("{}").unwrap(), Recorder::disabled());
            let listening = tokio::task::spawn_local(async move { engine.listen(&mut frames).await });
            tokio::time::timeout(Duration::from_secs(5), async {
                while wake_control.resets() == 0 { tokio::task::yield_now().await; }
            }).await.unwrap();
            input.set(true);
            input.set(false);
            tokio::time::timeout(Duration::from_secs(5), status.changed()).await.unwrap().unwrap();
            assert!(matches!(&*status.borrow(), Status::Failed { notice, .. } if notice == Refusal::TooQuick.notice()));
            listening.abort();
            let _ = listening.await;

            publisher.mode(Mode::Idle);
            publisher.update(|view| view.engine = EngineState::Capturing {
                committed: "keep speaking".to_owned(), partial: None,
                notice: None,
            });
            publisher.notice_sink()(Refusal::Capturing.notice());
            assert!(matches!(&*status.borrow(), Status::Capturing { committed, .. } if committed == "keep speaking"));
        }).await;
    }

    /// A capture that, like the engine's, forgets cancels sent before it starts and ends on the
    /// next one.
    async fn capture(cancels: &watch::Sender<u64>, started: &Cell<bool>) -> &'static str {
        let mut cancelled = cancels.subscribe();
        cancelled.borrow_and_update();
        started.set(true);
        let _ = cancelled.changed().await;
        "cancelled"
    }

    /// Runs `drive` after sending `before` and, once the capture runs, `during`.
    async fn run(
        before: Vec<Control>,
        during: Vec<Control>,
    ) -> (Option<&'static str>, Requests, bool) {
        let (sender, mut control) = mpsc::unbounded_channel();
        let (cancels, _) = watch::channel(0_u64);
        let started = Cell::new(false);
        let mut requests = Requests::default();
        for message in before {
            sender.send(message).unwrap();
        }
        let driven = drive(
            capture(&cancels, &started),
            &mut control,
            || {
                cancels.send_modify(|count| *count += 1);
            },
            &mut requests,
        );
        let sending = async {
            tokio::task::yield_now().await;
            for message in during {
                sender.send(message).unwrap();
            }
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(driven, sending)
        })
        .await
        .expect("the capture was never cancelled");
        (outcome, requests, started.get())
    }

    #[tokio::test(start_paused = true)]
    async fn a_control_that_ends_the_capture_before_it_starts_keeps_it_from_starting() {
        for (control, quit, listening_off) in [
            (Control::Quit, true, false),
            (Control::Cancel, false, false),
            (Control::SetListening(false), false, true),
            (Control::ToggleListening, false, true),
        ] {
            let label = format!("{control:?}");
            let (outcome, requests, started) = run(vec![control], vec![]).await;
            assert_eq!(outcome, None, "{label}");
            assert!(!started, "{label}");
            assert_eq!(requests.quit, quit, "{label}");
            assert_eq!(requests.listening_off, listening_off, "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_control_during_the_capture_cancels_it() {
        for (control, quit, listening_off) in [
            (Control::Quit, true, false),
            (Control::Cancel, false, false),
            (Control::SetListening(false), false, true),
            (Control::ToggleListening, false, true),
        ] {
            let label = format!("{control:?}");
            let (outcome, requests, started) = run(vec![], vec![control]).await;
            assert_eq!(outcome, Some("cancelled"), "{label}");
            assert!(started, "{label}");
            assert_eq!(requests.quit, quit, "{label}");
            assert_eq!(requests.listening_off, listening_off, "{label}");
        }
    }

    #[test]
    fn a_file_that_arrives_while_a_capture_runs_is_refused_and_ends_nothing() {
        let (feed, mut ended) = Feed::new(Clip::from_frames(Vec::new()));
        let mut requests = Requests::default();
        assert!(!requests.take(Some(Control::Feed(feed))));
        assert!(!requests.ended);
        assert_eq!(
            ended.try_recv(),
            Ok(FeedEnd::Refused("Thalvi was busy. Try again in a moment."))
        );
    }

    /// What an engine answers to a request to stop for an update, whether it quits, and what the
    /// request saved for the next start.
    fn restart_answer(
        paused: bool,
        (held, fed): (bool, bool),
        save_fails: bool,
    ) -> (Stop, bool, Vec<bool>) {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (reply, mut answer) = oneshot::channel();
        let restart = Restart::new(
            {
                let saved = Arc::clone(&saved);
                move |paused| {
                    saved.lock().unwrap().push(paused);
                    if save_fails {
                        Err("The restart note could not be saved.".to_owned())
                    } else {
                        Ok(())
                    }
                }
            },
            reply,
        );
        let flow = answer_restart(restart, paused, held, fed);
        let saved = saved.lock().unwrap().clone();
        (
            answer.try_recv().unwrap(),
            matches!(flow, Flow::Quit),
            saved,
        )
    }

    #[test]
    fn only_an_idle_engine_stops_for_an_update_after_saving_whether_it_was_paused() {
        let refused = || Stop::Refused("The restart note could not be saved.".to_owned());
        for (paused, held, fed, save_fails, answer, quits, saved) in [
            (false, false, false, false, Stop::Stopped, true, vec![false]),
            (true, false, false, false, Stop::Stopped, true, vec![true]),
            (false, true, false, false, Stop::Busy, false, vec![]),
            (true, false, true, false, Stop::Busy, false, vec![]),
            (false, false, false, true, refused(), false, vec![false]),
        ] {
            assert_eq!(
                restart_answer(paused, (held, fed), save_fails),
                (answer, quits, saved),
                "paused {paused}, held {held}, fed {fed}, save fails {save_fails}"
            );
        }
    }

    #[test]
    fn a_request_to_stop_for_an_update_during_a_capture_is_told_busy_and_ends_nothing() {
        let (reply, mut answer) = oneshot::channel();
        let mut requests = Requests::default();
        let restart = Restart::new(|_| panic!("a busy engine saves nothing"), reply);
        assert!(!requests.take(Some(Control::RestartIfIdle(restart))));
        assert!(!requests.ended && !requests.quit);
        assert_eq!(answer.try_recv(), Ok(Stop::Busy));
    }

    #[test]
    fn desktop_only_settings_and_the_wake_switch_do_not_rebuild_the_engine() {
        let old = DesktopConfig::from_json_str("{}").unwrap();
        for (change, rebuilds) in [
            (r#"{"recordings": {"text": true}}"#, false),
            (r#"{"update_channel": "development"}"#, false),
            (r#"{"history": {"enabled": false}}"#, false),
            (
                r#"{"themes":{"version":1,"gunmetal":{"font":{"scale":1.25}}}}"#,
                false,
            ),
            (
                r#"{"history": {"limit": 5}, "recordings": {"audio": true}}"#,
                false,
            ),
            // The wake switch is a retune: the model stays loaded and the microphone open.
            (r#"{"wake": {"enabled": false}}"#, false),
            (r#"{"wake": {"enabled": false}, "look": "neon"}"#, true),
            (r#"{"wake": {"capture": {"model": "hey_jarvis"}}}"#, true),
            (r#"{"look": "neon"}"#, true),
            (r#"{"history": {"limit": 5}, "look": "neon"}"#, true),
        ] {
            let new = DesktopConfig::from_json_str(change).unwrap();
            assert_eq!(!retunes_only(&old, &new), rebuilds, "{change}");
        }
    }

    #[test]
    fn a_missing_microphone_is_retried_after_one_two_then_every_five_seconds() {
        let waits: Vec<u64> = (0..6).map(|failures| backoff(failures).as_secs()).collect();
        assert_eq!(waits, [1, 2, 5, 5, 5, 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_microphone_is_reopened_when_its_time_comes_unless_a_control_comes_first() {
        let (sender, mut control) = mpsc::unbounded_channel();
        let started = tokio::time::Instant::now();

        let (_input, state) = HoldInput::new();
        let mut key = Presses::new(state);
        let reopen = tokio::time::timeout(
            Duration::from_secs(10),
            next(&mut control, &mut key, Some(Duration::from_secs(2))),
        )
        .await
        .expect("the microphone was never reopened");
        assert!(matches!(reopen, Next::Reopen));
        assert_eq!(started.elapsed(), Duration::from_secs(2));

        sender.send(Control::Quit).unwrap();
        let quit = next(&mut control, &mut key, Some(Duration::from_secs(2))).await;
        assert!(matches!(quit, Next::Control(Some(Control::Quit))));
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(2),
            "a control does not wait"
        );

        let waiting =
            tokio::time::timeout(Duration::from_secs(60), next(&mut control, &mut key, None)).await;
        assert!(
            waiting.is_err(),
            "without a retry only a control ends the wait"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_of_the_hold_key_wakes_the_wait_that_refuses_it_but_a_release_does_not() {
        let (_sender, mut control) = mpsc::unbounded_channel();
        let (input, state) = HoldInput::new();
        let mut key = Presses::new(state);
        input.set(true);
        let pressed =
            tokio::time::timeout(Duration::from_secs(60), next(&mut control, &mut key, None)).await;
        assert!(matches!(pressed, Ok(Next::Key)), "a press wakes the wait");
        assert_eq!(key.take().map(|press| press.number), Some(1));
        input.set(true);
        input.set(false);
        let released =
            tokio::time::timeout(Duration::from_secs(60), next(&mut control, &mut key, None)).await;
        assert!(
            matches!(released, Ok(Next::Key)),
            "a release wakes the wait too"
        );
        assert!(key.take().is_none(), "but it is no press to refuse");
    }

    #[tokio::test(start_paused = true)]
    async fn each_press_while_a_delivery_runs_is_refused_once_and_a_repeat_or_earlier_press_is_not()
    {
        let (input, state) = HoldInput::new();
        let mut key = Presses::new(state);
        // Pressed during the capture: the engine's, not a refusal.
        input.set(true);
        key.skip();
        let refused = Cell::new(0);
        let delivery = async {
            input.set(true);
            tokio::task::yield_now().await;
            input.set(false);
            tokio::task::yield_now().await;
            input.set(true);
            input.set(true);
            tokio::task::yield_now().await;
            "delivered"
        };
        // The held key is released first, so the next press is new.
        input.set(false);
        let outcome = while_delivering(delivery, &mut key, |refusal, count| {
            assert_eq!(refusal, Refusal::Delivering);
            refused.set(refused.get() + count);
        })
        .await;
        assert_eq!(outcome, "delivered");
        assert_eq!(refused.get(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_refused_after_a_rebuild_does_not_dictate_later_but_a_fresh_press_does() {
        use thalvi_desktop::engine::fakes::{FakeCuePlayer, FakeFrames, FakeWake};
        use thalvi_desktop::recognizer::FakeRecognizer;

        LocalSet::new()
            .run_until(async {
                let (out, _status) = watch::channel(Status::Listening);
                let publisher = Publisher::new(out, EndingPauses::default());
                let gate = InputGate::default();
                let (mut frames, _feed) = FakeFrames::new(&gate);
                let (wake, wake_control) = FakeWake::new();
                let (cues, _cue_control) = FakeCuePlayer::new();
                let (recognizer, _server) = FakeRecognizer::new();
                let (input, hold) = HoldInput::new();
                let key = hold.clone();
                let (mut engine, _) = Engine::new(
                    Parts {
                        chat: None,
                        recognizer,
                        wake,
                        cues,
                        gate,
                        hold,
                        http: reqwest::Client::new(),
                        notice: publisher.notice_sink(),
                        spotter: None,
                        spotter_off: None,
                        extras: Vec::new(),
                    },
                    &DesktopConfig::from_json_str("{}").unwrap(),
                    Recorder::disabled(),
                );

                let mut presses = Presses::new(key);
                presses.skip();
                input.set(true);
                assert_eq!(
                    pressed(&mut presses),
                    1,
                    "the press during the rebuild is refused"
                );
                engine.resume_listening(presses);

                let listening =
                    tokio::task::spawn_local(async move { engine.listen(&mut frames).await });
                tokio::time::timeout(Duration::from_secs(5), async {
                    while wake_control.resets() == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                assert!(
                    !listening.is_finished(),
                    "the refused press must not start a dictation"
                );

                input.set(false);
                input.set(true);
                let trigger = tokio::time::timeout(Duration::from_secs(5), listening)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(trigger, Ok(WakeTrigger::Hold)),
                    "a fresh press dictates"
                );
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_and_release_during_a_delivery_is_refused_not_lost() {
        let (input, state) = HoldInput::new();
        let mut key = Presses::new(state);
        key.skip();
        let refused = Cell::new(0);
        let delivery = async {
            // Both land before the delivery wait looks at the key.
            input.set(true);
            input.set(false);
            tokio::task::yield_now().await;
            "delivered"
        };
        while_delivering(delivery, &mut key, |refusal, count| {
            assert_eq!(refusal, Refusal::Delivering);
            refused.set(refused.get() + count);
        })
        .await;
        assert_eq!(refused.get(), 1);
        assert_eq!(key.take_lost(), 0, "it was counted once, not left over");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_that_arrives_as_the_capture_finishes_keeps_its_outcome_from_delivery() {
        let config = DesktopConfig::from_json_str("{}").unwrap();
        for (control, delivered) in [
            (None, true),
            (Some(Control::Apply(Box::new(config))), true),
            (Some(Control::Cancel), false),
            (Some(Control::SetListening(false)), false),
            (Some(Control::ToggleListening), false),
            (Some(Control::Quit), false),
        ] {
            let label = format!("{control:?}");
            let (sender, mut control_rx) = mpsc::unbounded_channel();
            let mut requests = Requests::default();
            // The capture finishes on its second poll, after the control was sent.
            let finishing = async {
                tokio::task::yield_now().await;
                "accepted"
            };
            let driven = drive(finishing, &mut control_rx, || {}, &mut requests);
            let sending = async {
                if let Some(control) = control {
                    sender.send(control).unwrap();
                }
            };
            let (outcome, ()) = tokio::join!(driven, sending);
            assert_eq!(outcome, Some("accepted"), "{label}: the outcome is kept");
            assert_eq!(requests.ended, !delivered, "{label}");
        }
    }

    /// Runs `drive` over a capture that, once running, waits for `release` before it finishes
    /// as accepted: like the engine closing its recognizer after the accepted cue. `during` is
    /// sent once the capture runs, and `release` follows once the first ending control among
    /// them was consumed.
    async fn consumed_while_closing(during: Vec<Control>) -> (Option<&'static str>, Requests) {
        let (sender, mut control) = mpsc::unbounded_channel();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (consumed_tx, consumed) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let consumed_tx = std::sync::Mutex::new(Some(consumed_tx));
        let mut requests = Requests::default();
        let capture = async {
            entered_tx.send(()).unwrap();
            released.await.unwrap();
            "accepted"
        };
        let driven = drive(
            capture,
            &mut control,
            || {
                if let Some(consumed) = consumed_tx.lock().unwrap().take() {
                    consumed.send(()).unwrap();
                }
            },
            &mut requests,
        );
        let ordering = async {
            entered.await.unwrap();
            for message in during {
                sender.send(message).unwrap();
            }
            consumed.await.unwrap();
            release.send(()).unwrap();
            // Disconnecting is a Quit of its own; keep the channel open until `drive` is done.
            sender
        };
        let (outcome, _sender) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(driven, ordering)
        })
        .await
        .expect("the capture never finished");
        (outcome, requests)
    }

    #[tokio::test(start_paused = true)]
    async fn an_ending_control_consumed_before_the_capture_finishes_still_keeps_it_from_the_host() {
        // (what was sent while the capture ran, quit, the latest listening request is off)
        for (during, quit, listening_off) in [
            (vec![Control::Cancel], false, false),
            (vec![Control::SetListening(false)], false, true),
            (vec![Control::ToggleListening], false, true),
            (vec![Control::Quit], true, false),
            // Resuming changes what listening does next, not what became of this capture.
            (
                vec![Control::SetListening(false), Control::SetListening(true)],
                false,
                false,
            ),
            (
                vec![Control::ToggleListening, Control::ToggleListening],
                false,
                false,
            ),
            (
                vec![Control::Cancel, Control::SetListening(false)],
                false,
                true,
            ),
        ] {
            let label = format!("{during:?}");
            let (outcome, requests) = consumed_while_closing(during).await;
            assert_eq!(outcome, Some("accepted"), "{label}: its text is kept");
            assert!(requests.ended, "{label}");
            assert_eq!(requests.quit, quit, "{label}");
            assert_eq!(requests.listening_off, listening_off, "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_nothing_ended_is_delivered_even_with_settings_and_a_resume_sent() {
        let config = DesktopConfig::from_json_str("{}").unwrap();
        for during in [
            vec![],
            vec![Control::Apply(Box::new(config))],
            vec![Control::SetListening(true)],
        ] {
            let label = format!("{during:?}");
            let (sender, mut control) = mpsc::unbounded_channel();
            let (release, gate) = tokio::sync::oneshot::channel();
            let mut requests = Requests::default();
            let driven = drive(
                async {
                    gate.await.unwrap();
                    "accepted"
                },
                &mut control,
                || panic!("{label}: nothing asked to cancel"),
                &mut requests,
            );
            let sending = async {
                for message in during {
                    sender.send(message).unwrap();
                }
                tokio::task::yield_now().await;
                release.send(()).unwrap();
                sender
            };
            let (outcome, _sender) = tokio::join!(driven, sending);
            assert_eq!(outcome, Some("accepted"), "{label}");
            assert!(!requests.ended, "{label}");
        }
    }

    /// The window and the calls a host saw.
    #[derive(Clone, Default)]
    pub(super) struct TestHost(
        pub(super) Arc<Mutex<Vec<&'static str>>>,
        pub(super) Arc<Mutex<Vec<String>>>,
    );

    impl TestHost {
        fn called(&self, call: &'static str) {
            self.0.lock().unwrap().push(call);
        }
    }

    impl Host for TestHost {
        type Target = String;

        fn copies_only(&self) -> bool {
            false
        }

        fn is_terminal(&self, _: &String) -> bool {
            false
        }

        fn writing_target(
            &self,
            target: &String,
        ) -> Result<thalvi_desktop::host::WindowApp, thalvi_desktop::host::AppMatchLimit> {
            Ok(thalvi_desktop::host::WindowApp::new(
                target.clone(),
                String::new(),
            ))
        }

        fn overlay_takes_clicks(&self) -> bool {
            false
        }

        fn recent_app(&self) -> Result<Option<WindowApp>, thalvi_desktop::host::AppMatchLimit> {
            Ok(Some(WindowApp::new("Editor", "")))
        }

        fn capture_target(&self) -> Result<String, thalvi_desktop::host::HostError> {
            Ok("Editor".to_owned())
        }

        fn deliver(
            &self,
            _: &String,
            text: &thalvi_desktop::host::PasteText,
            _: bool,
        ) -> Result<thalvi_desktop::host::Delivered, thalvi_desktop::host::HostError> {
            self.called("deliver");
            self.1.lock().unwrap().push(text.as_str().to_owned());
            Ok(thalvi_desktop::host::Delivered::InWindow)
        }

        fn copy(&self, text: &str) -> Result<(), thalvi_desktop::host::HostError> {
            self.called("copy");
            self.1.lock().unwrap().push(text.to_owned());
            Ok(())
        }

        fn focus_app(
            &self,
            _: &thalvi_desktop::host::App,
        ) -> Result<String, thalvi_desktop::host::HostError> {
            self.called("focus_app");
            Ok("App".to_owned())
        }

        fn open_url(
            &self,
            _: &thalvi_desktop::host::WebUrl,
        ) -> Result<(), thalvi_desktop::host::HostError> {
            self.called("open_url");
            Ok(())
        }

        fn open_folder(&self, _: &std::path::Path) -> Result<(), thalvi_desktop::host::HostError> {
            self.called("open_folder");
            Ok(())
        }
    }

    fn request(route: CaptureRoute, text: &str) -> CaptureOutcome {
        CaptureOutcome::Accepted(thalvi_desktop::capture::CaptureRequest {
            route,
            text: text.to_owned(),
        })
    }

    /// Settles `outcome` as `ended` and returns what the host, the history and the draft saw.
    async fn settled(
        outcome: &CaptureOutcome,
        ended: bool,
        expected_entries: usize,
    ) -> (
        Option<Finished>,
        Vec<&'static str>,
        Vec<thalvi_desktop::history::Entry>,
        Option<thalvi_desktop::draft::DraftView>,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "thalvi-runner-test-{}-{}",
            std::process::id(),
            ended
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let host = TestHost::default();
        let delivery = Delivery::new(host.clone());
        let recorder = Recorder::disabled();
        let history = Arc::new(History::at(dir.join("history.json")));
        let drafts = Drafts::default();
        let config = DesktopConfig::from_json_str("{}").unwrap();
        let bound = delivery.bind(&recorder).await;
        let (_input, state) = HoldInput::new();
        let mut key = Presses::new(state);
        let finished = Settling {
            delivery: &delivery,
            history: &history,
            drafts: &drafts,
            config: &config,
            recorder: &recorder,
        }
        .settle(outcome, None, ended, &bound, &mut key, |_, _| {})
        .await;
        // The history is written on a blocking thread: wait for what it will hold.
        let entries = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let entries = history.list().unwrap();
                if entries.len() >= expected_entries {
                    return entries;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the history entry was never written");
        let _ = std::fs::remove_dir_all(&dir);
        let calls = host.0.lock().unwrap().clone();
        (finished, calls, entries, drafts.view())
    }

    #[tokio::test]
    async fn a_capture_with_text_is_kept_once_and_reaches_no_host_when_a_control_ended_it() {
        use thalvi_desktop::draft::DraftOutcome;
        use thalvi_desktop::history::Outcome;

        // (outcome, ended, host calls, history outcome, window, draft outcome)
        type Case = (
            CaptureOutcome,
            bool,
            Vec<&'static str>,
            Outcome,
            Option<&'static str>,
            Option<DraftOutcome>,
        );
        let cases: Vec<Case> = vec![
            (
                request(CaptureRoute::Paste, "hello world"),
                false,
                vec!["deliver"],
                Outcome::Accepted,
                Some("Editor"),
                Some(DraftOutcome::Delivered),
            ),
            (
                request(CaptureRoute::Paste, "hello world"),
                true,
                vec![],
                Outcome::Cancelled,
                Some("Editor"),
                Some(DraftOutcome::Cancelled),
            ),
            (
                request(CaptureRoute::Submit, "hello world"),
                true,
                vec![],
                Outcome::Cancelled,
                Some("Editor"),
                Some(DraftOutcome::Cancelled),
            ),
            // A task's text is a command: kept in the history, never offered as a draft.
            (
                request(CaptureRoute::Task, "open the docs"),
                true,
                vec![],
                Outcome::Cancelled,
                None,
                None,
            ),
            (
                CaptureOutcome::Cancelled {
                    dictated: Some("hello world".to_owned()),
                },
                true,
                vec![],
                Outcome::Cancelled,
                Some("Editor"),
                Some(DraftOutcome::Cancelled),
            ),
            // The clipboard is a host action too: a failed capture's text is not copied.
            (
                CaptureOutcome::Failed {
                    error: thalvi_desktop::engine::CaptureError::MicStopped,
                    recoverable_text: Some("hello world".to_owned()),
                },
                true,
                vec![],
                Outcome::Failed,
                Some("Editor"),
                Some(DraftOutcome::Failed),
            ),
        ];
        for (case, (outcome, ended, calls, history, window, draft)) in cases.into_iter().enumerate()
        {
            let label = format!("case {case}: {outcome:?}, ended {ended}");
            let (finished, seen, entries, view) = settled(&outcome, ended, 1).await;
            assert_eq!(finished.is_some(), !ended, "{label}");
            assert_eq!(seen, calls, "{label}");
            assert_eq!(entries.len(), 1, "{label}");
            assert_eq!(entries[0].outcome, history, "{label}");
            assert_eq!(entries[0].target.as_deref(), window, "{label}");
            assert_eq!(view.as_ref().map(|view| view.outcome), draft, "{label}");
            if let Some(view) = view {
                assert_eq!(view.id, 1, "{label}: offered once");
                assert_eq!(view.original, "hello world", "{label}");
            }
        }
    }

    #[test]
    fn only_a_capture_that_was_not_delivered_fails_as_a_delivery() {
        let finished = |delivered| Finished {
            delivered,
            text_delivery: None,
            maybe_partial: false,
            notice: "Done.".to_owned(),
            headline: "Done",
            causes: Vec::new(),
            task_target: None,
        };
        assert_eq!(
            delivered(Some(finished(false)), Some(CaptureRoute::Paste)),
            Mode::Failed(Failure {
                notice: "Done.".to_owned(),
                headline: "Done",
                causes: Vec::new(),
                kind: FailureKind::Delivery,
            }),
        );
        assert_eq!(
            delivered(Some(finished(true)), Some(CaptureRoute::Paste)),
            Mode::Done {
                route: CaptureRoute::Paste,
                notice: "Done.".to_owned(),
            },
        );
    }

    #[test]
    fn what_the_merge_told_is_kept_beside_the_result_and_not_lost_to_it() {
        let done = Mode::Done {
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
        };
        let failure = Failure {
            kind: FailureKind::Delivery,
            notice: "It failed.".to_owned(),
            headline: "Failed",
            causes: Vec::new(),
        };
        let note =
            || Some("Merge skipped: the chat model took too long. Used the live text.".to_owned());
        assert_eq!(
            noted(done.clone(), note()),
            Mode::Done {
                route: CaptureRoute::Paste,
                notice:
                    "Merge skipped: the chat model took too long. Used the live text. Delivered."
                        .to_owned(),
            }
        );
        assert_eq!(
            noted(Mode::Failed(failure.clone()), note()),
            Mode::Failed(Failure {
                notice:
                    "Merge skipped: the chat model took too long. Used the live text. It failed."
                        .to_owned(),
                ..failure
            })
        );
        assert_eq!(
            noted(done.clone(), None),
            done,
            "nothing told, nothing added"
        );
        assert_eq!(
            noted(Mode::Idle, note()),
            Mode::Idle,
            "a cancelled capture shows nothing"
        );
    }

    #[test]
    fn presses_that_started_nothing_are_told_without_replacing_the_result() {
        let failure = |notice: &str| Failure {
            kind: FailureKind::Other,
            notice: notice.to_owned(),
            headline: "Nothing was pasted",
            causes: vec!["cause".to_owned()],
        };
        let done = Mode::Done {
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
        };
        assert_eq!(
            told(done.clone(), &[]),
            done,
            "nothing ignored, nothing told"
        );
        assert_eq!(
            told(
                done,
                &[Refusal::Delivering, Refusal::Capturing, Refusal::Delivering]
            ),
            Mode::Done {
                route: CaptureRoute::Paste,
                notice: format!(
                    "Delivered. {} {}",
                    Refusal::Delivering.notice(),
                    Refusal::Capturing.notice()
                ),
            },
            "each reason once, the delivery still reported"
        );
        assert_eq!(
            told(Mode::Failed(failure("It failed.")), &[Refusal::TooQuick]),
            Mode::Failed(Failure {
                kind: FailureKind::Other,
                notice: format!("It failed. {}", Refusal::TooQuick.notice()),
                ..failure("")
            }),
        );
        assert_eq!(
            told(Mode::Idle, &[Refusal::Capturing]),
            Mode::Failed(Failure {
                kind: FailureKind::Other,
                notice: Refusal::Capturing.notice().to_owned(),
                headline: "Dictating — key ignored",
                causes: Vec::new(),
            }),
            "a cancelled capture has no result to add to"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_latest_listening_request_wins_and_settings_wait_for_the_end() {
        let config = DesktopConfig::from_json_str("{}").unwrap();
        let (outcome, requests, _) = run(
            vec![],
            vec![
                Control::SetListening(false),
                Control::Apply(Box::new(config.clone())),
                Control::SetListening(true),
            ],
        )
        .await;
        assert_eq!(outcome, Some("cancelled"));
        assert!(!requests.listening_off, "the later resume is the request");
        assert_eq!(requests.pending, Some(config));

        let (_, requests, _) = run(
            vec![],
            vec![Control::SetListening(true), Control::SetListening(false)],
        )
        .await;
        assert!(requests.listening_off);
    }

    #[tokio::test(start_paused = true)]
    async fn changed_writing_openings_wait_for_the_end_of_the_capture_and_then_rebuild_the_engine()
    {
        let with = |openings: &str| {
            DesktopConfig::from_json_str(&format!(
                r#"{{"writing": {{"url": "http://127.0.0.1:9/v1", "model": "m", "openings": {openings}}}}}"#
            ))
            .unwrap()
        };
        let running = with(r#"[{"phrase": "write me an email", "prompt": "Old."}]"#);
        let changed = with(r#"[{"phrase": "write me an email", "prompt": "New."}]"#);
        let (outcome, requests, started) = run(
            vec![],
            vec![Control::Apply(Box::new(changed.clone())), Control::Cancel],
        )
        .await;
        assert!(started);
        assert_eq!(outcome, Some("cancelled"));
        assert_eq!(
            requests.pending,
            Some(changed.clone()),
            "the capture goes on with the rules it started with"
        );
        assert!(!retunes_only(&running, &changed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_switch_of_the_live_entry_waits_for_the_end_of_the_running_capture() {
        let running = DesktopConfig::from_json_str(
            r#"{"stt":{"entries":[{"kind":"on_device","id":"first","name":"First","active":true,"model":"nemotron_160ms"}]}}"#,
        )
        .unwrap();
        let switched = DesktopConfig::from_json_str(
            r#"{"stt":{"entries":[{"kind":"on_device","id":"second","name":"Second","active":true,"model":"unified_160ms"}]}}"#,
        )
        .unwrap();
        assert_ne!(
            running.stt.entries.active_live().id,
            switched.stt.entries.active_live().id
        );
        let (outcome, requests, started) = run(
            vec![],
            vec![Control::Apply(Box::new(switched.clone())), Control::Cancel],
        )
        .await;
        assert!(started);
        assert_eq!(outcome, Some("cancelled"));
        assert_eq!(
            requests.pending,
            Some(switched),
            "the new entry is held for the next capture"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_toggle_changes_the_intent_without_reviving_a_cancelled_capture() {
        // Both toggles land before the capture starts: nothing starts, and the intent is on again.
        let (outcome, requests, started) = run(
            vec![Control::ToggleListening, Control::ToggleListening],
            vec![],
        )
        .await;
        assert_eq!(outcome, None, "a cancelled capture delivers nothing");
        assert!(!started);
        assert!(
            !requests.listening_off,
            "the second toggle is the latest intent"
        );

        // Both toggles land during the capture: the first cancels it, the second only
        // changes the intent, so the cancelled outcome still is not a delivery to resume.
        let (outcome, requests, started) = run(
            vec![],
            vec![Control::ToggleListening, Control::ToggleListening],
        )
        .await;
        assert_eq!(outcome, Some("cancelled"));
        assert!(started);
        assert!(!requests.listening_off);

        let (outcome, requests, _) = run(vec![], vec![Control::ToggleListening]).await;
        assert_eq!(outcome, Some("cancelled"));
        assert!(requests.listening_off, "one toggle during a capture pauses");
    }

    struct NoticeRig;

    impl Rig for NoticeRig {
        type Recognizer = Chosen;
        type Wake = WakeEngine;
        type Cues = CpalCuePlayer;
        type Input = Microphone;
        type Host = TestHost;

        async fn load(
            &self,
            _: &DesktopConfig,
            _: &InputGate,
            _: &Rc<Publisher>,
        ) -> Result<Loaded<Self>, Failure> {
            std::future::pending().await
        }

        fn input(&self, _: InputGate, _: Option<String>, _: Metrics) -> Microphone {
            panic!("notice controls do not open a microphone")
        }
    }

    #[tokio::test]
    async fn working_hold_keys_clear_the_shown_or_pending_failure_without_ending_a_capture() {
        let config = DesktopConfig::from_json_str("{}").unwrap();
        let (link, ends, _) = EngineLink::wire(&config, false);
        let publisher = Publisher::new(ends.status, config.ending_pause);
        publisher.mode(Mode::Idle);
        let mut runner = Runner::new(
            config,
            Arc::new(Delivery::new(TestHost::default())),
            ends.control,
            Rc::clone(&publisher),
            ends.level,
            ends.audio,
            Shared {
                rig: NoticeRig,
                key: ends.key,
                history: Arc::new(History::at("unused-history")),
                drafts: Arc::new(Drafts::default()),
                feeding: ends.feeding,
                paused: false,
                last_writing_window: ends.last_writing_window,
            },
        );
        for show in [true, false] {
            runner
                .handle(Control::HoldKeyFailed("Hyprland refused F9.".to_owned()))
                .await;
            if show {
                runner.show_hold_problem();
                assert!(
                    matches!(&publisher.view.borrow().mode, Mode::Failed(f) if f.headline == "Hold key unavailable")
                );
            }
            link.hold_problems()(HoldReport::Working);
            let control = runner.control.recv().await.unwrap();
            runner.handle(control).await;
            assert!(
                runner.hold_problem.is_none(),
                "recovery clears a pending hold failure"
            );
            assert_eq!(
                publisher.view.borrow().mode,
                Mode::Idle,
                "recovery clears a shown hold failure"
            );
        }
        runner.hold_problem = Some("Hyprland refused F9.".to_owned());
        link.hold_problems()(HoldReport::Working);
        runner.open_microphone().await;
        assert!(
            runner.hold_problem.is_none(),
            "queued recovery clears the earlier pending failure"
        );

        let other = Mode::Failed(Failure {
            kind: FailureKind::Other,
            headline: "Another failure",
            notice: "Keep this.".to_owned(),
            causes: Vec::new(),
        });
        publisher.mode(other.clone());
        runner.handle(Control::HoldKeyWorks).await;
        assert_eq!(
            publisher.view.borrow().mode,
            other,
            "unrelated failures stay visible"
        );

        let mut requests = Requests::default();
        assert!(!requests.take(Some(Control::HoldKeyFailed(
            "Hyprland refused F9.".to_owned()
        ))));
        assert!(
            !requests.take(Some(Control::HoldKeyWorks)),
            "recovery does not end the capture"
        );
        assert!(!requests.ended);
        assert_eq!(
            requests.hold_problem, None,
            "recovery clears a failure pending during capture"
        );
    }

    #[test]
    fn a_refused_hold_key_is_kept_through_a_capture_and_told_in_plain_words() {
        let (sender, mut control) = mpsc::unbounded_channel();
        sender
            .send(Control::HoldKeyFailed(
                "Windows refused the hook.".to_owned(),
            ))
            .unwrap();
        let mut requests = Requests::default();
        assert!(
            !requests.read_queued(&mut control),
            "it does not end a capture"
        );
        assert_eq!(
            requests.hold_problem.as_deref(),
            Some("Windows refused the hook.")
        );
        let failure = hold_failure("Windows refused the hook.".to_owned());
        assert_eq!(
            (failure.notice.as_str(), failure.headline),
            ("Windows refused the hook.", "Hold key unavailable")
        );
    }

    #[test]
    fn listening_requests_queued_before_the_microphone_opens_settle_on_the_latest_one() {
        let config = DesktopConfig::from_json_str("{}").unwrap();
        for (listening_off, queued, expected) in [
            (false, vec![Control::ToggleListening], true),
            (
                false,
                vec![Control::ToggleListening, Control::ToggleListening],
                false,
            ),
            (true, vec![Control::ToggleListening], false),
            (
                false,
                vec![Control::SetListening(false), Control::ToggleListening],
                false,
            ),
            (
                false,
                vec![Control::ToggleListening, Control::SetListening(true)],
                false,
            ),
            (true, vec![Control::SetListening(true)], false),
            (
                false,
                vec![Control::SetListening(false), Control::SetListening(false)],
                true,
            ),
            (
                true,
                vec![Control::SetListening(true), Control::SetListening(true)],
                false,
            ),
        ] {
            let label = format!("off={listening_off} {queued:?}");
            let (sender, mut control) = mpsc::unbounded_channel();
            for message in queued {
                sender.send(message).unwrap();
            }
            let mut requests = Requests {
                listening_off,
                ..Requests::default()
            };
            requests.read_queued(&mut control);
            assert_eq!(requests.listening_off, expected, "{label}");
            assert!(!requests.quit, "{label}");
        }

        let (sender, mut control) = mpsc::unbounded_channel();
        sender
            .send(Control::Apply(Box::new(config.clone())))
            .unwrap();
        sender.send(Control::Quit).unwrap();
        let mut requests = Requests::default();
        assert!(requests.read_queued(&mut control), "a queued quit ends it");
        assert!(requests.quit);
        assert_eq!(requests.pending, Some(config));
    }

    #[tokio::test(start_paused = true)]
    async fn a_level_that_arrives_after_the_microphone_closed_or_paused_stays_zero() {
        LocalSet::new()
            .run_until(a_level_forwarding_follows_the_open_microphone())
            .await;
    }

    async fn a_level_forwarding_follows_the_open_microphone() {
        let (state_tx, state) = watch::channel(EngineState::Listening);
        let (engine_level_tx, engine_level) = watch::channel(0.0_f32);
        let (level_tx, mut level) = watch::channel(0.0_f32);
        let (status_tx, _status) = watch::channel(Status::Paused);
        let (audio_tx, audio) = watch::channel(AudioStatus {
            selected: None,
            active: Some("USB Mic".to_owned()),
            paused: false,
            problem: None,
        });
        let following = tokio::task::spawn_local(follow(
            state,
            engine_level,
            Publisher::new(status_tx, EndingPauses::default()),
            Arc::new(level_tx),
            audio,
        ));

        engine_level_tx.send_replace(0.8);
        tokio::time::timeout(Duration::from_secs(5), level.changed())
            .await
            .expect("the level was not forwarded")
            .unwrap();
        assert_eq!(
            *level.borrow(),
            0.8,
            "an open microphone forwards its level"
        );

        // What closing and pausing publish, before any pending engine level arrives.
        audio_tx.send_replace(AudioStatus {
            selected: None,
            active: None,
            paused: true,
            problem: None,
        });
        engine_level_tx.send_replace(0.9);
        tokio::time::timeout(Duration::from_secs(5), level.changed())
            .await
            .expect("the level was not forwarded")
            .unwrap();
        assert_eq!(
            *level.borrow(),
            0.0,
            "a pending level must not restore a paused meter"
        );

        audio_tx.send_replace(AudioStatus {
            selected: None,
            active: None,
            paused: false,
            problem: None,
        });
        engine_level_tx.send_replace(0.7);
        tokio::time::timeout(Duration::from_secs(5), level.changed())
            .await
            .expect("the level was not forwarded")
            .unwrap();
        assert_eq!(
            *level.borrow(),
            0.0,
            "a pending level must not restore a closed meter"
        );

        audio_tx.send_replace(AudioStatus {
            selected: None,
            active: Some("USB Mic".to_owned()),
            paused: false,
            problem: None,
        });
        engine_level_tx.send_replace(0.6);
        tokio::time::timeout(Duration::from_secs(5), level.changed())
            .await
            .expect("the level was not forwarded")
            .unwrap();
        assert_eq!(*level.borrow(), 0.6, "a reopened microphone forwards again");

        drop(state_tx);
        drop(engine_level_tx);
        following.await.unwrap();
    }
    #[tokio::test]
    async fn the_spotter_loader_reports_downloads_and_returns_the_model_or_a_notice() {
        use thalvi_desktop::engine::fakes::FakeSpotter;
        use thalvi_desktop::spotter::Unspeakable;

        let assets = [
            models::SPOTTER.encoder,
            models::SPOTTER.decoder,
            models::SPOTTER.joiner,
            models::SPOTTER.pieces,
        ];
        for missing in [None, Some(0), Some(3)] {
            let dir = std::env::temp_dir().join(format!(
                "thalvi-spotter-load-{}-{missing:?}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            for (index, asset) in assets.iter().enumerate() {
                if missing != Some(index) {
                    std::fs::File::create(dir.join(asset.file_name))
                        .unwrap()
                        .set_len(asset.size)
                        .unwrap();
                }
            }
            let store = ModelStore::in_dir(&dir);
            for fails in [false, true] {
                let (out, status) = watch::channel(Status::Paused);
                let publisher = Publisher::new(out, EndingPauses::default());
                publisher.mode(Mode::Paused);
                let recorder = Recorder::disabled();
                let (spotter, notice) =
                    load_spotter(&publisher, &Endings::default(), &store, &recorder, async {
                        assert_eq!(
                            *status.borrow(),
                            if missing.is_some() {
                                Status::Loading {
                                    notice: SPOTTER_DOWNLOAD_NOTICE.to_owned(),
                                    downloading: true,
                                }
                            } else {
                                Status::Paused
                            }
                        );
                        if fails {
                            Err(SpotterError::Phrases(vec![Unspeakable {
                                phrase: "f\u{fc}ge ein".to_owned(),
                                characters: "\u{dc}".to_owned(),
                            }]))
                        } else {
                            let (fake, _control) = FakeSpotter::new();
                            Ok(Box::new(fake) as Box<dyn KeywordSpotter>)
                        }
                    })
                    .await;
                if fails {
                    assert!(spotter.is_none());
                    let notice = notice.unwrap();
                    assert!(
                        notice.contains("\"f\u{fc}ge ein\"")
                            && notice.ends_with("Endings still work from the transcript."),
                        "{notice}"
                    );
                } else {
                    assert!(spotter.is_some() && notice.is_none());
                }
                if missing.is_some() {
                    assert_eq!(title(&status.borrow()), "Thalvi: starting");
                } else {
                    assert_eq!(*status.borrow(), Status::Paused);
                }
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A microphone the test can leave missing, and the fakes the runner listens through.
    struct MicLab {
        opens: Rc<Cell<u32>>,
        missing: Rc<Cell<bool>>,
        wake: RefCell<Option<thalvi_desktop::engine::fakes::FakeWake>>,
        recognizer: RefCell<Option<thalvi_desktop::recognizer::FakeRecognizer>>,
        cues: RefCell<Option<thalvi_desktop::engine::fakes::FakeCuePlayer>>,
    }

    struct LabMic {
        gate: InputGate,
        opens: Rc<Cell<u32>>,
        missing: Rc<Cell<bool>>,
        opened: bool,
        /// Holds the fake stream open; the test never sends room audio through it.
        held: Option<thalvi_desktop::engine::fakes::FrameFeed>,
    }

    impl Rig for MicLab {
        type Recognizer = thalvi_desktop::recognizer::FakeRecognizer;
        type Wake = thalvi_desktop::engine::fakes::FakeWake;
        type Cues = thalvi_desktop::engine::fakes::FakeCuePlayer;
        type Input = LabMic;
        type Host = TestHost;

        fn load(
            &self,
            _: &DesktopConfig,
            _: &InputGate,
            _: &Rc<Publisher>,
        ) -> impl Future<Output = Result<Loaded<Self>, Failure>> {
            let recognizer = self.recognizer.borrow_mut().take().unwrap();
            let wake = self.wake.borrow_mut().take().unwrap();
            let cues = self.cues.borrow_mut().take().unwrap();
            std::future::ready(Ok(Loaded {
                recognizer,
                wake,
                cues,
                recorder: Recorder::disabled(),
                http: reqwest::Client::new(),
                chat: None,
                spotter: None,
                spotter_off: None,
                extras: Vec::new(),
                local: None,
                extra_locals: Vec::new(),
            }))
        }

        fn input(&self, gate: InputGate, _: Option<String>, _: Metrics) -> LabMic {
            LabMic {
                gate,
                opens: Rc::clone(&self.opens),
                missing: Rc::clone(&self.missing),
                opened: false,
                held: None,
            }
        }
    }

    impl Input for LabMic {
        type Frames = thalvi_desktop::engine::fakes::FakeFrames;

        fn active_name(&self) -> Option<&str> {
            self.opened.then_some("Test microphone")
        }

        fn select(&mut self, _: Option<String>) {}

        fn pause(&mut self) {
            self.opened = false;
            self.held = None;
        }

        fn open(
            self,
        ) -> impl Future<Output = Result<(Self, Result<Self::Frames, Failure>), Failure>> {
            std::future::ready(Ok(self.open_stream()))
        }

        fn replace(
            self,
        ) -> impl Future<Output = Result<(Self, Result<Self::Frames, Failure>), Failure>> {
            std::future::ready(Ok(self.open_stream()))
        }
    }

    impl LabMic {
        fn open_stream(mut self) -> (Self, Result<FakeFramesSlot, Failure>) {
            self.opens.set(self.opens.get() + 1);
            if self.missing.get() {
                self.opened = false;
                self.held = None;
                return (
                    self,
                    Err(Failure::of(&thalvi_desktop::audio::AudioError::NoDevice)),
                );
            }
            let (frames, feed) = thalvi_desktop::engine::fakes::FakeFrames::new(&self.gate);
            self.opened = true;
            self.held = Some(feed);
            (self, Ok(frames))
        }
    }

    type FakeFramesSlot = thalvi_desktop::engine::fakes::FakeFrames;

    fn spoken_clip() -> Clip {
        use thalvi_desktop::frame::FRAME_SAMPLES;
        let mut frames = Vec::new();
        for n in 0..8 {
            let mut frame = [0_i16; FRAME_SAMPLES];
            if n == 0 {
                frame.fill(2_000);
            }
            frames.push(frame);
        }
        Clip::from_frames(frames)
    }

    fn feed_config() -> DesktopConfig {
        DesktopConfig::from_json_str(r#"{"ending_pause":{"paste":0},"history":{"enabled":false}}"#)
            .unwrap()
    }

    struct FedRun {
        link: EngineLink,
        running: tokio::task::JoinHandle<()>,
        wake: thalvi_desktop::engine::fakes::WakeControl,
        server: thalvi_desktop::recognizer::FakeControl,
        opens: Rc<Cell<u32>>,
        texts: Arc<Mutex<Vec<String>>>,
        status: watch::Receiver<Status>,
        audio: watch::Receiver<AudioStatus>,
        feeding: watch::Receiver<bool>,
    }

    impl FedRun {
        fn start(missing: bool) -> Self {
            use thalvi_desktop::cues::Cue;
            use thalvi_desktop::engine::fakes::{FakeCuePlayer, FakeWake};
            use thalvi_desktop::recognizer::FakeRecognizer;

            let (wake_detector, wake) = FakeWake::new();
            let (recognizer, server) = FakeRecognizer::new();
            let (cues, cue_control) = FakeCuePlayer::new();
            for cue in [
                Cue::Wake,
                Cue::Provisional,
                Cue::Accepted,
                Cue::Cancelled,
                Cue::Error,
            ] {
                cue_control.set_length(cue, Duration::ZERO);
            }
            let opens = Rc::new(Cell::new(0));
            let lab = MicLab {
                opens: Rc::clone(&opens),
                missing: Rc::new(Cell::new(missing)),
                wake: RefCell::new(Some(wake_detector)),
                recognizer: RefCell::new(Some(recognizer)),
                cues: RefCell::new(Some(cues)),
            };
            let texts = Arc::new(Mutex::new(Vec::new()));
            let host = TestHost(Arc::new(Mutex::new(Vec::new())), Arc::clone(&texts));
            let config = feed_config();
            let (link, ends, _) = EngineLink::wire(&config, false);
            let status = link.status_updates();
            let audio = link.audio_updates();
            let feeding = link.feeding_updates();
            let running = tokio::task::spawn_local(ends.run(
                config,
                false,
                lab,
                Arc::new(Delivery::new(host)),
                Arc::new(History::at("unused-scripted-feed")),
                Arc::new(Drafts::default()),
            ));
            Self {
                link,
                running,
                wake,
                server,
                opens,
                texts,
                status,
                audio,
                feeding,
            }
        }

        fn missing_mic(&self) -> bool {
            matches!(
                &*self.status.borrow(),
                Status::Failed { notice, .. } if notice.contains("No microphone was found")
            )
        }

        fn pasted(&self) -> Vec<String> {
            self.texts.lock().unwrap().clone()
        }

        async fn stop(self) {
            self.link.send(Control::Quit);
            self.running.await.unwrap();
        }
    }

    async fn yield_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{what}");
    }

    /// Lets the paused clock run until `done`, failing after 6.4 s of it; the clock holds still
    /// while a host call runs on the blocking pool, so a slow thread cannot use up the budget.
    async fn pump_until(what: &str, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_millis(6_400), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    }

    async fn feed_result(mut ended: oneshot::Receiver<FeedEnd>) -> FeedEnd {
        for _ in 0..50 {
            match ended.try_recv() {
                Ok(end) => return end,
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => return FeedEnd::Cancelled,
            }
            tokio::task::yield_now().await;
        }
        panic!("the feed did not finish");
    }

    /// `Ok` once a file is playing. `Err` is how a feed that never started ended.
    async fn feed_started(
        ended: &mut oneshot::Receiver<FeedEnd>,
        feeding: &watch::Receiver<bool>,
    ) -> Result<(), FeedEnd> {
        for _ in 0..50 {
            if *feeding.borrow() {
                return Ok(());
            }
            match ended.try_recv() {
                Ok(end) => return Err(end),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => return Err(FeedEnd::Cancelled),
            }
            tokio::task::yield_now().await;
        }
        panic!("the feed neither started nor finished");
    }

    async fn capture_started(script: &mut FedRun) {
        tokio::time::timeout(
            Duration::from_secs(3),
            script
                .status
                .wait_for(|status| matches!(status, Status::Capturing { .. })),
        )
        .await
        .expect("the file never started a capture")
        .expect("the runner stopped");
        assert!(
            *script.feeding.borrow(),
            "capture did not start during the file"
        );
    }

    async fn deliver(script: &mut FedRun) {
        use thalvi_desktop::recognizer::RecognizerEvent;
        use thalvi_desktop::wake::WakeEvent;

        let prior_pastes = script.pasted().len();
        script.server.ready();
        script.wake.fire(WakeEvent::Capture);
        let (feed, mut ended) = Feed::new(spoken_clip());
        script.link.send(Control::Feed(feed));
        feed_started(&mut ended, &script.feeding)
            .await
            .unwrap_or_else(|end| panic!("the file was refused: {end:?}"));
        capture_started(script).await;
        script
            .server
            .emit(Ok(RecognizerEvent::Final("Hello paste that".to_owned())));
        pump_until("the file was not delivered", || {
            script.pasted().len() > prior_pastes
        })
        .await;
        pump_until("the file did not finish", || {
            matches!(
                ended.try_recv(),
                Ok(FeedEnd::Played) | Err(oneshot::error::TryRecvError::Closed)
            )
        })
        .await;
    }

    async fn pasted_twice(mut script: FedRun, mut ended: oneshot::Receiver<FeedEnd>, label: &str) {
        script
            .server
            .emit(Ok(thalvi_desktop::recognizer::RecognizerEvent::Final(
                "Hello paste that".to_owned(),
            )));
        pump_until(&format!("{label}: nothing was pasted"), || {
            script.pasted() == ["Hello"]
        })
        .await;
        pump_until(
            &format!("{label}: the microphone error did not return"),
            || script.missing_mic() && !*script.feeding.borrow(),
        )
        .await;
        assert!(script.audio.borrow().active.is_none(), "{label}");
        assert!(
            matches!(ended.try_recv(), Ok(FeedEnd::Played)),
            "{label}: {:?}",
            ended.try_recv()
        );
        // The same path accepts another file once the first has released.
        script.server.connect_again();
        deliver(&mut script).await;
        assert_eq!(script.pasted(), ["Hello", "Hello"], "{label}");
        pump_until(
            &format!("{label}: the microphone error did not return"),
            || script.missing_mic() && !*script.feeding.borrow(),
        )
        .await;
        assert!(script.audio.borrow().active.is_none(), "{label}");
        let opens = script.opens.get();
        tokio::time::advance(Duration::from_secs(1)).await;
        yield_until(&format!("{label}: the microphone was not retried"), || {
            script.opens.get() > opens
        })
        .await;
        assert_eq!(script.opens.get(), opens + 1, "{label}");
        script.stop().await;
    }

    #[derive(Clone, Copy)]
    enum FeedStop {
        Deliver,
        Cancel,
        Pause,
        Quit,
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_feed_reaches_delivery_without_a_microphone_unless_it_is_stopped() {
        for stop in [
            FeedStop::Deliver,
            FeedStop::Cancel,
            FeedStop::Pause,
            FeedStop::Quit,
        ] {
            let label = match stop {
                FeedStop::Deliver => "deliver",
                FeedStop::Cancel => "cancel",
                FeedStop::Pause => "pause",
                FeedStop::Quit => "quit",
            };
            LocalSet::new()
                .run_until(async {
                    let mut script = FedRun::start(true);
                    yield_until("the missing microphone was not reported", || {
                        script.missing_mic()
                    })
                    .await;
                    assert_eq!(script.opens.get(), 1, "{label}");
                    assert!(script.audio.borrow().active.is_none(), "{label}");
                    let (feed, mut ended) = Feed::new(spoken_clip());
                    script.wake.fire(thalvi_desktop::wake::WakeEvent::Capture);
                    script.server.ready();
                    script.link.send(Control::Feed(feed));
                    feed_started(&mut ended, &script.feeding)
                        .await
                        .unwrap_or_else(|end| panic!("{label}: the file was refused: {end:?}"));
                    assert_eq!(
                        script.opens.get(),
                        1,
                        "{label}: a feed must not reopen the microphone"
                    );
                    assert!(script.audio.borrow().active.is_none(), "{label}");
                    capture_started(&mut script).await;
                    match stop {
                        FeedStop::Deliver => pasted_twice(script, ended, label).await,
                        FeedStop::Cancel => {
                            script.link.send(Control::Cancel);
                            let end = tokio::time::timeout(Duration::from_secs(3), ended)
                                .await
                                .expect("cancel: the file kept playing")
                                .expect("cancel: the file lost its reply");
                            assert_eq!(end, FeedEnd::Cancelled, "{label}");
                            assert!(script.pasted().is_empty(), "{label}");
                            pump_until(
                                &format!("{label}: the microphone error did not return"),
                                || script.missing_mic(),
                            )
                            .await;
                            script.stop().await;
                        }
                        FeedStop::Pause => {
                            script.link.send(Control::SetListening(false));
                            pump_until(&format!("{label}: listening stayed on"), || {
                                matches!(&*script.status.borrow(), Status::Paused)
                            })
                            .await;
                            assert!(
                                matches!(ended.try_recv(), Ok(FeedEnd::Cancelled)),
                                "{label}"
                            );
                            assert!(script.pasted().is_empty(), "{label}");
                            assert!(script.audio.borrow().paused, "{label}");
                            assert!(script.audio.borrow().active.is_none(), "{label}");
                            script.stop().await;
                        }
                        FeedStop::Quit => {
                            let texts = Arc::clone(&script.texts);
                            script.link.send(Control::Quit);
                            script.running.await.unwrap();
                            yield_until(&format!("{label}: the file was delivered"), || {
                                matches!(
                                    ended.try_recv(),
                                    Ok(FeedEnd::Cancelled)
                                        | Err(oneshot::error::TryRecvError::Closed)
                                )
                            })
                            .await;
                            assert!(texts.lock().unwrap().is_empty(), "{label}");
                        }
                    }
                })
                .await;
        }
    }

    #[derive(Clone, Copy)]
    enum KeptFeed {
        Paused,
        HoldKey,
        Retry,
        Connected,
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_feed_keeps_pause_keys_retry_and_a_connected_microphone() {
        for case in [
            KeptFeed::Paused,
            KeptFeed::HoldKey,
            KeptFeed::Retry,
            KeptFeed::Connected,
        ] {
            let label = match case {
                KeptFeed::Paused => "paused",
                KeptFeed::HoldKey => "hold key",
                KeptFeed::Retry => "retry",
                KeptFeed::Connected => "connected",
            };
            LocalSet::new()
                .run_until(async {
                    let missing = !matches!(case, KeptFeed::Connected);
                    let mut script = FedRun::start(missing);
                    if missing {
                        yield_until(
                            &format!("{label}: the missing microphone was not reported"),
                            || script.missing_mic(),
                        )
                        .await;
                        assert!(script.audio.borrow().active.is_none(), "{label}");
                    } else {
                        yield_until(&format!("{label}: listening never started"), || {
                            matches!(&*script.status.borrow(), Status::Listening)
                        })
                        .await;
                        assert_eq!(
                            script.audio.borrow().active.as_deref(),
                            Some("Test microphone"),
                            "{label}"
                        );
                    }
                    match case {
                        KeptFeed::Paused => {
                            script.link.send(Control::SetListening(false));
                            yield_until("listening was not paused", || {
                                matches!(&*script.status.borrow(), Status::Paused)
                            })
                            .await;
                            let (feed, ended) = Feed::new(spoken_clip());
                            script.link.send(Control::Feed(feed));
                            let end = feed_result(ended).await;
                            assert_eq!(
                                end,
                                FeedEnd::Refused("Thalvi is not listening."),
                                "{label}"
                            );
                            assert!(script.pasted().is_empty(), "{label}");
                            assert!(script.audio.borrow().active.is_none(), "{label}");
                            script.stop().await;
                        }
                        KeptFeed::HoldKey => {
                            let key = script.link.hold_input();
                            key.set(true);
                            key.set(false);
                            yield_until("the press was not taken", || script.opens.get() == 1)
                                .await;
                            for _ in 0..20 {
                                tokio::task::yield_now().await;
                            }
                            assert!(script.missing_mic(), "{label}");
                            assert!(script.pasted().is_empty(), "{label}");
                            assert_eq!(script.opens.get(), 1, "{label}");
                            script.stop().await;
                        }
                        KeptFeed::Retry => {
                            assert_eq!(script.opens.get(), 1, "{label}");
                            tokio::time::advance(Duration::from_millis(999)).await;
                            for _ in 0..20 {
                                tokio::task::yield_now().await;
                            }
                            assert_eq!(script.opens.get(), 1, "{label}: retried early");
                            tokio::time::advance(Duration::from_millis(1)).await;
                            yield_until("the microphone was not retried at one second", || {
                                script.opens.get() == 2
                            })
                            .await;
                            assert!(script.missing_mic(), "{label}");
                            assert!(script.audio.borrow().active.is_none(), "{label}");
                            script.stop().await;
                        }
                        KeptFeed::Connected => {
                            deliver(&mut script).await;
                            assert_eq!(script.pasted(), ["Hello"], "{label}");
                            assert_eq!(
                                script.audio.borrow().active.as_deref(),
                                Some("Test microphone"),
                                "{label}"
                            );
                            assert_eq!(script.opens.get(), 1, "{label}");
                            script.stop().await;
                        }
                    }
                })
                .await;
        }
    }
}

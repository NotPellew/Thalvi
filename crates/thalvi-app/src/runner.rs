//! Runs the capture engine on its own thread and follows the app's control messages.

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use thalvi_desktop::audio::{Frames, InputGate, Microphone};
use thalvi_desktop::capture::{CaptureRoute, EndingPauses, Endings};
use thalvi_desktop::chosen::Chosen;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::delivery::{Bound, Delivery, Finished};
use thalvi_desktop::device::Step;
use thalvi_desktop::draft::{Draft, Drafts};
use thalvi_desktop::engine::{
    CaptureOutcome, CpalCuePlayer, Engine, EngineHandle, EngineState, Parts, WakeTrigger,
};
use thalvi_desktop::feed::{Clip, Fed, Feed, FeedEnd, Feeder};
use thalvi_desktop::history::{Capture, Store as History};
use thalvi_desktop::hold::{HoldInput, KeyState, Presses, Refusal};
use thalvi_desktop::host::{Desktop, Host};
use thalvi_desktop::local::{LocalRecognizer, ModelSource};
use thalvi_desktop::metrics::{Event, Metrics, Recorder};
use thalvi_desktop::models::{self, ModelStore};
use thalvi_desktop::report::{UserError, causes};
use thalvi_desktop::speech_device::SpeechDevices;
use thalvi_desktop::spotter::{KeywordSpotter, OnnxSpotter, SpotterError};
use thalvi_desktop::transducer::TransducerSource;
use thalvi_desktop::wake::{WakeConfig, WakeEngine};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, LocalSet};

use crate::status::{
    Failure, FailureKind, Mode, SPOTTER_DOWNLOAD_NOTICE, STARTING_NOTICE, Status, View,
    WAKE_DOWNLOAD_NOTICE, load_notice,
};

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
    /// Cancels any capture, closes the microphone and the recognizer, and stops.
    Quit,
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
}

impl EngineLink {
    /// Starts the engine thread; it loads the wake word models before it listens.
    ///
    /// # Errors
    /// Fails when the thread or its runtime cannot start.
    pub fn start(
        config: DesktopConfig,
        delivery: Arc<Delivery<Desktop>>,
        history: Arc<History>,
        drafts: Arc<Drafts>,
        devices: Arc<SpeechDevices>,
    ) -> Result<Self, String> {
        let (control, control_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(Status::Loading {
            notice: STARTING_NOTICE.to_owned(),
            downloading: false,
        });
        let (level_tx, level) = watch::channel(0.0);
        let (audio_tx, audio) = watch::channel(AudioStatus {
            selected: config.microphone.clone(),
            active: None,
            paused: false,
        });
        let (feeding_tx, feeding) = watch::channel(false);
        let (key, key_state) = HoldInput::new();
        let (ready_tx, ready) = std_mpsc::channel();
        let (stopped_tx, stopped) = std_mpsc::channel();
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
                let publisher = Publisher::new(status_tx, config.ending_pause);
                let runner = Runner::new(
                    config,
                    delivery,
                    control_rx,
                    publisher,
                    Arc::new(level_tx),
                    audio_tx,
                    Shared {
                        key: key_state,
                        history,
                        drafts,
                        devices,
                        feeding: Arc::new(feeding_tx),
                    },
                );
                LocalSet::new().block_on(&runtime, runner.run());
                finish_runtime(runtime, &stopped_tx);
            })
            .map_err(|error| format!("Thalvi could not start: {error}"))?;
        ready
            .recv()
            .map_err(|_| "Thalvi could not start the engine.".to_owned())??;
        Ok(Self {
            control,
            status,
            level,
            audio,
            feeding,
            stopped: Mutex::new(stopped),
            key,
        })
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
    pub fn hold_problems(&self) -> impl Fn(String) + Send + Sync + 'static {
        let control = self.control.clone();
        move |notice| {
            let _ = control.send(Control::HoldKeyFailed(notice));
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
        let stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = stopped.recv_timeout(QUIT_TIMEOUT);
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
struct Publisher {
    view: RefCell<View>,
    out: watch::Sender<Status>,
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
            }),
            out,
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

    fn mode(&self, mode: Mode) {
        self.update(|view| view.mode = mode);
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
async fn load_showing<S: ModelSource>(local: &LocalRecognizer<S>, publisher: &Publisher) {
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
            view.mode = Mode::Failed(Failure {
                kind: FailureKind::Other,
                notice: format!("{error} Dictation will try again when you start it."),
                headline: error.headline(),
                causes: causes(&error),
            });
        }
    });
}

/// Everything one engine needs; replaced when settings change.
struct Session {
    engine: Engine<Chosen, WakeEngine, CpalCuePlayer>,
    /// The local recognizer, whose model loads ahead of the first dictation.
    local: Option<Arc<LocalRecognizer<TransducerSource>>>,
    handle: EngineHandle,
    gate: InputGate,
    device: Option<String>,
    metrics: Metrics,
    microphone: Option<Microphone>,
    frames: Option<Fed<Frames>>,
    feeder: Feeder,
}

impl Session {
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

    async fn open_microphone(&mut self) -> Result<(), Failure> {
        let mut microphone = self.microphone.take().unwrap_or_else(|| {
            Microphone::new(self.gate.clone(), self.device.clone(), self.metrics.clone())
        });
        let (microphone, frames) = tokio::task::spawn_blocking(move || {
            let frames = microphone.open();
            (microphone, frames)
        })
        .await
        .map_err(|error| Failure {
            kind: FailureKind::Other,
            notice: "The microphone's audio thread crashed. Restart Thalvi.".to_owned(),
            headline: "Microphone crashed — restart Thalvi",
            causes: vec![error.to_string()],
        })?;
        self.microphone = Some(microphone);
        self.frames = Some(
            self.feeder
                .wrap(frames.map_err(|error| Failure::of(&error))?),
        );
        Ok(())
    }

    /// Flushes the recordings and closes the microphone.
    async fn close(mut self) {
        self.close_microphone();
        self.engine.finish().await;
    }
}

enum Flow {
    Continue,
    Quit,
}

enum Woke {
    Trigger(Result<WakeTrigger, thalvi_desktop::engine::ListenError>),
    Control(Option<Control>),
}

/// What the app shares with the runner besides its controls.
struct Shared {
    /// Whether the hold key is down.
    key: watch::Receiver<KeyState>,
    history: Arc<History>,
    drafts: Arc<Drafts>,
    /// Where the speech model runs, for Settings.
    devices: Arc<SpeechDevices>,
    /// Whether a file's audio is being fed in place of the microphone's.
    feeding: Arc<watch::Sender<bool>>,
}

struct Runner {
    config: DesktopConfig,
    session: Option<Session>,
    paused: bool,
    stopping: bool,
    /// Set while listening wants a microphone it does not have: how many reopen attempts failed.
    reopen_failures: Option<u32>,
    /// Settings that arrived during a capture.
    pending: Option<DesktopConfig>,
    /// A refused hold key not yet shown.
    hold_problem: Option<String>,
    control: mpsc::UnboundedReceiver<Control>,
    publisher: Rc<Publisher>,
    level: Arc<watch::Sender<f32>>,
    audio: watch::Sender<AudioStatus>,
    delivery: Arc<Delivery<Desktop>>,
    history: Arc<History>,
    drafts: Arc<Drafts>,
    devices: Arc<SpeechDevices>,
    feeding: Arc<watch::Sender<bool>>,
    http: reqwest::Client,
    /// Loads the current engine's speech model.
    loading: Option<JoinHandle<()>>,
    /// The hold key, for presses the engine is not listening for.
    key: Presses,
    key_state: watch::Receiver<KeyState>,
}

impl Runner {
    fn new(
        config: DesktopConfig,
        delivery: Arc<Delivery<Desktop>>,
        control: mpsc::UnboundedReceiver<Control>,
        publisher: Rc<Publisher>,
        level: Arc<watch::Sender<f32>>,
        audio: watch::Sender<AudioStatus>,
        shared: Shared,
    ) -> Self {
        Self {
            config,
            session: None,
            paused: false,
            stopping: false,
            reopen_failures: None,
            pending: None,
            hold_problem: None,
            control,
            publisher,
            level,
            audio,
            delivery,
            history: shared.history,
            drafts: shared.drafts,
            devices: shared.devices,
            feeding: shared.feeding,
            http: reqwest::Client::new(),
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
        let Some(local) = local else {
            self.publisher.loading(None);
            return;
        };
        let first = *local.steps().borrow();
        println!("{}", load_notice(first));
        self.publisher.loading(Some(first));
        let publisher = Rc::clone(&self.publisher);
        self.loading = Some(tokio::task::spawn_local(async move {
            load_showing(&local, &publisher).await;
        }));
    }

    fn fail(&mut self, failure: Failure) {
        println!("{}", failure.notice);
        self.publisher.mode(Mode::Failed(failure));
    }

    async fn build(&self, config: &DesktopConfig) -> Result<Session, Failure> {
        let recorder =
            Recorder::per_user(config.recordings).map_err(|error| Failure::of(&error))?;
        let store = ModelStore::per_user().map_err(|error| Failure::of(&error))?;
        let started = Instant::now();
        let (wake, (spotter, spotter_off)) = tokio::join!(
            load_wake(
                &self.publisher,
                &config.wake,
                &store,
                WakeEngine::load(&config.wake, &store),
            ),
            load_spotter(&self.publisher, &config.endings, &store, &recorder, async {
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
        let gate = InputGate::default();
        let metrics = recorder.metrics();
        let recognizer = Chosen::new(
            config.stt.entries.active_live(),
            store,
            metrics.clone(),
            &config.stt.phrases,
            self.devices.adapters(),
        );
        let local = recognizer.local();
        self.devices.use_recognizer(local.clone());
        let (engine, handle) = Engine::new(
            Parts {
                recognizer,
                wake,
                cues: CpalCuePlayer::new(config.cues, gate.clone()),
                gate: gate.clone(),
                http: self.http.clone(),
                hold: self.key_state.clone(),
                notice: self.publisher.notice_sink(),
                spotter,
                spotter_off,
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
        self.level.send_replace(0.0);
        self.publish_audio();
    }

    /// Opens the microphone unless listening is paused; a failure becomes the status.
    async fn open_microphone(&mut self) {
        let mut requests = Requests {
            listening_off: self.paused,
            ..Requests::default()
        };
        requests.read_queued(&mut self.control);
        self.paused = requests.listening_off;
        self.stopping |= requests.quit;
        self.hold_problem = requests.hold_problem.or(self.hold_problem.take());
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
        match session.open_microphone().await {
            Ok(()) => {
                self.reopen_failures = None;
                self.publisher.mode(Mode::Idle);
            }
            Err(failure) => self.microphone_failed(failure),
        }
        self.publish_audio();
    }

    /// Shows why listening has no microphone and schedules the next attempt to reopen it.
    fn microphone_failed(&mut self, failure: Failure) {
        if self.reopen_failures.is_none() {
            self.fail(failure);
        } else {
            tracing::warn!(notice = failure.notice, "microphone still unavailable");
        }
        self.reopen_failures = Some(self.reopen_failures.map_or(0, |failures| failures + 1));
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
        let woke = {
            let Self {
                session, control, ..
            } = &mut *self;
            let Some(Session {
                engine,
                frames: Some(frames),
                ..
            }) = session.as_mut()
            else {
                return Flow::Continue;
            };
            tokio::select! {
                trigger = engine.listen(frames) => Woke::Trigger(trigger),
                control = control.recv() => Woke::Control(control),
            }
        };
        match woke {
            Woke::Control(Some(control)) => self.handle(control).await,
            Woke::Control(None) => Flow::Quit,
            Woke::Trigger(Ok(WakeTrigger::Capture | WakeTrigger::Hold)) => {
                Box::pin(self.capture()).await
            }
            Woke::Trigger(Err(error)) => {
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
            Control::Feed(feed) => match &self.session {
                Some(Session {
                    frames: Some(_),
                    feeder,
                    ..
                }) => feeder.start(feed),
                _ => feed.refuse("Thalvi is not listening."),
            },
            Control::Apply(config) => self.apply(*config).await,
            Control::HoldKeyFailed(notice) => self.hold_problem = Some(notice),
            Control::Quit => return Flow::Quit,
        }
        Flow::Continue
    }

    /// Shows a refused hold key once nothing else is on the status: the failure stays until the
    /// next capture or resumed listening, like any other.
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

    /// Replaces the engine by one built from `config`; the old one stays if that fails. A change
    /// to the recordings or the history alone only retunes the running engine.
    async fn apply(&mut self, config: DesktopConfig) {
        if config == self.config {
            return;
        }
        if retunes_only(&self.config, &config) {
            if let Some(session) = self.session.as_mut() {
                session.engine.set_recordings(config.recordings);
            }
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
                self.publisher
                    .update(|view| view.pauses = config.ending_pause);
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
    async fn capture(&mut self) -> Flow {
        let bound = {
            let Some(session) = self.session.as_ref() else {
                return Flow::Continue;
            };
            self.delivery.bind(session.engine.recorder()).await
        };
        self.publisher
            .woke(bound.window(), self.delivery.copies_only());
        let mut requests = Requests::default();
        let (finished, route, ignored) = {
            let Self {
                session,
                control,
                delivery,
                history,
                drafts,
                config,
                key,
                publisher,
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
            let outcome = Box::pin(drive(
                engine.capture(frames),
                control,
                || {
                    handle.cancel();
                    feeder.cancel();
                },
                &mut requests,
            ))
            .await;
            let mut ignored = engine.take_ignored();
            match outcome {
                None => {
                    // The wake word closed the gate and only a capture opens it again.
                    gate.set_enabled(true);
                    (None, None, ignored)
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
                    let finished = Settling {
                        delivery,
                        history,
                        drafts,
                        config,
                        recorder: engine.recorder(),
                    }
                    .settle(&outcome, requests.ended, &bound, key, |refusal, count| {
                        report_refusal(engine.recorder(), refusal, count);
                        refused.borrow_mut().push(refusal);
                    })
                    .await;
                    ignored.extend(refused.into_inner());
                    (finished, route, ignored)
                }
            }
        };
        if let Some(config) = requests.pending.take() {
            self.pending = Some(config);
        }
        self.hold_problem = requests.hold_problem.or(self.hold_problem.take());
        if requests.quit {
            // Nothing is delivered after a quit; what was dictated is already kept.
            return Flow::Quit;
        }
        self.publisher
            .mode(told(delivered(finished, route), &ignored));
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
        self.keep(outcome, ended, finished.as_ref(), bound);
        finished
    }

    fn keep(
        &self,
        outcome: &CaptureOutcome,
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
        if let Some(capture) = Capture::of(kept, finished, window) {
            self.history
                .record_in_background(self.config.history, capture);
        }
        if !task && let Some(draft) = Draft::of(kept, finished, bound) {
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
        ..new.clone()
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
            Some(Control::Feed(feed)) => {
                feed.refuse("Thalvi was busy. Try again in a moment.");
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
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use thalvi_desktop::device::Export;
    use thalvi_desktop::models::{self, CatalogHead};

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
        let cases = [
            (
                "from disk",
                Step::Reading,
                false,
                "Loading the speech model.",
            ),
            (
                "the CPU files",
                Step::Fetching(Export::Cpu),
                true,
                "Loading the speech model. Downloading about 650 MB.",
            ),
            (
                "the GPU files",
                Step::Fetching(Export::Gpu),
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
            step(Step::Fetching(Export::Gpu));
            let gate = self.gate.lock().unwrap().take().unwrap();
            let _ = gate.await;
            step(Step::Fetching(Export::Cpu));
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
                        while publisher.view.borrow().loading != Some(Step::Fetching(Export::Gpu)) {
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
        publisher.loading(Some(Step::Fetching(Export::Cpu)));
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
        use Export::{Cpu, Gpu};
        LocalSet::new()
            .run_until(async {
                let (steps, _publisher, load, mut status) = loading_download();
                let mut shown = vec![loading_now(&status)];
                for (step, changes) in [
                    (Step::Downloading(Cpu, 0), true),
                    (Step::Downloading(Cpu, 0), false),
                    (Step::Downloading(Cpu, 1), true),
                    (Step::Downloading(Cpu, 100), true),
                    (Step::Reading, true),
                    (Step::Fetching(Gpu), true),
                    (Step::Downloading(Gpu, 50), true),
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
                        download("Loading the speech model. Downloading about 650 MB."),
                        download("Downloading the speech model, about 650 MB \u{2014} 0%"),
                        download("Downloading the speech model, about 650 MB \u{2014} 1%"),
                        download("Downloading the speech model, about 650 MB \u{2014} 100%"),
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
                steps.send(Step::Downloading(Export::Cpu, 40)).unwrap();
                assert_eq!(
                    next_loading(&mut status).await.0,
                    "Downloading the speech model, about 650 MB \u{2014} 40%"
                );

                load.abort();
                assert!(load.await.unwrap_err().is_cancelled());
                publisher.loading(None);
                let late = steps.send(Step::Downloading(Export::Cpu, 41));

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
                recognizer, wake, cues, gate, hold,
                http: reqwest::Client::new(),
                notice: publisher.notice_sink(),
                spotter: None,
                spotter_off: None,
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

    #[test]
    fn only_the_recordings_and_the_history_are_taken_without_rebuilding_the_engine() {
        let old = DesktopConfig::from_json_str("{}").unwrap();
        for (change, rebuilds) in [
            (r#"{"recordings": {"text": true}}"#, false),
            (r#"{"history": {"enabled": false}}"#, false),
            (
                r#"{"history": {"limit": 5}, "recordings": {"audio": true}}"#,
                false,
            ),
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
                        recognizer,
                        wake,
                        cues,
                        gate,
                        hold,
                        http: reqwest::Client::new(),
                        notice: publisher.notice_sink(),
                        spotter: None,
                        spotter_off: None,
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
    struct TestHost(Arc<Mutex<Vec<&'static str>>>);

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

        fn overlay_takes_clicks(&self) -> bool {
            false
        }

        fn capture_target(&self) -> Result<String, thalvi_desktop::host::HostError> {
            Ok("Editor".to_owned())
        }

        fn deliver(
            &self,
            _: &String,
            _: &thalvi_desktop::host::PasteText,
            _: bool,
        ) -> Result<thalvi_desktop::host::Delivered, thalvi_desktop::host::HostError> {
            self.called("deliver");
            Ok(thalvi_desktop::host::Delivered::InWindow)
        }

        fn copy(&self, _: &str) -> Result<(), thalvi_desktop::host::HostError> {
            self.called("copy");
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

        fn prepare_overlay(&self, _: f64, _: f64) -> Result<f64, thalvi_desktop::host::HostError> {
            Ok(0.0)
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
        .settle(outcome, ended, &bound, &mut key, |_, _| {})
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
    async fn a_switch_of_the_live_server_waits_for_the_end_of_the_running_capture() {
        let running = DesktopConfig::from_json_str(r#"{"stt": {"engine": "server"}}"#).unwrap();
        let switched = DesktopConfig::from_json_str(r#"{"stt": {"engine": "local"}}"#).unwrap();
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
}

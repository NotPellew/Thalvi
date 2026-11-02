//! Runs the capture engine on its own thread and follows the app's control messages.

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use thalvi_desktop::audio::{Frames, InputGate, Microphone};
use thalvi_desktop::capture::EndingPauses;
use thalvi_desktop::chosen::Chosen;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::delivery::Delivery;
use thalvi_desktop::engine::{
    CaptureOutcome, CpalCuePlayer, Engine, EngineHandle, EngineState, Parts, WakeTrigger,
};
use thalvi_desktop::host::Desktop;
use thalvi_desktop::local::LocalRecognizer;
use thalvi_desktop::metrics::{Event, Metrics, Recorder};
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::transducer::TransducerSource;
use thalvi_desktop::wake::WakeEngine;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, LocalSet};

use crate::status::{Mode, STARTING_NOTICE, Status, View};

/// What the user sees while the speech model downloads or loads.
const LOADING_NOTICE: &str = "Loading the speech model. The first time it downloads about 650 MB.";

/// How long a quit waits for the engine to close the microphone and the recognizer.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// What the app asks of the engine.
#[derive(Debug)]
pub enum Control {
    /// Off closes the microphone; on opens it again.
    SetListening(bool),
    /// Toggle the runner-owned listening intent, including while starting.
    ToggleListening,
    /// Cancels the capture in progress.
    Cancel,
    /// Takes new settings in between captures.
    Apply(Box<DesktopConfig>),
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
    stopped: Mutex<std_mpsc::Receiver<()>>,
}

impl EngineLink {
    /// Starts the engine thread; it loads the wake word models before it listens.
    ///
    /// # Errors
    /// Fails when the thread or its runtime cannot start.
    pub fn start(config: DesktopConfig, host: Desktop) -> Result<Self, String> {
        let (control, control_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(Status::Loading {
            notice: STARTING_NOTICE.to_owned(),
        });
        let (level_tx, level) = watch::channel(0.0);
        let (audio_tx, audio) = watch::channel(AudioStatus {
            selected: config.microphone.clone(),
            active: None,
            paused: false,
        });
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
                    host,
                    control_rx,
                    publisher,
                    Arc::new(level_tx),
                    audio_tx,
                );
                LocalSet::new().block_on(&runtime, runner.run());
                let _ = stopped_tx.send(());
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
            stopped: Mutex::new(stopped),
        })
    }

    pub fn send(&self, control: Control) {
        // The engine thread is gone only while the app quits or after it failed.
        let _ = self.control.send(control);
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

    /// Stops the engine and waits until the microphone and the recognizer are closed.
    ///
    /// Blocks for at most a few seconds.
    pub fn quit(&self) {
        self.send(Control::Quit);
        let stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = stopped.recv_timeout(QUIT_TIMEOUT);
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

    fn loading(&self, notice: Option<&str>) {
        self.update(|view| view.loading = notice.map(str::to_owned));
    }
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
    frames: Option<Frames>,
}

impl Session {
    fn active_device(&self) -> Option<String> {
        self.frames.as_ref()?;
        self.microphone
            .as_ref()
            .and_then(|microphone| microphone.active_name().map(str::to_owned))
    }

    fn close_microphone(&mut self) {
        self.frames = None;
        if let Some(microphone) = &mut self.microphone {
            microphone.pause();
        }
    }

    async fn open_microphone(&mut self) -> Result<(), String> {
        let mut microphone = self.microphone.take().unwrap_or_else(|| {
            Microphone::new(self.gate.clone(), self.device.clone(), self.metrics.clone())
        });
        let (microphone, frames) = tokio::task::spawn_blocking(move || {
            let frames = microphone.open();
            (microphone, frames)
        })
        .await
        .map_err(|error| format!("Thalvi could not open the microphone: {error}"))?;
        self.microphone = Some(microphone);
        self.frames = Some(frames.map_err(|error| error.to_string())?);
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

struct Runner {
    config: DesktopConfig,
    session: Option<Session>,
    paused: bool,
    stopping: bool,
    /// Set while listening wants a microphone it does not have: how many reopen attempts failed.
    reopen_failures: Option<u32>,
    /// Settings that arrived during a capture.
    pending: Option<DesktopConfig>,
    control: mpsc::UnboundedReceiver<Control>,
    publisher: Rc<Publisher>,
    level: Arc<watch::Sender<f32>>,
    audio: watch::Sender<AudioStatus>,
    delivery: Delivery<Desktop>,
    http: reqwest::Client,
    /// Loads the current engine's speech model.
    loading: Option<JoinHandle<()>>,
}

impl Runner {
    fn new(
        config: DesktopConfig,
        host: Desktop,
        control: mpsc::UnboundedReceiver<Control>,
        publisher: Rc<Publisher>,
        level: Arc<watch::Sender<f32>>,
        audio: watch::Sender<AudioStatus>,
    ) -> Self {
        Self {
            config,
            session: None,
            paused: false,
            stopping: false,
            reopen_failures: None,
            pending: None,
            control,
            publisher,
            level,
            audio,
            delivery: Delivery::new(host),
            http: reqwest::Client::new(),
            loading: None,
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
            Err(message) => {
                self.fail(&message);
                self.publish_audio();
            }
        }
    }

    /// Fetches and loads the speech model in the background, and shows that until it is ready.
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
        println!("{LOADING_NOTICE}");
        self.publisher.loading(Some(LOADING_NOTICE));
        let publisher = Rc::clone(&self.publisher);
        self.loading = Some(tokio::task::spawn_local(async move {
            let loaded = local.load().await;
            if let Err(error) = &loaded {
                println!("{error}");
            }
            publisher.update(|view| {
                view.loading = None;
                if let Err(error) = loaded
                    && !matches!(view.mode, Mode::Paused | Mode::Delivering(_))
                {
                    view.mode = Mode::Failed(format!(
                        "{error} Dictation will try again when you start it."
                    ));
                }
            });
        }));
    }

    fn fail(&mut self, message: &str) {
        println!("{message}");
        self.publisher.mode(Mode::Failed(message.to_owned()));
    }

    async fn build(&self, config: &DesktopConfig) -> Result<Session, String> {
        let recorder = Recorder::per_user(config.record).map_err(|error| error.to_string())?;
        let store = ModelStore::per_user().map_err(|error| error.to_string())?;
        println!("Loading the wake word models.");
        let started = Instant::now();
        let wake = WakeEngine::load(&config.wake, &store).await;
        recorder.metric(
            Event::WakeLoad,
            if wake.is_ok() { "ok" } else { "error" },
            Some(started.elapsed()),
            Value::Null,
        );
        let wake = wake.map_err(|error| error.to_string())?;
        let gate = InputGate::default();
        let metrics = recorder.metrics();
        let recognizer = Chosen::new(&config.stt.engine, &config.server, store, metrics.clone());
        let local = recognizer.local();
        let (engine, handle) = Engine::new(
            Parts {
                recognizer,
                wake,
                cues: CpalCuePlayer::new(config.cues, gate.clone()),
                gate: gate.clone(),
                http: self.http.clone(),
                notice: Box::new(|notice| println!("{notice}")),
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
        if local.is_some() {
            println!("Transcribing on this machine.");
        } else {
            println!("Transcribing through {}.", config.server);
        }
        Ok(Session {
            engine,
            local,
            handle,
            gate,
            device: config.microphone.clone(),
            metrics,
            microphone: None,
            frames: None,
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
            Err(message) => self.microphone_failed(&message),
        }
        self.publish_audio();
    }

    /// Shows why listening has no microphone and schedules the next attempt to reopen it.
    fn microphone_failed(&mut self, message: &str) {
        if self.reopen_failures.is_none() {
            self.fail(message);
        } else {
            tracing::warn!(message, "microphone still unavailable");
        }
        self.reopen_failures = Some(self.reopen_failures.map_or(0, |failures| failures + 1));
    }

    async fn step(&mut self) -> Flow {
        if self.stopping {
            return Flow::Quit;
        }
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
            return match next(&mut self.control, retry).await {
                Next::Control(Some(control)) => self.handle(control).await,
                Next::Control(None) => Flow::Quit,
                Next::Reopen => {
                    self.open_microphone().await;
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
            Woke::Trigger(Ok(WakeTrigger::Capture)) => Box::pin(self.capture()).await,
            Woke::Trigger(Ok(WakeTrigger::Conversation)) => {
                println!("Conversation wake heard. This build has no conversation.");
                Flow::Continue
            }
            Woke::Trigger(Err(error)) => {
                tracing::warn!(?error, "listening stopped");
                self.close_microphone();
                // A capture that failed with the microphone has said where its text went.
                if !self.publisher.failed() {
                    self.fail(&error.to_string());
                }
                self.reopen_failures = Some(0);
                Flow::Continue
            }
        }
    }

    /// Acts on a control message while nothing is being captured.
    async fn handle(&mut self, control: Control) -> Flow {
        match control {
            Control::SetListening(on) => self.set_listening(on).await,
            Control::ToggleListening => self.set_listening(self.paused).await,
            Control::Cancel => {}
            Control::Apply(config) => self.apply(*config).await,
            Control::Quit => return Flow::Quit,
        }
        Flow::Continue
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

    /// Replaces the engine by one built from `config`; the old one stays if that fails.
    async fn apply(&mut self, config: DesktopConfig) {
        if config == self.config {
            return;
        }
        self.close_microphone();
        match self.build(&config).await {
            Ok(session) => {
                if let Some(old) = self.session.replace(session) {
                    old.close().await;
                }
                self.publisher
                    .update(|view| view.pauses = config.ending_pause);
                self.config = config;
                self.load_speech_model();
                self.open_microphone().await;
            }
            Err(message) => {
                self.open_microphone().await;
                self.fail(&message);
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
        self.publisher.update(|view| view.target = bound.window());
        let mut requests = Requests::default();
        let outcome = {
            let Self {
                session, control, ..
            } = &mut *self;
            let Some(Session {
                engine,
                handle,
                gate,
                frames: Some(frames),
                ..
            }) = session.as_mut()
            else {
                return Flow::Continue;
            };
            let outcome = Box::pin(drive(
                engine.capture(frames),
                control,
                || handle.cancel(),
                &mut requests,
            ))
            .await;
            if outcome.is_none() {
                // The wake word closed the gate and only a capture opens it again.
                gate.set_enabled(true);
            }
            outcome
        };
        if let Some(config) = requests.pending.take() {
            self.pending = Some(config);
        }
        if requests.quit {
            // Nothing is delivered after a quit.
            return Flow::Quit;
        }
        let Some(outcome) = outcome else {
            self.publisher.mode(Mode::Idle);
            if requests.listening_off {
                self.pause();
            }
            return Flow::Continue;
        };
        let route = match &outcome {
            CaptureOutcome::Accepted(request) => {
                self.publisher.mode(Mode::Delivering(request.route));
                Some(request.route)
            }
            _ => None,
        };
        let finished = match self.session.as_ref() {
            Some(session) => {
                self.delivery
                    .finish(&outcome, &bound, &self.config, session.engine.recorder())
                    .await
            }
            None => None,
        };
        let mode = match (finished, route) {
            (Some(finished), Some(route)) if finished.delivered => {
                println!("{}", finished.notice);
                Mode::Done {
                    route,
                    notice: finished.notice,
                }
            }
            (Some(finished), _) => {
                println!("{}", finished.notice);
                Mode::Failed(finished.notice)
            }
            (None, _) => Mode::Idle,
        };
        self.publisher.mode(mode);
        if requests.listening_off {
            self.pause();
        }
        Flow::Continue
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
}

/// Waits for the next control, or until `retry` has passed.
async fn next(control: &mut mpsc::UnboundedReceiver<Control>, retry: Option<Duration>) -> Next {
    match retry {
        Some(delay) => tokio::select! {
            received = control.recv() => Next::Control(received),
            () = tokio::time::sleep(delay) => Next::Reopen,
        },
        None => Next::Control(control.recv().await),
    }
}

/// What the user asked for while a capture ran.
#[derive(Default)]
struct Requests {
    /// The latest listening request was off.
    listening_off: bool,
    quit: bool,
    /// Settings to apply once the capture is over.
    pending: Option<DesktopConfig>,
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
            Some(Control::Quit) | None => {
                self.quit = true;
                true
            }
        }
    }
}

/// Runs `capture` while following the controls; `None` when a control ended the capture before
/// it started or asked for a cancel as it finished, so its outcome is not to be delivered. The
/// engine ignores a cancel that arrives before it starts, so those are read first, and the
/// capture is polled before any later control.
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
                return (!requests.read_queued(control)).then_some(outcome);
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

    use super::*;

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
    fn a_missing_microphone_is_retried_after_one_two_then_every_five_seconds() {
        let waits: Vec<u64> = (0..6).map(|failures| backoff(failures).as_secs()).collect();
        assert_eq!(waits, [1, 2, 5, 5, 5, 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_microphone_is_reopened_when_its_time_comes_unless_a_control_comes_first() {
        let (sender, mut control) = mpsc::unbounded_channel();
        let started = tokio::time::Instant::now();

        let reopen = tokio::time::timeout(
            Duration::from_secs(10),
            next(&mut control, Some(Duration::from_secs(2))),
        )
        .await
        .expect("the microphone was never reopened");
        assert!(matches!(reopen, Next::Reopen));
        assert_eq!(started.elapsed(), Duration::from_secs(2));

        sender.send(Control::Quit).unwrap();
        let quit = next(&mut control, Some(Duration::from_secs(2))).await;
        assert!(matches!(quit, Next::Control(Some(Control::Quit))));
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(2),
            "a control does not wait"
        );

        let waiting = tokio::time::timeout(Duration::from_secs(60), next(&mut control, None)).await;
        assert!(
            waiting.is_err(),
            "without a retry only a control ends the wait"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_that_arrives_as_the_capture_finishes_keeps_its_outcome_from_delivery() {
        let config = DesktopConfig::from_json_str("{}", None).unwrap();
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
            assert_eq!(outcome.is_some(), delivered, "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_latest_listening_request_wins_and_settings_wait_for_the_end() {
        let config = DesktopConfig::from_json_str("{}", None).unwrap();
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
    fn listening_requests_queued_before_the_microphone_opens_settle_on_the_latest_one() {
        let config = DesktopConfig::from_json_str("{}", None).unwrap();
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
}

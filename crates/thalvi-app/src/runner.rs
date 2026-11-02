//! Runs the capture engine on its own thread and follows the app's control messages.

use std::cell::RefCell;
use std::pin::pin;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use thalvi_desktop::audio::{Frames, InputGate, Microphone};
use thalvi_desktop::capture::EndingPauses;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::delivery::Delivery;
use thalvi_desktop::engine::{
    CaptureOutcome, CpalCuePlayer, Engine, EngineHandle, EngineState, Parts, WakeTrigger,
};
use thalvi_desktop::host::Desktop;
use thalvi_desktop::metrics::{Event, Recorder};
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::recognizer::ServerRecognizer;
use thalvi_desktop::wake::WakeEngine;
use tokio::sync::{mpsc, watch};
use tokio::task::LocalSet;

use crate::status::{Mode, Status, View};

/// How long a quit waits for the engine to close the microphone and the recognizer.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// What the app asks of the engine.
#[derive(Debug)]
pub enum Control {
    /// Off closes the microphone; on opens it again.
    SetListening(bool),
    /// Cancels the capture in progress.
    Cancel,
    /// Takes new settings in between captures.
    Apply(Box<DesktopConfig>),
    /// Cancels any capture, closes the microphone and the recognizer, and stops.
    Quit,
}

/// The app's side of the engine thread.
pub struct EngineLink {
    control: mpsc::UnboundedSender<Control>,
    status: watch::Receiver<Status>,
    level: watch::Receiver<f32>,
    stopped: Mutex<std_mpsc::Receiver<()>>,
}

impl EngineLink {
    /// Starts the engine thread; it loads the wake word models before it listens.
    ///
    /// # Errors
    /// Fails when the thread or its runtime cannot start.
    pub fn start(config: DesktopConfig, host: Desktop) -> Result<Self, String> {
        let (control, control_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(Status::Paused);
        let (level_tx, level) = watch::channel(0.0);
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
                let runner = Runner::new(config, host, control_rx, publisher, Arc::new(level_tx));
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
                mode: Mode::Paused,
                engine: EngineState::Listening,
                target: None,
                pauses,
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

    fn mode(&self, mode: Mode) {
        self.update(|view| view.mode = mode);
    }
}

/// Everything one engine needs; replaced when settings change.
struct Session {
    engine: Engine<ServerRecognizer, WakeEngine, CpalCuePlayer>,
    handle: EngineHandle,
    gate: InputGate,
    device: Option<String>,
    microphone: Option<Microphone>,
    frames: Option<Frames>,
}

impl Session {
    fn close_microphone(&mut self) {
        self.frames = None;
        if let Some(microphone) = &mut self.microphone {
            microphone.pause();
        }
    }

    async fn open_microphone(&mut self) -> Result<(), String> {
        let mut microphone = self
            .microphone
            .take()
            .unwrap_or_else(|| Microphone::new(self.gate.clone(), self.device.clone()));
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
    /// Settings that arrived during a capture.
    pending: Option<DesktopConfig>,
    control: mpsc::UnboundedReceiver<Control>,
    publisher: Rc<Publisher>,
    level: Arc<watch::Sender<f32>>,
    delivery: Delivery<Desktop>,
    http: reqwest::Client,
}

impl Runner {
    fn new(
        config: DesktopConfig,
        host: Desktop,
        control: mpsc::UnboundedReceiver<Control>,
        publisher: Rc<Publisher>,
        level: Arc<watch::Sender<f32>>,
    ) -> Self {
        Self {
            config,
            session: None,
            paused: false,
            pending: None,
            control,
            publisher,
            level,
            delivery: Delivery::new(host),
            http: reqwest::Client::new(),
        }
    }

    async fn run(mut self) {
        let config = self.config.clone();
        self.start(config).await;
        while let Flow::Continue = Box::pin(self.step()).await {}
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
                self.publisher.mode(Mode::Idle);
                self.open_microphone().await;
            }
            Err(message) => self.fail(&message),
        }
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
        let (engine, handle) = Engine::new(
            Parts {
                recognizer: ServerRecognizer::new(config.server.clone()),
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
            handle.clone(),
            Rc::clone(&self.publisher),
            Arc::clone(&self.level),
        ));
        println!("Transcribing through {}.", config.server);
        Ok(Session {
            engine,
            handle,
            gate,
            device: config.microphone.clone(),
            microphone: None,
            frames: None,
        })
    }

    /// Opens the microphone unless listening is paused; a failure becomes the status.
    async fn open_microphone(&mut self) {
        if self.paused {
            self.publisher.mode(Mode::Paused);
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match session.open_microphone().await {
            Ok(()) => self.publisher.mode(Mode::Idle),
            Err(message) => self.fail(&message),
        }
    }

    async fn step(&mut self) -> Flow {
        if let Some(config) = self.pending.take() {
            self.apply(config).await;
        }
        let listening = !self.paused
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.frames.is_some());
        if !listening {
            return match self.control.recv().await {
                Some(control) => self.handle(control).await,
                None => Flow::Quit,
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
                if let Some(session) = self.session.as_mut() {
                    session.close_microphone();
                }
                self.fail(&error.to_string());
                Flow::Continue
            }
        }
    }

    /// Acts on a control message while nothing is being captured.
    async fn handle(&mut self, control: Control) -> Flow {
        match control {
            Control::SetListening(true) => {
                self.paused = false;
                let open = self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.frames.is_some());
                if open {
                    self.publisher.mode(Mode::Idle);
                } else if self.session.is_some() {
                    self.open_microphone().await;
                } else {
                    let config = self.config.clone();
                    self.start(config).await;
                }
            }
            Control::SetListening(false) => self.pause(),
            Control::Cancel => {}
            Control::Apply(config) => self.apply(*config).await,
            Control::Quit => return Flow::Quit,
        }
        Flow::Continue
    }

    fn pause(&mut self) {
        self.paused = true;
        if let Some(session) = self.session.as_mut() {
            session.close_microphone();
        }
        self.publisher.mode(Mode::Paused);
    }

    /// Replaces the engine by one built from `config`; the old one stays if that fails.
    async fn apply(&mut self, config: DesktopConfig) {
        if config == self.config {
            return;
        }
        if let Some(session) = self.session.as_mut() {
            // The queue would overflow while the models load.
            session.close_microphone();
        }
        match self.build(&config).await {
            Ok(session) => {
                if let Some(old) = self.session.replace(session) {
                    old.close().await;
                }
                self.publisher
                    .update(|view| view.pauses = config.ending_pause);
                self.config = config;
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
        let mut pause = false;
        let mut quit = false;
        let outcome = {
            let Self {
                session,
                control,
                pending,
                ..
            } = &mut *self;
            let Some(Session {
                engine,
                handle,
                frames: Some(frames),
                ..
            }) = session.as_mut()
            else {
                return Flow::Continue;
            };
            let mut capture = pin!(engine.capture(frames));
            loop {
                tokio::select! {
                    outcome = &mut capture => break outcome,
                    received = control.recv(), if !quit => match received {
                        Some(Control::Cancel) => handle.cancel(),
                        Some(Control::SetListening(false)) => {
                            pause = true;
                            handle.cancel();
                        }
                        Some(Control::SetListening(true)) => {}
                        Some(Control::Apply(config)) => *pending = Some(*config),
                        Some(Control::Quit) | None => {
                            quit = true;
                            handle.cancel();
                        }
                    },
                }
            }
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
        if quit {
            return Flow::Quit;
        }
        if pause {
            self.pause();
        }
        Flow::Continue
    }
}

/// Carries one engine's state and level changes to the publisher until the engine is dropped.
async fn follow(
    handle: EngineHandle,
    publisher: Rc<Publisher>,
    level_out: Arc<watch::Sender<f32>>,
) {
    let mut state = handle.state();
    let mut level = handle.level();
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
                level_out.send_replace(*level.borrow_and_update());
            }
        }
    }
}

//! Explores runner controls across captures with a virtual clock and the engine's real fakes.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use proptest::prelude::*;
use thalvi_desktop::audio::AudioError;
use thalvi_desktop::engine::MIN_HOLD;
use thalvi_desktop::engine::fakes::{FakeCuePlayer, FakeFrames, FakeWake, FrameFeed, WakeControl};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::recognizer::{FakeControl, FakeRecognizer, RecognizerError, RecognizerEvent};
use thalvi_desktop::wake::WakeEvent;

use super::tests::TestHost;
use super::*;
use crate::status::LastError;

#[derive(Default)]
struct Devices {
    wake: Option<WakeControl>,
    feed: Option<FrameFeed>,
}

#[derive(Default)]
struct Recognition {
    server: Option<FakeControl>,
    capture: Option<usize>,
    next_capture: usize,
}

#[derive(Clone, Default)]
struct FakeRig {
    devices: Rc<RefCell<Devices>>,
    recognition: Arc<Mutex<Recognition>>,
}

struct Connections {
    recognizer: FakeRecognizer,
    recognition: Arc<Mutex<Recognition>>,
}

impl Recognizer for Connections {
    type Session = <FakeRecognizer as Recognizer>::Session;

    async fn connect(&self) -> Result<Self::Session, RecognizerError> {
        {
            let mut recognition = self.recognition.lock().unwrap();
            recognition.capture = Some(recognition.next_capture);
            recognition.next_capture += 1;
            recognition.server.as_mut().unwrap().connect_again();
        }
        self.recognizer.connect().await
    }
}

impl Rig for FakeRig {
    type Recognizer = Connections;
    type Wake = FakeWake;
    type Cues = FakeCuePlayer;
    type Input = FakeInput;
    type Host = TestHost;

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "The fake loads only in-memory devices."
    )]
    async fn load(
        &self,
        _: &DesktopConfig,
        _: &InputGate,
        _: &Rc<Publisher>,
    ) -> Result<Loaded<Self>, Failure> {
        let (recognizer, server) = FakeRecognizer::new();
        let (wake, control) = FakeWake::new();
        let (cues, _) = FakeCuePlayer::new();
        self.recognition.lock().unwrap().server = Some(server);
        self.devices.borrow_mut().wake = Some(control);
        Ok(Loaded {
            recognizer: Connections {
                recognizer,
                recognition: Arc::clone(&self.recognition),
            },
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
        })
    }

    fn input(&self, gate: InputGate, device: Option<String>, _: Metrics) -> FakeInput {
        FakeInput {
            rig: self.clone(),
            gate,
            selected: device,
            active: None,
        }
    }
}

struct FakeInput {
    rig: FakeRig,
    gate: InputGate,
    selected: Option<String>,
    active: Option<String>,
}

impl Input for FakeInput {
    type Frames = FakeFrames;

    fn active_name(&self) -> Option<&str> {
        self.active.as_deref()
    }

    fn select(&mut self, device: Option<String>) {
        self.selected = device;
    }

    fn pause(&mut self) {
        self.active = None;
        self.rig.devices.borrow_mut().feed = None;
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "The fake opens an in-memory frame channel inline."
    )]
    async fn open(mut self) -> Result<(Self, Result<FakeFrames, Failure>), Failure> {
        let (frames, feed) = FakeFrames::new(&self.gate);
        self.rig.devices.borrow_mut().feed = Some(feed);
        self.active = Some(
            self.selected
                .clone()
                .unwrap_or_else(|| "Default".to_owned()),
        );
        Ok((self, Ok(frames)))
    }

    async fn replace(self) -> Result<(Self, Result<FakeFrames, Failure>), Failure> {
        self.open().await
    }
}

#[derive(Clone, Copy, Debug)]
enum Apply {
    Wake,
    Recordings,
    History,
    EndingPause,
    Cues,
    Microphone,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Wake,
    HoldDown,
    HoldUp(u16),
    Speech(u8),
    Silence(u8),
    Ready,
    Answer(bool),
    Fail,
    Cancel,
    Toggle,
    Listening(bool),
    Apply(Apply),
    Restart(bool),
    DeviceLost,
    Quiet,
    Quit,
}

fn actions() -> impl Strategy<Value = Vec<Action>> {
    let apply = prop_oneof![
        Just(Apply::Wake),
        Just(Apply::Recordings),
        Just(Apply::History),
        Just(Apply::EndingPause),
        Just(Apply::Cues),
        Just(Apply::Microphone),
    ]
    .prop_map(Action::Apply);
    let minimum = u16::try_from(MIN_HOLD.as_millis()).unwrap();
    let action = prop_oneof![
        9 => Just(Action::Wake),
        6 => Just(Action::HoldDown),
        6 => prop_oneof![Just(0), Just(100), Just(minimum - 1), Just(minimum), Just(500)].prop_map(Action::HoldUp),
        6 => (1_u8..6).prop_map(Action::Speech),
        9 => (1_u8..24).prop_map(Action::Silence),
        3 => Just(Action::Ready),
        12 => any::<bool>().prop_map(Action::Answer),
        3 => Just(Action::Fail),
        7 => Just(Action::Cancel),
        4 => Just(Action::Toggle),
        5 => any::<bool>().prop_map(Action::Listening),
        5 => apply,
        2 => any::<bool>().prop_map(Action::Restart),
        2 => Just(Action::DeviceLost),
        3 => Just(Action::Quiet),
        1 => Just(Action::Quit),
    ];
    proptest::collection::vec(action, 0..80).prop_map(|mut actions| {
        if let Some(quit) = actions
            .iter()
            .position(|action| matches!(action, Action::Quit))
        {
            actions.truncate(quit + 1);
        }
        actions
    })
}

fn resting(status: &Status) -> bool {
    matches!(
        status,
        Status::Paused
            | Status::Listening
            | Status::KeyboardOnly
            | Status::Failed { .. }
            | Status::Done { .. }
    )
}

// These private engine/recognizer limits bound readiness, lost audio and a buffered final.
const READY_TIMEOUT: Duration = Duration::from_secs(8);
const MIC_TIMEOUT: Duration = Duration::from_secs(3);
const BUFFERED_FINAL_WAIT: Duration = Duration::from_secs(11);

fn quiet_bound(config: &DesktopConfig) -> Duration {
    READY_TIMEOUT
        + MIC_TIMEOUT
        + BUFFERED_FINAL_WAIT
        + config
            .ending_pause
            .get(CaptureRoute::Submit)
            .max(config.ending_pause.get(CaptureRoute::Paste))
        + backoff(u32::MAX)
        + QUIT_TIMEOUT
}

struct Case {
    link: EngineLink,
    task: JoinHandle<()>,
    rig: FakeRig,
    host: TestHost,
    config: DesktopConfig,
    last_error: LastError,
    emitted: BTreeSet<String>,
    cancelled: BTreeSet<String>,
    held: bool,
}

impl Case {
    async fn start(path: PathBuf) -> Self {
        let config = DesktopConfig::from_json_str(r#"{"history":{"enabled":false}}"#).unwrap();
        let (link, ends, _) = EngineLink::wire(&config, false);
        let rig = FakeRig::default();
        let host = TestHost::default();
        let task = tokio::task::spawn_local(ends.run(
            config.clone(),
            false,
            rig.clone(),
            Arc::new(Delivery::new(host.clone())),
            Arc::new(History::at(path.join("history.json"))),
            Arc::new(Drafts::default()),
        ));
        let mut case = Self {
            link,
            task,
            rig,
            host,
            config,
            last_error: LastError::default(),
            emitted: BTreeSet::new(),
            cancelled: BTreeSet::new(),
            held: false,
        };
        case.synchronize().await;
        case.observe();
        case
    }

    async fn synchronize(&self) {
        tokio::time::timeout(quiet_bound(&self.config), async {
            self.checkpoint().await;
            // One blocking worker makes this a FIFO barrier for host calls and history writes.
            tokio::task::spawn_blocking(|| ()).await.unwrap();
            self.checkpoint().await;
            tokio::task::yield_now().await;
            let mut updates = self.link.status_updates();
            while matches!(
                *updates.borrow_and_update(),
                Status::Working {
                    cancellable: false,
                    ..
                }
            ) {
                updates.changed().await.unwrap();
            }
            tokio::task::spawn_blocking(|| ()).await.unwrap();
        })
        .await
        .expect("never stuck: delivery or history did not finish");
    }

    async fn checkpoint(&self) {
        let (reply, received) = oneshot::channel();
        self.link.send(Control::RestartIfIdle(Restart::new(
            |_| Err("Observation only.".to_owned()),
            reply,
        )));
        let result = tokio::time::timeout(quiet_bound(&self.config), received)
            .await
            .expect("never stuck: the runner stopped answering controls")
            .expect("the runner closed its control reply");
        assert!(matches!(result, Stop::Busy | Stop::Refused(_)));
    }

    fn observe(&mut self) {
        let status = self.link.status();
        self.last_error.record(&status);
        if matches!(status, Status::Failed { .. }) {
            let failure = self
                .last_error
                .get()
                .expect("errors are told: missing LastError");
            assert!(
                !failure.notice.trim().is_empty(),
                "errors are told: empty notice"
            );
            for artifact in ["{", "Error(", "::"] {
                assert!(
                    !failure.notice.contains(artifact),
                    "errors are told: {:?}",
                    failure.notice
                );
            }
        }
        let mut delivered = BTreeSet::new();
        for text in self.host.1.lock().unwrap().iter() {
            let ids: BTreeSet<_> = text
                .split_whitespace()
                .filter(|word| word.starts_with("capture"))
                .collect();
            assert!(!ids.is_empty(), "delivered once: no emitted id in {text:?}");
            for id in ids {
                assert!(
                    !self.cancelled.contains(id),
                    "cancel and pause win: {id} reached the host"
                );
                assert!(
                    self.emitted.contains(id),
                    "delivered once: {id} was never emitted"
                );
                assert!(
                    delivered.insert(id.to_owned()),
                    "delivered once: {id} reached the host twice"
                );
            }
        }
    }

    fn stop_capture(&mut self) {
        let status = self.link.status();
        if !resting(&status)
            && !matches!(
                status,
                Status::Working {
                    cancellable: false,
                    ..
                }
            )
            && let Some(id) = self.rig.recognition.lock().unwrap().capture
        {
            let id = format!("capture{id}");
            if !self
                .host
                .1
                .lock()
                .unwrap()
                .iter()
                .any(|text| text.split_whitespace().any(|word| word == id))
            {
                self.cancelled.insert(id);
            }
        }
    }

    async fn frames(&mut self, count: u8, speech: bool) {
        for _ in 0..count {
            let mut frame: Frame = [0; FRAME_SAMPLES];
            if speech {
                for (i, sample) in frame.iter_mut().enumerate() {
                    *sample = if i % 100 < 50 { 4_000 } else { -4_000 };
                }
            }
            if let Some(feed) = &self.rig.devices.borrow().feed {
                feed.send(frame);
            }
            self.synchronize().await;
            tokio::time::advance(Duration::from_millis(80)).await;
            self.synchronize().await;
            self.observe();
        }
    }

    fn apply(&mut self, change: Apply) {
        match change {
            Apply::Wake => {
                self.config.wake = self
                    .config
                    .wake
                    .clone()
                    .with_enabled(!self.config.wake.enabled());
            }
            Apply::Recordings => {
                self.config.recordings =
                    serde_json::from_value(json!({"metrics": !self.config.recordings.metrics}))
                        .unwrap();
            }
            Apply::History => {
                self.config.history =
                    serde_json::from_value(json!({"enabled": !self.config.history.enabled()}))
                        .unwrap();
            }
            Apply::EndingPause => {
                self.config.ending_pause = serde_json::from_value(
                    if self.config.ending_pause.get(CaptureRoute::Paste)
                        == Duration::from_millis(200)
                    {
                        json!({"paste":0.7,"submit":2.5})
                    } else {
                        json!({"paste":0.2,"submit":0.5})
                    },
                )
                .unwrap();
            }
            Apply::Cues => self.config.cues.wake = !self.config.cues.wake,
            Apply::Microphone => {
                self.config.microphone = if self.config.microphone.is_some() {
                    None
                } else {
                    Some("Other".to_owned())
                }
            }
        }
        self.link
            .send(Control::Apply(Box::new(self.config.clone())));
    }

    async fn action(&mut self, action: Action) -> bool {
        let starts = resting(&self.link.status())
            && match action {
                Action::Wake => self.link.audio().active.is_some() && self.config.wake.enabled(),
                Action::HoldDown => !self.held,
                _ => false,
            };
        let mut updates = self.link.status_updates();
        if self.link.audio().active.is_none() {
            updates.borrow_and_update();
        }
        if !self.perform(action).await {
            return false;
        }
        if starts {
            tokio::time::timeout(quiet_bound(&self.config), async {
                loop {
                    updates.changed().await.unwrap();
                    if !matches!(
                        *updates.borrow_and_update(),
                        Status::Listening | Status::KeyboardOnly | Status::Done { .. }
                    ) {
                        break;
                    }
                }
            })
            .await
            .expect("the trigger was neither taken nor refused");
        }
        self.synchronize().await;
        self.observe();
        // A wake result belongs to this frame; an unscored one cannot wake a later capture.
        self.rig
            .devices
            .borrow()
            .wake
            .as_ref()
            .unwrap()
            .score_and_detect(0.0, 1.0);
        true
    }

    async fn perform(&mut self, action: Action) -> bool {
        match action {
            Action::Wake => {
                self.rig
                    .devices
                    .borrow()
                    .wake
                    .as_ref()
                    .unwrap()
                    .fire(WakeEvent::Capture);
                let mut frame = [0; FRAME_SAMPLES];
                frame[0] = 1_500;
                if let Some(feed) = &self.rig.devices.borrow().feed {
                    feed.send(frame);
                }
            }
            Action::HoldDown => {
                self.link.hold_input().set(true);
                self.held = true;
            }
            Action::HoldUp(millis) => {
                tokio::time::advance(Duration::from_millis(u64::from(millis))).await;
                self.link.hold_input().set(false);
                self.held = false;
            }
            Action::Speech(count) => self.frames(count, true).await,
            Action::Silence(count) => self.frames(count, false).await,
            Action::Ready => self
                .rig
                .recognition
                .lock()
                .unwrap()
                .server
                .as_mut()
                .unwrap()
                .ready(),
            Action::Answer(ending) => {
                let mut recognition = self.rig.recognition.lock().unwrap();
                let id = format!(
                    "capture{}",
                    recognition.capture.unwrap_or(recognition.next_capture)
                );
                self.emitted.insert(id.clone());
                let text = if ending && !self.held {
                    format!("{id} paste that")
                } else {
                    id
                };
                let server = recognition.server.as_mut().unwrap();
                server.ready();
                server.emit(Ok(RecognizerEvent::Final(text)));
            }
            Action::Fail => {
                let mut recognition = self.rig.recognition.lock().unwrap();
                let server = recognition.server.as_mut().unwrap();
                server.fail("Speech recognition disconnected. Nothing was delivered.");
                server.emit(Err(RecognizerError::said(
                    "Speech recognition disconnected. Nothing was delivered.",
                )));
            }
            Action::Cancel => {
                self.stop_capture();
                self.link.send(Control::Cancel);
            }
            Action::Toggle => {
                if !self.link.audio().paused {
                    self.stop_capture();
                }
                self.link.send(Control::ToggleListening);
            }
            Action::Listening(on) => {
                if !on {
                    self.stop_capture();
                }
                self.link.send(Control::SetListening(on));
            }
            Action::Apply(change) => self.apply(change),
            Action::Restart(refuse) => {
                if !self.restart(refuse).await {
                    return false;
                }
            }
            Action::DeviceLost => {
                if let Some(feed) = &self.rig.devices.borrow().feed {
                    feed.fail(AudioError::DeviceLost);
                }
            }
            Action::Quiet => self.quiet().await,
            Action::Quit => {
                self.link.send(Control::Quit);
                self.stopped().await;
                return false;
            }
        }
        true
    }

    async fn restart(&mut self, refuse: bool) -> bool {
        let (reply, received) = oneshot::channel();
        self.link.send(Control::RestartIfIdle(Restart::new(
            move |_| {
                if refuse {
                    Err("The update could not be prepared.".to_owned())
                } else {
                    Ok(())
                }
            },
            reply,
        )));
        let result = tokio::time::timeout(quiet_bound(&self.config), received)
            .await
            .unwrap()
            .unwrap();
        if matches!(result, Stop::Stopped) {
            self.stopped().await;
            return false;
        }
        assert!(matches!(result, Stop::Busy | Stop::Refused(_)));
        true
    }

    async fn quiet(&mut self) {
        self.link.hold_input().set(false);
        self.held = false;
        self.synchronize().await;
        let mut updates = self.link.status_updates();
        let quiet_result = tokio::time::timeout(quiet_bound(&self.config), async {
            while !resting(&updates.borrow_and_update()) {
                updates.changed().await.unwrap();
            }
        })
        .await;
        self.synchronize().await;
        quiet_result
            .unwrap_or_else(|error| panic!("never stuck: {:?}; {error}", self.link.status()));
        self.observe();
        assert!(
            resting(&self.link.status()),
            "never stuck: {:?}",
            self.link.status()
        );
    }

    async fn stopped(&mut self) {
        tokio::time::timeout(QUIT_TIMEOUT, &mut self.task)
            .await
            .expect("quit ends: the runner exceeded QUIT_TIMEOUT")
            .unwrap();
        assert!(
            self.rig.devices.borrow().feed.is_none(),
            "quit ends: the microphone stayed open"
        );
        tokio::time::timeout(QUIT_TIMEOUT, tokio::task::spawn_blocking(|| ()))
            .await
            .expect("quit ends: history did not finish")
            .unwrap();
        self.observe();
    }
}

fn run(actions: &[Action]) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "t525-impl-b-case-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .max_blocking_threads(1)
        .build()
        .unwrap();
    LocalSet::new().block_on(&runtime, async {
        let mut case = Case::start(path.clone()).await;
        for &action in actions {
            if !case.action(action).await {
                return;
            }
        }
        case.quiet().await;
        case.action(Action::Quit).await;
    });
    drop(runtime);
    if path.exists() {
        std::fs::remove_dir_all(path).unwrap();
    }
}

proptest! {
    #[test]
    fn random_controls_leave_the_runner_responsive_and_deliver_each_uncancelled_capture_once(sequence in actions()) {
        run(&sequence);
    }
}

#[test]
fn rebuilding_after_an_unready_capture_publishes_the_new_engines_resting_state() {
    run(&[Action::Wake, Action::Apply(Apply::Cues), Action::Quiet]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    LocalSet::new().block_on(&runtime, async {
        let (link, ends, _) = EngineLink::wire(&DesktopConfig::from_json_str("{}").unwrap(), false);
        let publisher = Publisher::new(ends.status, EndingPauses::default());
        publisher.update(|view| {
            view.mode = Mode::Idle;
            view.engine = EngineState::StartingCapture;
        });
        let (_state, state) = watch::channel(EngineState::Listening);
        let (_level, level) = watch::channel(0.0);
        let follower = tokio::task::spawn_local(follow(
            state,
            level,
            publisher,
            ends.level,
            ends.audio.subscribe(),
        ));
        let mut status = link.status_updates();
        tokio::time::timeout(QUIT_TIMEOUT, async {
            while *status.borrow_and_update() != Status::Listening {
                status.changed().await.unwrap();
            }
        })
        .await
        .expect("the replacement engine never published its initial Listening state");
        follower.abort();
        assert!(follower.await.unwrap_err().is_cancelled());
    });
}

#[test]
fn cancelling_the_next_capture_does_not_cancel_text_already_delivered() {
    run(&[
        Action::Answer(false),
        Action::Silence(9),
        Action::Silence(18),
        Action::HoldDown,
        Action::Speech(5),
        Action::Wake,
        Action::HoldUp(300),
        Action::Answer(true),
        Action::Ready,
        Action::Fail,
        Action::HoldUp(299),
        Action::Wake,
        Action::HoldDown,
        Action::Answer(false),
        Action::Cancel,
        Action::Quiet,
        Action::Apply(Apply::Recordings),
        Action::Speech(5),
        Action::Silence(10),
        Action::HoldUp(300),
        Action::Listening(false),
        Action::Silence(14),
        Action::Silence(6),
        Action::Apply(Apply::Recordings),
        Action::Toggle,
        Action::Wake,
        Action::Wake,
        Action::Restart(false),
        Action::DeviceLost,
        Action::Speech(3),
        Action::DeviceLost,
        Action::Speech(2),
        Action::Answer(false),
        Action::Cancel,
        Action::Listening(false),
        Action::Apply(Apply::EndingPause),
        Action::Answer(false),
        Action::Cancel,
        Action::Cancel,
        Action::Answer(false),
        Action::Restart(true),
        Action::Wake,
        Action::Answer(false),
        Action::HoldDown,
        Action::HoldDown,
        Action::Speech(5),
        Action::Speech(2),
    ]);
}

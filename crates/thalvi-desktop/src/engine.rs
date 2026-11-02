//! Coordinates wake scoring, voice activity, cues, the recognizer and the silence timer into typed capture outcomes.
//!
//! One `!Send` owner runs on a current-thread runtime and `LocalSet`, so the voice activity and
//! wake detectors never cross a thread and every timer follows the runtime's clock.

pub mod fakes;

use std::collections::VecDeque;
use std::future::{Future, pending};
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{FutureExt, LocalBoxFuture};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::audio::{AudioError, Frames, InputGate};
use crate::capture::{
    CaptureFullError, CaptureRequest, CaptureRoute, CaptureSession, Endings, FinalStep,
    PauseDuration, QuietWindow,
};
use crate::config::DesktopConfig;
use crate::cues::{Cue, CueEngine, CueError, Cues};
use crate::frame::Frame;
use crate::metrics::{Event, Recorder};
use crate::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerSession};
use crate::secondary::{self, SecondaryApi, SecondaryError, SecondaryStt};
use crate::vad::VadDetector;
use crate::wake::{WakeEngine, WakeError, WakeEvent, WakeFrame};

/// How long the recognizer and the wake cue have to be ready before the capture is abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a pending ending may go without microphone audio before the capture fails.
const MIC_WATCHDOG: Duration = Duration::from_millis(250);

/// Consecutive speech frames, 240 ms, that make audio held back during a pending ending count as
/// the user continuing.
const CONTINUATION_FRAMES: u8 = 3;

/// Frames in one wake level report window, 2 s.
const LEVEL_WINDOW: u64 = 25;

/// The sample peak from which a wake level window counts as activity.
const ACTIVITY_PEAK: u16 = 1_000;

/// Microphone frames in order.
pub trait FrameSource {
    /// Waits for the next frame; safe to drop and call again.
    fn next(&mut self) -> impl Future<Output = Result<Frame, AudioError>>;
}

impl FrameSource for Frames {
    async fn next(&mut self) -> Result<Frame, AudioError> {
        Frames::next(self).await
    }
}

/// Scores microphone frames for the wake words.
pub trait WakeDetector {
    /// Scores the next frame.
    ///
    /// # Errors
    /// Fails when the wake models cannot score.
    fn process(&mut self, frame: &Frame) -> Result<WakeFrame, WakeError>;

    /// Forgets the audio so far.
    fn reset(&mut self);
}

impl WakeDetector for WakeEngine {
    fn process(&mut self, frame: &Frame) -> Result<WakeFrame, WakeError> {
        WakeEngine::process(self, frame)
    }

    fn reset(&mut self) {
        WakeEngine::reset(self);
    }
}

/// Plays one cue to its end.
pub trait CuePlayer {
    /// Resolves when the cue has finished or failed; dropping the future cuts the cue short.
    fn play(&self, cue: Cue) -> impl Future<Output = Result<(), CueError>>;
}

/// Plays cues on the default output through [`CueEngine`].
pub struct CpalCuePlayer(Arc<CueEngine>);

impl CpalCuePlayer {
    /// Plays the cues switched on in `cues`, muting `gate` while each plays.
    #[must_use]
    pub fn new(cues: Cues, gate: InputGate) -> Self {
        Self(Arc::new(CueEngine::new(cues, gate)))
    }
}

/// Stops the cue that is playing unless disarmed.
struct StopOnDrop(Option<Arc<CueEngine>>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(engine) = self.0.take() {
            engine.stop();
        }
    }
}

impl CuePlayer for CpalCuePlayer {
    async fn play(&self, cue: Cue) -> Result<(), CueError> {
        let engine = Arc::clone(&self.0);
        let mut stop = StopOnDrop(Some(Arc::clone(&self.0)));
        let played = tokio::task::spawn_blocking(move || engine.play(cue)).await;
        stop.0 = None;
        // A panic in the playback thread leaves the output in an unknown state.
        played.unwrap_or(Err(CueError::Stalled))
    }
}

/// What the engine does with a wake word, for the caller to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeTrigger {
    /// The caller binds its capture target, then calls [`Engine::capture`].
    Capture,
    /// The caller starts a conversation.
    Conversation,
}

/// What the engine is doing right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineState {
    Listening,
    /// The wake cue plays while the recognizer connects.
    StartingCapture,
    Capturing {
        committed: String,
        partial: Option<String>,
    },
    PendingEnding {
        route: CaptureRoute,
        /// What will be delivered.
        text: String,
        /// The quiet pause has not started because the user is still speaking.
        waiting_for_quiet: bool,
    },
    TranscribingSecondary {
        route: CaptureRoute,
    },
}

/// Why a capture failed; `Display` is the plain notice for the user.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error(transparent)]
    Recognizer(#[from] RecognizerError),
    #[error("Speech recognition was not ready in time. Nothing was delivered.")]
    NotReady,
    #[error(transparent)]
    Audio(#[from] AudioError),
    #[error("Microphone audio stopped. Nothing was delivered.")]
    MicStopped,
    #[error(transparent)]
    Full(#[from] CaptureFullError),
}

/// How a capture ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// The user ended the capture; nothing has been delivered yet.
    Accepted(CaptureRequest),
    /// The user cancelled it, by voice or through [`EngineHandle::cancel`].
    Cancelled,
    Failed {
        error: CaptureError,
        /// What the user had dictated so far, for the caller to keep.
        recoverable_text: Option<String>,
    },
}

/// Why listening stopped; `Display` is the plain notice for the user.
#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error(transparent)]
    Audio(#[from] AudioError),
    #[error(transparent)]
    Wake(#[from] WakeError),
}

/// The part of the engine other threads may hold: cancel a capture and watch the state.
#[derive(Clone)]
pub struct EngineHandle {
    cancel: Arc<watch::Sender<u64>>,
    state: watch::Receiver<EngineState>,
}

impl EngineHandle {
    /// Cancels the capture in progress; does nothing while listening.
    pub fn cancel(&self) {
        self.cancel
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    #[must_use]
    pub fn state(&self) -> watch::Receiver<EngineState> {
        self.state.clone()
    }
}

/// What the engine runs on.
pub struct Parts<R, W, C> {
    pub recognizer: R,
    pub wake: W,
    pub cues: C,
    /// The gate of the microphone `frames` come from.
    pub gate: InputGate,
    pub http: reqwest::Client,
    /// Tells the user about something that did not stop the capture.
    pub notice: Box<dyn Fn(&str)>,
}

/// Wake listening and capture, owning everything they share.
pub struct Engine<R, W, C> {
    recognizer: R,
    wake: W,
    cues: Rc<C>,
    cues_on: Cues,
    gate: InputGate,
    vad: VadDetector,
    endings: Endings,
    pause: PauseDuration,
    secondary: Option<SecondaryStt>,
    http: reqwest::Client,
    recorder: Recorder,
    notice: Box<dyn Fn(&str)>,
    state: watch::Sender<EngineState>,
    /// Keeps the cancel channel open so waiting on it never ends.
    _cancel_tx: Arc<watch::Sender<u64>>,
    cancel: watch::Receiver<u64>,
    level_frames: u64,
    level_activity: bool,
    ambiguous: bool,
}

/// How a cue ended.
struct CueEnd {
    cue: Cue,
    played: bool,
    took: Duration,
    result: Result<(), CueError>,
}

/// One capture in progress, after the recognizer is ready.
struct Live<S> {
    session: S,
    text: CaptureSession,
    partial: Option<String>,
    /// Frames the recognizer received, kept only when something will use them.
    sent: Vec<Frame>,
    keep_sent: bool,
    /// Frames withheld while an ending is pending, until the user is heard continuing.
    held: VecDeque<Frame>,
    speech_run: u8,
    relayed: bool,
    /// Whether the latest frame was speech.
    speech: bool,
    last_audio: Instant,
    cues: FuturesUnordered<LocalBoxFuture<'static, CueEnd>>,
}

/// Why sending audio stopped the capture.
enum Stop {
    Cancelled,
    Failed(CaptureError),
}

impl From<Stop> for Done {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Cancelled => Self::Stopped,
            Stop::Failed(error) => Self::Failed(error),
        }
    }
}

/// Why the capture loop stopped.
enum Done {
    /// [`EngineHandle::cancel`].
    Stopped,
    /// The user said "never mind".
    NeverMind,
    Accepted(CaptureRequest),
    Failed(CaptureError),
}

#[expect(
    clippy::large_enum_variant,
    reason = "one short-lived value per pass of the capture loop"
)]
enum Step {
    Cancel,
    Event(Result<RecognizerEvent, RecognizerError>),
    Frame(Result<Frame, AudioError>),
    Cue(CueEnd),
    Quiet,
    Silent,
}

async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => pending().await,
    }
}

fn cue_name(cue: Cue) -> &'static str {
    match cue {
        Cue::Wake => "wake",
        Cue::Provisional => "provisional",
        Cue::Cancelled => "cancelled",
        Cue::Accepted => "accepted",
        Cue::Error => "error",
    }
}

/// What the user dictated, for recovery: the armed ending's text, else everything so far.
fn recoverable(text: &CaptureSession) -> Option<String> {
    let kept = text
        .pending()
        .map_or_else(|| text.text(), |pending| pending.text.clone());
    (!kept.is_empty()).then_some(kept)
}

impl<R, W, C> Engine<R, W, C>
where
    R: Recognizer,
    W: WakeDetector,
    C: CuePlayer + 'static,
{
    #[must_use]
    pub fn new(
        parts: Parts<R, W, C>,
        config: &DesktopConfig,
        recorder: Recorder,
    ) -> (Self, EngineHandle) {
        let (state, state_rx) = watch::channel(EngineState::Listening);
        let (cancel_tx, cancel) = watch::channel(0);
        let cancel_tx = Arc::new(cancel_tx);
        let handle = EngineHandle {
            cancel: Arc::clone(&cancel_tx),
            state: state_rx,
        };
        let engine = Self {
            recognizer: parts.recognizer,
            wake: parts.wake,
            cues: Rc::new(parts.cues),
            cues_on: config.cues,
            gate: parts.gate,
            vad: VadDetector::new(),
            endings: config.endings.clone(),
            pause: config.ending_pause,
            secondary: config.stt.secondary.clone(),
            http: parts.http,
            recorder,
            notice: parts.notice,
            state,
            _cancel_tx: cancel_tx,
            cancel,
            level_frames: 0,
            level_activity: false,
            ambiguous: false,
        };
        (engine, handle)
    }

    /// Writes the recordings still queued; call once at shutdown.
    pub async fn finish(self) {
        self.recorder.finish().await;
    }

    /// Scores microphone frames until a wake word is heard.
    ///
    /// On [`WakeTrigger::Capture`] the microphone gate is already off, so nothing said before
    /// the wake cue reaches the capture; call [`capture`](Self::capture) next.
    ///
    /// # Errors
    /// Fails when the microphone or the wake models stop working.
    pub async fn listen(
        &mut self,
        frames: &mut impl FrameSource,
    ) -> Result<WakeTrigger, ListenError> {
        self.set_state(EngineState::Listening);
        let reset = std::time::Instant::now();
        self.wake.reset();
        self.recorder
            .metric(Event::WakeReset, "ok", Some(reset.elapsed()), Value::Null);
        loop {
            let frame = frames.next().await?;
            self.note_level(&frame);
            let scored = self.wake.process(&frame);
            let (outcome, took) = match &scored {
                Ok(WakeFrame {
                    event, inference, ..
                }) => (
                    match event {
                        None => "none",
                        Some(WakeEvent::Capture) => "capture",
                        Some(WakeEvent::Conversation) => "conversation",
                        Some(WakeEvent::Ambiguous) => "ambiguous",
                    },
                    Some(*inference),
                ),
                Err(_) => ("error", None),
            };
            self.recorder
                .metric(Event::WakeInference, outcome, took, Value::Null);
            let event = scored?.event;
            if event == Some(WakeEvent::Ambiguous) {
                if !self.ambiguous {
                    (self.notice)("Wake was ambiguous; still listening.");
                }
                self.ambiguous = true;
                continue;
            }
            self.ambiguous = false;
            match event {
                Some(WakeEvent::Capture) => {
                    self.gate.set_enabled(false);
                    return Ok(WakeTrigger::Capture);
                }
                Some(WakeEvent::Conversation) => return Ok(WakeTrigger::Conversation),
                Some(WakeEvent::Ambiguous) | None => {}
            }
        }
    }

    fn note_level(&mut self, frame: &Frame) {
        if !self.recorder.kinds().metrics {
            return;
        }
        self.level_frames += 1;
        self.level_activity |= frame.iter().any(|s| s.unsigned_abs() >= ACTIVITY_PEAK);
        if self.level_frames == 1 || self.level_frames.is_multiple_of(LEVEL_WINDOW) {
            let outcome = if self.level_activity {
                "activity"
            } else {
                "quiet"
            };
            self.level_activity = false;
            self.recorder
                .metric(Event::WakeLevel, outcome, None, Value::Null);
        }
    }

    /// Plays the wake cue while the recognizer connects, then takes the user's dictation until
    /// they end it, cancel it, or something fails.
    ///
    /// Call after [`listen`](Self::listen) returned [`WakeTrigger::Capture`]. Cancellation through
    /// [`EngineHandle::cancel`] applies to this call only. Returns to listening in every case.
    pub async fn capture(&mut self, frames: &mut impl FrameSource) -> CaptureOutcome {
        self.cancel.borrow_and_update();
        self.set_state(EngineState::StartingCapture);
        let started = Instant::now();
        let outcome = match self.start(frames).await {
            Ok((mut live, backlog)) => {
                let done = self.drive(frames, &mut live, backlog).await;
                self.conclude(done, Some(live)).await
            }
            Err(done) => self.conclude(done, None).await,
        };
        self.gate.set_enabled(true);
        self.set_state(EngineState::Listening);
        let flow = match &outcome {
            CaptureOutcome::Accepted(_) => "ok",
            CaptureOutcome::Cancelled => "cancelled",
            CaptureOutcome::Failed { .. } => "failed",
        };
        self.recorder.metric(
            Event::CaptureFlow,
            flow,
            Some(started.elapsed()),
            Value::Null,
        );
        outcome
    }

    fn set_state(&self, next: EngineState) {
        self.state.send_if_modified(|current| {
            if *current == next {
                return false;
            }
            *current = next;
            true
        });
    }

    fn publish(&self, live: &Live<R::Session>) {
        self.set_state(match live.text.pending() {
            Some(pending) => EngineState::PendingEnding {
                route: pending.route,
                text: pending.text.clone(),
                waiting_for_quiet: pending.window == QuietWindow::WaitingForQuiet,
            },
            None => EngineState::Capturing {
                committed: live.text.text(),
                partial: live.partial.clone(),
            },
        });
    }

    /// A future that plays `cue` unless it is switched off.
    fn cue(&self, cue: Cue) -> LocalBoxFuture<'static, CueEnd> {
        let player = Rc::clone(&self.cues);
        let on = self.cues_on.enabled(cue);
        async move {
            let started = Instant::now();
            let result = if on { player.play(cue).await } else { Ok(()) };
            CueEnd {
                cue,
                played: on,
                took: started.elapsed(),
                result,
            }
        }
        .boxed_local()
    }

    fn cue_metric(&self, event: Event, end: &CueEnd) {
        if !end.played {
            return;
        }
        let outcome = if end.result.is_ok() { "ok" } else { "error" };
        self.recorder.metric(
            event,
            outcome,
            Some(end.took),
            json!({"cue": cue_name(end.cue)}),
        );
    }

    /// Reports a cue that failed to play; the capture goes on without it.
    fn cue_failed(&self, end: &CueEnd, then: &str) {
        if let Err(error) = &end.result {
            (self.notice)(&format!("{error} {then}"));
        }
    }

    /// Plays the cue that ends a capture; false when the user cancelled while it played.
    async fn end_cue(&mut self, cue: Cue) -> bool {
        let started = Instant::now();
        let played = self.cue(cue);
        tokio::select! {
            biased;
            _ = self.cancel.changed() => {
                self.recorder.metric(
                    Event::EndCue,
                    "cancelled",
                    Some(started.elapsed()),
                    json!({"cue": cue_name(cue)}),
                );
                false
            }
            end = played => {
                self.cue_metric(Event::EndCue, &end);
                self.cue_failed(&end, "The capture continues.");
                true
            }
        }
    }

    /// Plays the wake cue while the recognizer connects, then returns the open recognition and
    /// the frames that arrived after the cue while it was still connecting.
    async fn start(
        &mut self,
        frames: &mut impl FrameSource,
    ) -> Result<(Live<R::Session>, Vec<Frame>), Done> {
        let started = Instant::now();
        let deadline = started + READY_TIMEOUT;
        let recognizer = &self.recognizer;
        let mut connect = pin!(async move {
            let at = Instant::now();
            (recognizer.connect().await, at.elapsed())
        });
        let mut wake_cue = self.cue(Cue::Wake);
        let mut session = None;
        let mut cue_ended = false;
        let mut backlog = Vec::new();
        let mut connection: (&str, Option<Duration>) = ("cancelled", None);
        let stop = loop {
            tokio::select! {
                biased;
                _ = self.cancel.changed() => break Some(Done::Stopped),
                (result, took) = &mut connect, if session.is_none() => match result {
                    Ok(open) => {
                        connection = ("ok", Some(took));
                        session = Some(open);
                    }
                    Err(error) => {
                        connection = ("error", Some(took));
                        break Some(Done::Failed(error.into()));
                    }
                },
                end = &mut wake_cue, if !cue_ended => {
                    cue_ended = true;
                    // The cue engine does not restore a gate that was already off.
                    self.gate.set_enabled(true);
                    self.cue_metric(Event::ReadyCue, &end);
                    self.cue_failed(&end, "The capture continues.");
                }
                () = tokio::time::sleep_until(deadline) => break Some(Done::Failed(CaptureError::NotReady)),
                frame = frames.next(), if cue_ended => match frame {
                    Ok(frame) => backlog.push(frame),
                    Err(error) => break Some(Done::Failed(error.into())),
                },
            }
            if cue_ended && session.is_some() {
                break None;
            }
        };
        self.recorder.metric(
            Event::TranscriptionReady,
            connection.0,
            Some(connection.1.unwrap_or_else(|| started.elapsed())),
            Value::Null,
        );
        if let Some(done) = stop {
            if let Some(session) = session {
                session.cancel().await;
            }
            return Err(done);
        }
        let Some(session) = session else {
            return Err(Done::Failed(CaptureError::NotReady));
        };
        let live = Live {
            session,
            text: CaptureSession::new(self.endings.clone(), self.pause),
            partial: None,
            sent: Vec::new(),
            keep_sent: self.secondary.is_some() || self.recorder.kinds().audio,
            held: VecDeque::new(),
            speech_run: 0,
            relayed: false,
            speech: false,
            last_audio: Instant::now(),
            cues: FuturesUnordered::new(),
        };
        self.publish(&live);
        Ok((live, backlog))
    }

    /// Runs the capture until it ends.
    async fn drive(
        &mut self,
        frames: &mut impl FrameSource,
        live: &mut Live<R::Session>,
        backlog: Vec<Frame>,
    ) -> Done {
        for frame in backlog {
            if let Err(stop) = self.on_frame(live, &frame).await {
                return stop.into();
            }
        }
        loop {
            let watchdog = (live.text.pending().is_some() && live.cues.is_empty())
                .then(|| live.last_audio + MIC_WATCHDOG);
            let step = tokio::select! {
                biased;
                _ = self.cancel.changed() => Step::Cancel,
                event = live.session.next_event() => Step::Event(event),
                frame = frames.next() => Step::Frame(frame),
                Some(end) = live.cues.next() => Step::Cue(end),
                () = until(live.text.ready_at()) => Step::Quiet,
                () = until(watchdog) => Step::Silent,
            };
            let done = match step {
                Step::Cancel => Some(Done::Stopped),
                Step::Event(Err(error)) => Some(Done::Failed(error.into())),
                Step::Event(Ok(RecognizerEvent::Partial(text))) => {
                    self.on_partial(live, text);
                    None
                }
                Step::Event(Ok(RecognizerEvent::Final(text))) => self.on_final(live, &text),
                Step::Frame(Err(error)) => Some(Done::Failed(error.into())),
                Step::Frame(Ok(frame)) => self.on_frame(live, &frame).await.err().map(Done::from),
                Step::Cue(end) => {
                    live.last_audio = Instant::now();
                    self.cue_metric(Event::Cue, &end);
                    self.cue_failed(&end, "The capture continues.");
                    None
                }
                Step::Quiet => self.on_quiet(live),
                Step::Silent => Some(Done::Failed(CaptureError::MicStopped)),
            };
            if let Some(done) = done {
                return done;
            }
        }
    }

    fn queue_cue(&self, live: &Live<R::Session>, cue: Cue) {
        live.cues.push(self.cue(cue));
    }

    fn on_partial(&self, live: &mut Live<R::Session>, text: String) {
        if live.text.on_partial(&text) {
            self.ending_state("partial_cancelled");
            self.queue_cue(live, Cue::Cancelled);
        }
        live.partial = Some(text).filter(|text| !text.trim().is_empty());
        self.publish(live);
    }

    fn on_final(&self, live: &mut Live<R::Session>, text: &str) -> Option<Done> {
        self.recorder.text("final", json!({"text": text}));
        live.partial = None;
        let now = Instant::now();
        match live.text.on_final(text, now, live.speech) {
            Err(full) => return Some(Done::Failed(full.into())),
            Ok(FinalStep::Cancelled) => return Some(Done::NeverMind),
            Ok(FinalStep::Continue { cancelled_pending }) => {
                if cancelled_pending {
                    self.ending_state("final_cancelled");
                    self.queue_cue(live, Cue::Cancelled);
                }
            }
            Ok(FinalStep::Armed(pending)) => {
                if live.relayed {
                    live.relayed = false;
                    live.held.clear();
                    live.speech_run = 0;
                }
                live.last_audio = now;
                self.ending_state(if pending.window == QuietWindow::WaitingForQuiet {
                    "waiting_for_quiet"
                } else {
                    "pending"
                });
                self.queue_cue(live, Cue::Provisional);
            }
        }
        self.publish(live);
        None
    }

    fn on_quiet(&self, live: &mut Live<R::Session>) -> Option<Done> {
        let now = Instant::now();
        let since = live.text.ready_at().map(|at| at - self.pause.get());
        let request = live.text.poll_ready(now)?;
        self.ending_state("accepted");
        if let Some(since) = since {
            self.recorder.metric(
                Event::EndingPause,
                "ok",
                Some(now.duration_since(since)),
                Value::Null,
            );
        }
        Some(Done::Accepted(request))
    }

    fn ending_state(&self, outcome: &str) {
        self.recorder
            .metric(Event::EndingState, outcome, None, Value::Null);
    }

    /// Judges one frame and sends it, or holds it while an ending is pending and the user has
    /// not been heard continuing.
    async fn on_frame(&mut self, live: &mut Live<R::Session>, frame: &Frame) -> Result<(), Stop> {
        let now = Instant::now();
        live.last_audio = now;
        let pending = live.text.pending().is_some();
        live.speech = self.vad.speech(frame, pending);
        if !pending {
            live.speech_run = 0;
            live.relayed = false;
            let mut out: Vec<Frame> = live.held.drain(..).collect();
            out.push(*frame);
            return self.send(live, &out).await;
        }
        if live.held.len() == usize::from(CONTINUATION_FRAMES) {
            live.held.pop_front();
        }
        live.held.push_back(*frame);
        live.speech_run = if live.speech {
            live.speech_run.saturating_add(1)
        } else {
            0
        };
        if !live.speech && live.text.on_quiet_frame(now) {
            self.ending_state("quiet_started");
            self.publish(live);
        }
        if live.relayed {
            live.held.clear();
            return self.send(live, std::slice::from_ref(frame)).await;
        }
        if live.speech_run >= CONTINUATION_FRAMES {
            live.relayed = true;
            self.ending_state("local_speech_relayed");
            let out: Vec<Frame> = live.held.drain(..).collect();
            return self.send(live, &out).await;
        }
        Ok(())
    }

    /// Sends `frames`, giving up when the user cancels while the recognizer is not taking audio.
    async fn send(&mut self, live: &mut Live<R::Session>, frames: &[Frame]) -> Result<(), Stop> {
        for frame in frames {
            tokio::select! {
                biased;
                _ = self.cancel.changed() => return Err(Stop::Cancelled),
                sent = live.session.send_frame(frame) => sent.map_err(|e| Stop::Failed(e.into()))?,
            }
            if live.keep_sent {
                live.sent.push(*frame);
            }
        }
        Ok(())
    }

    /// Plays the cue that ends the capture, releases the recognizer and settles the outcome.
    async fn conclude(&mut self, done: Done, live: Option<Live<R::Session>>) -> CaptureOutcome {
        let (session, sent, scratched, recoverable_text) = match live {
            // Dropping the rest stops any cue still playing.
            Some(live) => (
                Some(live.session),
                live.sent,
                live.text.scratched(),
                recoverable(&live.text),
            ),
            None => (None, Vec::new(), false, None),
        };
        if !sent.is_empty() {
            self.recorder.audio(sent.as_flattened().to_vec());
        }
        let outcome = match done {
            Done::Stopped => {
                if let Some(session) = session {
                    session.cancel().await;
                }
                CaptureOutcome::Cancelled
            }
            Done::NeverMind => {
                self.gate.set_enabled(false);
                self.end_with(Cue::Cancelled, session, false).await;
                CaptureOutcome::Cancelled
            }
            Done::Failed(error) => {
                self.gate.set_enabled(false);
                self.end_with(Cue::Error, session, false).await;
                CaptureOutcome::Failed {
                    error,
                    recoverable_text,
                }
            }
            Done::Accepted(request) => {
                self.gate.set_enabled(false);
                if self.end_with(Cue::Accepted, session, true).await {
                    self.reconcile(request, &sent, scratched).await
                } else {
                    CaptureOutcome::Cancelled
                }
            }
        };
        match &outcome {
            CaptureOutcome::Accepted(request) => self.recorder.text(
                "accepted",
                json!({"route": request.route, "text": request.text}),
            ),
            CaptureOutcome::Cancelled => self.recorder.text("cancelled", Value::Null),
            CaptureOutcome::Failed { error, .. } => self
                .recorder
                .text("failed", json!({"error": error.to_string()})),
        }
        outcome
    }

    /// Plays `cue` while releasing `session`: closed when the capture was accepted, cancelled
    /// otherwise. False when the user cancelled while the cue played.
    async fn end_with(&mut self, cue: Cue, session: Option<R::Session>, close: bool) -> bool {
        let release = async {
            match session {
                Some(session) if close => session.close().await,
                Some(session) => session.cancel().await,
                None => {}
            }
        };
        let (completed, ()) = tokio::join!(self.end_cue(cue), release);
        completed
    }

    /// Replaces the accepted text with the secondary transcription's when it agrees with the
    /// ending the user spoke; otherwise keeps the live transcript and says so.
    async fn reconcile(
        &mut self,
        request: CaptureRequest,
        sent: &[Frame],
        scratched: bool,
    ) -> CaptureOutcome {
        let Some(settings) = self.secondary.as_ref().filter(|_| !scratched) else {
            return CaptureOutcome::Accepted(request);
        };
        self.set_state(EngineState::TranscribingSecondary {
            route: request.route,
        });
        let started = Instant::now();
        let heard = tokio::select! {
            biased;
            _ = self.cancel.changed() => None,
            heard = secondary::transcribe(&self.http, settings, sent) => Some(heard),
        };
        let fields = json!({
            "model": settings.model(),
            "api": match settings.api() {
                SecondaryApi::OpenAi => "openai",
                SecondaryApi::Meta => "meta",
            },
        });
        let (outcome, replaced) = match heard {
            None => ("cancelled", None),
            Some(Ok(raw)) => {
                self.recorder.text("secondary", json!({"text": raw}));
                match self.agreed(&raw, request.route) {
                    Some(text) => ("ok", Some(text)),
                    None => ("error", None),
                }
            }
            Some(Err(SecondaryError::Timeout)) => ("timeout", None),
            Some(Err(_)) => ("error", None),
        };
        self.recorder.metric(
            Event::SecondaryStt,
            outcome,
            Some(started.elapsed()),
            fields,
        );
        match (outcome, replaced) {
            ("cancelled", _) => CaptureOutcome::Cancelled,
            (_, Some(text)) => CaptureOutcome::Accepted(CaptureRequest {
                route: request.route,
                text,
            }),
            _ => {
                (self.notice)("The second transcription failed. Using the live transcript.");
                CaptureOutcome::Accepted(request)
            }
        }
    }

    /// The secondary transcript without its ending phrase, when it ends the way the user did.
    fn agreed(&self, raw: &str, route: CaptureRoute) -> Option<String> {
        let mut parser = CaptureSession::new(self.endings.clone(), self.pause);
        let text = match parser.on_final(raw, Instant::now(), false).ok()? {
            FinalStep::Armed(pending) if pending.route == route => pending.text,
            FinalStep::Continue { .. } if !parser.scratched() => parser.text(),
            _ => return None,
        };
        (!text.is_empty()).then_some(text)
    }
}

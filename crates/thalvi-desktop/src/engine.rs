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
use std::task::Poll;
use std::time::Duration;

use futures_util::future::{FutureExt, LocalBoxFuture};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

use crate::audio::{AudioError, Frames, InputGate};
use crate::capture::{
    CaptureFullError, CaptureLimits, CaptureRequest, CaptureRoute, CaptureSession, EndingPauses,
    Endings, FinalStep, QuietWindow, has_words,
};
use crate::claim::{Effect, Resolution, Watch};
use crate::config::{DesktopConfig, Script};
use crate::cues::{Cue, CueEngine, CueError, CueTiming, Cues};
use crate::entries::SecondPassEntry;
use crate::frame::{FRAME_SAMPLES, Frame, Order, Queued};
use crate::hold::{Cutoff, HoldKind, KeyState, Press, Presses, Refusal};
use crate::metrics::{Dictation, Event, RecordKinds, Recorder};
use crate::recognizer::{
    Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, SpeechSource,
};
use crate::report::UserError;
use crate::secondary::{self, SecondaryApi, SecondaryError};
use crate::spotter::KeywordSpotter;
use crate::vad::VadDetector;
use crate::wake::{WakeEngine, WakeError, WakeEvent, WakeFrame};
use crate::wake_stats::{Latency, LatencySummary, NearMisses};
use crate::writing::{self, Piece, Writing, WritingError};

/// How long the recognizer and the wake cue have to be ready before the capture is abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a pending ending may go without microphone audio before the capture fails.
const MIC_WATCHDOG: Duration = Duration::from_millis(250);

/// How long any capture may go without microphone audio before it fails.
const MIC_TIMEOUT: Duration = Duration::from_secs(3);

/// Consecutive speech frames, 240 ms, that make audio held back during a pending ending count as
/// the user continuing.
const CONTINUATION_FRAMES: u8 = 3;

/// The shortest hold of the dictation key that dictates; a shorter one is a slip of the finger.
pub const MIN_HOLD: Duration = Duration::from_millis(300);

/// Silence sent at release, 800 ms: enough for the recognizer to finish the last word and
/// reach its endpoint, so a final comes without waiting for real audio.
const RELEASE_SILENCE_FRAMES: usize = 10;

/// How long after the release the engine waits for that final before pasting what has arrived.
/// Measured on the server recognizer, the last word was in the text by 200 ms in every run.
const RELEASE_LIMIT: Duration = Duration::from_millis(250);

static RELEASE_SILENCE: [Frame; RELEASE_SILENCE_FRAMES] =
    [[0i16; FRAME_SAMPLES]; RELEASE_SILENCE_FRAMES];

/// Frames in one wake level window, 2 s.
const LEVEL_WINDOW: u64 = 25;

/// How long a wake level window lasts.
const LEVEL_SPAN: Duration = Duration::from_secs(2);

/// Frames of microphone audio kept for the wake clip, 2 s.
const WAKE_CLIP_FRAMES: usize = 25;

/// The sample peak from which a wake level window counts as activity.
const ACTIVITY_PEAK: u16 = 1_000;

/// The head's context, 1.28 s: the last frames whose peak a wake must reach.
const WAKE_FLOOR_FRAMES: usize = 16;

/// The sample peak below which a wake is too quiet to be speech at the microphone. Real wakes
/// peaked at 2,073 or more and faint false wakes at 1,152 or less.
const WAKE_FLOOR_PEAK: u16 = 1_500;

/// Microphone frames in order.
pub trait FrameSource {
    /// Waits for the next frame; safe to drop and call again.
    fn next(&mut self) -> impl Future<Output = Result<Queued, AudioError>>;
}

impl FrameSource for Frames {
    async fn next(&mut self) -> Result<Queued, AudioError> {
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
    /// Resolves when the cue has finished or failed; dropping the future cuts the cue short. The
    /// timing is none when the cue never reached the output.
    fn play(&self, cue: Cue) -> impl Future<Output = Result<Option<CueTiming>, CueError>>;
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
    async fn play(&self, cue: Cue) -> Result<Option<CueTiming>, CueError> {
        let called = std::time::Instant::now();
        let engine = Arc::clone(&self.0);
        let mut stop = StopOnDrop(Some(Arc::clone(&self.0)));
        let played = tokio::task::spawn_blocking(move || engine.play(cue, called)).await;
        stop.0 = None;
        // A panic in the playback thread leaves the output in an unknown state.
        played.unwrap_or(Err(CueError::Stalled))
    }
}

/// A score to three decimals.
fn round3(score: f32) -> f64 {
    (f64::from(score) * 1000.0).round() / 1000.0
}

/// Both heads' scores for one frame, to three decimals.
fn scores(wake: &WakeFrame) -> Value {
    json!({"capture": round3(wake.capture), "conversation": round3(wake.conversation)})
}

/// What the engine does with a wake word, for the caller to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeTrigger {
    /// The caller binds its capture target, then calls [`Engine::capture`].
    Capture,
    /// The caller starts a conversation.
    Conversation,
    /// The dictation key went down: the caller binds its capture target, then calls
    /// [`Engine::capture`], which ends with the key's release.
    Hold,
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
        /// Something the user should know that did not stop the capture; it stays until the next
        /// words are heard, and holds none of what was said.
        notice: Option<String>,
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
    /// The writing model is putting the words into proper text; `text` is what has streamed in.
    Writing {
        text: String,
    },
    /// The writing model is reasoning and has written no text yet.
    Thinking,
}

/// Why a capture failed; `Display` is the plain notice for the user.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error(transparent)]
    Recognizer(#[from] RecognizerError),
    #[error(
        "{} was not ready after {}. Nothing was delivered. {}",
        speech.subject(),
        plain_duration(*waited),
        speech.fix()
    )]
    NotReady {
        waited: Duration,
        speech: SpeechSource,
    },
    #[error(transparent)]
    Audio(#[from] AudioError),
    #[error("Microphone audio stopped. Nothing was delivered.")]
    MicStopped,
    #[error("{0} Nothing was pasted.")]
    Writing(WritingError),
    #[error("The key was released too quickly to dictate. Nothing was pasted.")]
    HoldTooShort,
    #[error("No words were heard while the key was held. Nothing was pasted.")]
    NothingHeard,
    #[error("No words heard for {}.", plain_duration(*.0))]
    NoWords(Duration),
    #[error("Didn't hear an ending for {}.", plain_duration(*.0))]
    NoEnding(Duration),
    #[error("Capture stopped after {} without an ending.", plain_duration(*.0))]
    TooLong(Duration),
    #[error(transparent)]
    Full(#[from] CaptureFullError),
}

impl UserError for CaptureError {
    fn headline(&self) -> &'static str {
        match self {
            Self::Recognizer(error) => error.headline(),
            Self::NotReady { speech, .. } if speech.is_on_device() => {
                "Speech model not ready — Settings"
            }
            Self::NotReady { .. } => "Speech server not ready — Settings",
            Self::Audio(error) => error.headline(),
            Self::Writing(error) => error.headline(),
            Self::MicStopped => "Microphone stopped mid-capture",
            Self::HoldTooShort => "Key released too soon",
            Self::NothingHeard | Self::NoWords(_) => "No words heard",
            Self::NoEnding(_) => "No ending heard",
            Self::TooLong(_) => "Capture too long — not delivered",
            Self::Full(_) => "Capture full — finish or cancel",
        }
    }
}

/// Whole minutes read as minutes, anything else as seconds.
fn plain_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (count, unit) = if seconds > 0 && seconds.is_multiple_of(60) {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

impl CaptureError {
    /// A short name for the metrics, without any text.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Recognizer(_) => "recognizer",
            Self::NotReady { .. } => "not_ready",
            Self::Audio(_) => "audio",
            Self::MicStopped => "mic_stopped",
            Self::Writing(_) => "writing",
            Self::HoldTooShort => "hold_too_short",
            Self::NothingHeard => "hold_empty",
            Self::NoWords(_) | Self::NoEnding(_) => "no_ending",
            Self::TooLong(_) => "too_long",
            Self::Full(_) => "full",
        }
    }
}

/// How a capture ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// The user ended the capture; nothing has been delivered yet.
    Accepted(CaptureRequest),
    /// The user cancelled it, by voice or through [`EngineHandle::cancel`].
    Cancelled {
        /// What the user had dictated before cancelling; nothing was delivered.
        dictated: Option<String>,
    },
    Failed {
        error: CaptureError,
        /// What the user had dictated so far, for the caller to keep.
        recoverable_text: Option<String>,
    },
}

impl CaptureOutcome {
    /// A cancelled capture that had dictated `text`.
    fn cancelled_with(text: String) -> Self {
        Self::Cancelled {
            dictated: (!text.is_empty()).then_some(text),
        }
    }
}

/// Why listening stopped; `Display` is the plain notice for the user.
#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error(transparent)]
    Audio(#[from] AudioError),
    #[error(transparent)]
    Wake(#[from] WakeError),
}

impl UserError for ListenError {
    fn headline(&self) -> &'static str {
        match self {
            Self::Audio(error) => error.headline(),
            Self::Wake(error) => error.headline(),
        }
    }
}

/// The part of the engine other threads may hold: cancel a capture and watch the state.
#[derive(Clone)]
pub struct EngineHandle {
    cancel: Arc<watch::Sender<u64>>,
    state: watch::Receiver<EngineState>,
    level: watch::Receiver<f32>,
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

    /// The loudness of the latest frame while listening or capturing, 0 to 1; 0 otherwise.
    #[must_use]
    pub fn level(&self) -> watch::Receiver<f32> {
        self.level.clone()
    }
}

/// Loudness of `frame`: its RMS from -60 dBFS (0) to -10 dBFS (1).
fn loudness(frame: &Frame) -> f32 {
    let energy: f64 = frame.iter().map(|&s| f64::from(s).powi(2)).sum();
    #[expect(clippy::cast_precision_loss, reason = "a frame length is exact in f64")]
    let samples = FRAME_SAMPLES as f64;
    let rms = (energy / samples).sqrt();
    let db = 20.0 * (rms / 32_768.0).max(1e-9).log10();
    #[expect(clippy::cast_possible_truncation, reason = "a level between 0 and 1")]
    let level = ((db + 60.0) / 50.0).clamp(0.0, 1.0) as f32;
    level
}

/// What the engine runs on.
pub struct Parts<R, W, C> {
    pub recognizer: R,
    pub wake: W,
    pub cues: C,
    /// The gate of the microphone `frames` come from.
    pub gate: InputGate,
    pub http: reqwest::Client,
    /// Whether the dictation key is down.
    pub hold: watch::Receiver<KeyState>,
    /// Tells the user about something that did not stop the capture.
    pub notice: Box<dyn Fn(&str)>,
    /// Listens for the ending phrases beside the recognizer; none when it could not be loaded.
    pub spotter: Option<Box<dyn KeywordSpotter>>,
    /// Why there is no spotter, for the first capture to tell the user once.
    pub spotter_off: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListenState {
    Stopped,
    Active,
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
    pauses: EndingPauses,
    scripts: Vec<Script>,
    limits: CaptureLimits,
    /// The ticked second-pass entries in list order.
    secondary: Vec<SecondPassEntry>,
    /// Second-pass requests the paste did not wait for, one set per capture. Each runs as its own
    /// task, so its latency is its own, and is recorded when a loop of the engine collects it.
    /// Dropping the engine aborts them.
    recorded_only: Vec<JoinSet<Answer>>,
    writing: Option<Writing>,
    speech: SpeechSource,
    http: reqwest::Client,
    recorder: Recorder,
    notice: Box<dyn Fn(&str)>,
    spotter: Option<Box<dyn KeywordSpotter>>,
    spotter_off: Option<String>,
    state: watch::Sender<EngineState>,
    level: watch::Sender<f32>,
    /// Keeps the cancel channel open so waiting on it never ends.
    _cancel_tx: Arc<watch::Sender<u64>>,
    cancel: watch::Receiver<u64>,
    level_frames: u64,
    level_activity: bool,
    ambiguous: bool,
    /// Whether the previous frame's wake was refused as too quiet.
    refused: bool,
    /// The capture and conversation thresholds the wake detector runs with.
    thresholds: (f32, f32),
    /// The wake word that opened the next capture.
    woke: Option<Woke>,
    /// The latest frames scored while listening.
    wake_clip: VecDeque<Frame>,
    key: Presses,
    listening: ListenState,
    /// The hold that opened the next capture.
    held: Option<Held>,
    /// How the latest hold ended, when the key decided it.
    hold_end: Option<&'static str>,
    /// How long the latest hold's key was down, once it came up.
    held_for: Option<Duration>,
    /// The presses of the dictation key that started nothing, until the caller takes them.
    ignored: Vec<Refusal>,
}

/// A press of the dictation key that opened a capture.
struct Held {
    press: u64,
    kind: HoldKind,
    at: Instant,
    /// Where this press was released among the queued frames, however many presses followed.
    release: Cutoff,
}

impl Held {
    /// What releasing the key delivers.
    fn route(&self) -> CaptureRoute {
        match self.kind {
            HoldKind::Dictate => CaptureRoute::Paste,
            HoldKind::Write => CaptureRoute::Write,
        }
    }
}

/// A wake word heard: when, and the folder of its recordings.
struct Woke {
    at: Instant,
    dictation: Dictation,
}

/// How a cue ended.
struct CueEnd {
    cue: Cue,
    played: bool,
    took: Duration,
    /// From the event that called for the cue until it started playing.
    waited: Duration,
    result: Result<Option<CueTiming>, CueError>,
}

/// One capture in progress, after the recognizer is ready.
struct Live<S> {
    session: S,
    dictation: Dictation,
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
    started: Instant,
    /// When the recognizer last returned words.
    last_words: Instant,
    heard: Heard,
    cues: FuturesUnordered<LocalBoxFuture<'static, CueEnd>>,
    release: Release,
    /// The release of the hold that opened this capture; frames queued after it are not speech
    /// of the hold.
    cutoff: Option<Cutoff>,
    /// What the ending spotter's hits mean; none when it does not listen to this capture.
    watch: Option<Watch>,
    spot: SpotStats,
    /// Shown with the capture until the next words are heard.
    notice: Option<String>,
}

/// What the ending spotter did during one capture.
#[derive(Default)]
struct SpotStats {
    frames: u64,
    hits: u32,
    total: Duration,
    slowest: Duration,
}

impl SpotStats {
    fn record(&self, recorder: &Recorder) {
        if self.frames > 0 {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a frame count is far below 2^52"
            )]
            let mean = millis(self.total) / self.frames as f64;
            let tenth = |ms: f64| (ms * 1000.0).round() / 1000.0;
            recorder.metric(
                Event::EndingSpotter,
                "ok",
                Some(self.total),
                json!({
                    "call": "capture",
                    "frames": self.frames,
                    "hits": self.hits,
                    "step_mean_ms": tenth(mean),
                    "step_max_ms": tenth(millis(self.slowest)),
                }),
            );
        }
    }
}

/// When speech was heard, to time the recognizer's answers to it.
#[derive(Default)]
struct Heard {
    /// The first speech heard, until the recognizer's first words come.
    first: Option<Instant>,
    /// Whether the recognizer has returned words in this capture.
    words_came: bool,
    /// The latest speech heard since the recognizer's last final result.
    last: Option<Instant>,
}

/// What the dictation key's release does to a capture.
#[derive(Clone, Copy)]
enum Release {
    /// The key is down and its release ends the capture; or no key opened this capture.
    Down,
    /// The key came up at this instant; the last words may still arrive.
    Tail(Instant),
    /// Nothing more comes of the key: the capture is not a hold.
    Over,
}

impl<S> Live<S> {
    /// Whether a frame queued at `order` came after the key went up and is not the hold's to
    /// send.
    fn after_release(&self, order: Order) -> bool {
        !matches!(self.release, Release::Over)
            && self
                .cutoff
                .as_ref()
                .is_some_and(|cutoff| cutoff.precedes(order))
    }

    /// When a released hold stops waiting for words.
    fn tail(&self) -> Option<Instant> {
        match self.release {
            Release::Tail(at) => Some(at + RELEASE_LIMIT),
            _ => None,
        }
    }
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
    NeverMind(Option<String>),
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
    Frame(Result<Queued, AudioError>),
    Cue(CueEnd),
    Quiet,
    /// The quiet interval of a disputed ending has passed.
    Disputed,
    Silent,
    Idle,
    TooLong,
    Tail,
    /// The dictation key changed; false when it can no longer change.
    Key(bool),
    /// A second pass that was not waited for finished.
    Recorded(Answer),
}

/// One entry's reply to a capture, with what is needed to judge and record it when it arrives.
struct Answer {
    /// The entry's place among the ticked ones.
    index: usize,
    entry: SecondPassEntry,
    dictation: Dictation,
    route: CaptureRoute,
    has_endings: bool,
    heard: Result<String, SecondaryError>,
    took: Duration,
}

/// An [`Answer`] with its outcome, and the text it gives when it agrees with the user's ending.
struct Judged {
    answer: Answer,
    outcome: &'static str,
    agreed: Option<String>,
}

/// The answer of a finished request; a request only ends without one if it panicked, and that
/// panic is the engine's too.
fn settled(joined: Result<Answer, JoinError>) -> Option<Answer> {
    match joined {
        Ok(answer) => Some(answer),
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => None,
        },
    }
}

/// The next answer of a second pass the paste did not wait for; never ready while none is running.
async fn next_recorded(sets: &mut Vec<JoinSet<Answer>>) -> Answer {
    std::future::poll_fn(|cx| {
        let mut at = 0;
        while let Some(set) = sets.get_mut(at) {
            match set.poll_join_next(cx) {
                Poll::Ready(None) => {
                    sets.swap_remove(at);
                }
                Poll::Ready(Some(joined)) => {
                    if let Some(answer) = settled(joined) {
                        return Poll::Ready(answer);
                    }
                }
                Poll::Pending => at += 1,
            }
        }
        Poll::Pending
    })
    .await
}

/// Which entry the paste takes, as far as the answers so far say.
enum Usable {
    /// An earlier entry may still give a usable answer.
    Waiting,
    Entry(usize),
    Nobody,
}

/// The earliest entry that gave a usable answer, once every entry before it has answered.
fn earliest_usable(slots: &[Option<Judged>]) -> Usable {
    for (index, slot) in slots.iter().enumerate() {
        match slot {
            None => return Usable::Waiting,
            Some(judged) if judged.agreed.is_some() => return Usable::Entry(index),
            Some(_) => {}
        }
    }
    Usable::Nobody
}

/// Whether any of the frames heard after the start cue holds speech.
fn speaks(frames: &[Queued]) -> bool {
    let mut vad = VadDetector::new();
    frames.iter().any(|queued| vad.speech(&queued.frame, false))
}

/// Takes frames published before release, including one that arrived after the last pending poll.
fn take_queued_before(
    frames: &mut impl FrameSource,
    cutoff: &Cutoff,
    backlog: &mut Vec<Queued>,
) -> Result<(), AudioError> {
    while let Some(next) = frames.next().now_or_never() {
        let queued = next?;
        if cutoff.precedes(queued.order) {
            break;
        }
        backlog.push(queued);
    }
    Ok(())
}

/// Why a hold ended before the recognizer could take any speech, and how the metrics name it.
fn released_early(pressed: Instant) -> (CaptureError, &'static str) {
    if pressed.elapsed() < MIN_HOLD {
        (CaptureError::HoldTooShort, "too_short")
    } else {
        (CaptureError::NothingHeard, "empty")
    }
}

/// How a step bounded by cancellation and a deadline ended.
enum Bounded<T> {
    Cancelled,
    TimedOut,
    Done(T),
}

/// When a writing call's first reasoning and first text arrived, counted from its start.
#[derive(Clone, Copy, Default)]
struct FirstPieces {
    reasoning: Option<Duration>,
    text: Option<Duration>,
}

impl FirstPieces {
    /// A timeout while the model had only reasoned is the model still thinking.
    fn explain(self, error: WritingError) -> WritingError {
        match (error, self.reasoning, self.text) {
            (WritingError::Timeout, Some(_), None) => WritingError::StillThinking,
            _ => error,
        }
    }
}

impl<T> Bounded<Result<T, WritingError>> {
    /// The step's value, or why it gave none: its error, a timeout, or `None` for a cancel.
    fn finish(self) -> Result<T, Option<WritingError>> {
        match self {
            Self::Cancelled => Err(None),
            Self::TimedOut => Err(Some(WritingError::Timeout)),
            Self::Done(done) => done.map_err(Some),
        }
    }
}

/// Polls `work` unless the user cancels or `deadline` passes first; cancellation wins a tie.
async fn bounded<T>(
    cancel: &mut watch::Receiver<u64>,
    deadline: Instant,
    work: impl Future<Output = T>,
) -> Bounded<T> {
    tokio::select! {
        biased;
        _ = cancel.changed() => Bounded::Cancelled,
        () = tokio::time::sleep_until(deadline) => Bounded::TimedOut,
        done = work => Bounded::Done(done),
    }
}

async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => pending().await,
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
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

/// Why a capture ended with nothing to deliver; `None` when something was.
fn nothing_delivered(outcome: &CaptureOutcome) -> Option<&'static str> {
    match outcome {
        CaptureOutcome::Accepted(request) => {
            (request.text.is_empty() && request.route != CaptureRoute::Submit).then_some("empty")
        }
        CaptureOutcome::Cancelled { .. } => Some("cancelled"),
        CaptureOutcome::Failed { error, .. } => Some(error.kind()),
    }
}

/// The capture heard no new words for `idle`: words were heard but never their ending, or none at all.
fn idle_error(idle: Duration, text: &CaptureSession) -> CaptureError {
    if recoverable(text).is_some() {
        CaptureError::NoEnding(idle)
    } else {
        CaptureError::NoWords(idle)
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
        let (level, level_rx) = watch::channel(0.0);
        let (cancel_tx, cancel) = watch::channel(0);
        let cancel_tx = Arc::new(cancel_tx);
        let handle = EngineHandle {
            cancel: Arc::clone(&cancel_tx),
            state: state_rx,
            level: level_rx,
        };
        let engine = Self {
            recognizer: parts.recognizer,
            wake: parts.wake,
            cues: Rc::new(parts.cues),
            cues_on: config.cues,
            gate: parts.gate,
            vad: VadDetector::new(),
            endings: config.endings.clone(),
            pauses: config.ending_pause,
            scripts: config.scripts.clone(),
            limits: config.capture_limits,
            secondary: config.stt.entries.ticked().cloned().collect(),
            recorded_only: Vec::new(),
            writing: config.writing.clone(),
            speech: config.speech_source(),
            http: parts.http,
            recorder,
            notice: parts.notice,
            spotter: parts.spotter,
            spotter_off: parts.spotter_off,
            state,
            level,
            _cancel_tx: cancel_tx,
            cancel,
            level_frames: 0,
            level_activity: false,
            ambiguous: false,
            refused: false,
            thresholds: (
                config.wake.capture().threshold.get(),
                config.wake.conversation().threshold.get(),
            ),
            woke: None,
            wake_clip: VecDeque::new(),
            key: Presses::new(parts.hold),
            listening: ListenState::Stopped,
            held: None,
            hold_end: None,
            held_for: None,
            ignored: Vec::new(),
        };
        (engine, handle)
    }

    /// The recorder the engine writes to, for the caller's own events.
    #[must_use]
    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// Changes what is recorded from the next [`listen`](Self::listen) on. Taking `&mut self` keeps
    /// it from running during a capture, which finishes under the kinds it started with.
    pub fn set_recordings(&mut self, kinds: RecordKinds) {
        self.recorder.set_kinds(kinds);
    }

    /// The presses of the dictation key that started nothing since this was last called: each
    /// was already told through the notice and the metrics.
    pub fn take_ignored(&mut self) -> Vec<Refusal> {
        std::mem::take(&mut self.ignored)
    }

    /// Writes the recordings still queued and abandons second passes still running; call once at
    /// shutdown.
    pub async fn finish(self) {
        drop(self.recorded_only);
        self.recorder.finish().await;
    }

    /// Ends listening and hands its unhandled presses to the caller. The next listener ignores
    /// an already-held key.
    pub fn stop_listening(&mut self) -> Presses {
        let mut presses = self.key.clone();
        if self.listening == ListenState::Stopped {
            presses.skip();
        }
        self.listening = ListenState::Stopped;
        presses
    }

    /// Resumes after the caller has accounted for presses while the microphone was closed.
    pub fn resume_listening(&mut self, presses: Presses) {
        self.key = presses;
        self.listening = ListenState::Active;
    }

    /// Scores microphone frames until a wake word is heard or the dictation key goes down. A key
    /// already down when listening starts counts for nothing until it is released and pressed again.
    /// Dropping this future preserves pending presses until listening resumes or `stop_listening`.
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
        if self.listening == ListenState::Stopped {
            self.key.skip();
        }
        self.listening = ListenState::Active;
        // A capture the caller abandoned before it began leaves its start behind; the next one is
        // not that hold or that wake.
        self.held = None;
        self.woke = None;
        let reset = std::time::Instant::now();
        self.wake.reset();
        self.wake_clip.clear();
        self.refused = false;
        self.recorder
            .metric(Event::WakeReset, "ok", Some(reset.elapsed()), Value::Null);
        let mut misses = NearMisses::new(self.thresholds.0, self.thresholds.1);
        let mut latency = Latency::default();
        let heard = self.scan(frames, &mut misses, &mut latency).await;
        self.listening = ListenState::Stopped;
        if let Some(summary) = latency.finish() {
            self.latency_line(summary);
        }
        heard
    }

    async fn scan(
        &mut self,
        frames: &mut impl FrameSource,
        misses: &mut NearMisses,
        latency: &mut Latency,
    ) -> Result<WakeTrigger, ListenError> {
        let metrics = self.recorder.kinds().metrics;
        let mut key_open = true;
        loop {
            let frame = tokio::select! {
                biased;
                changed = self.key.changed(), if key_open => {
                    key_open = changed.is_ok();
                    let press = self.key.take();
                    self.ignore_lost();
                    if let Some(press) = press {
                        self.begin_hold(press);
                        return Ok(WakeTrigger::Hold);
                    }
                    continue;
                }
                answer = next_recorded(&mut self.recorded_only) => {
                    self.record_late(answer);
                    continue;
                }
                frame = frames.next() => frame,
            };
            let frame = match frame {
                Ok(queued) => queued.frame,
                Err(error) => {
                    self.level.send_replace(0.0);
                    return Err(error.into());
                }
            };
            self.level.send_replace(loudness(&frame));
            self.note_level(&frame, latency);
            self.remember(&frame);
            let wake = match self.wake.process(&frame) {
                Ok(wake) => wake,
                Err(error) => {
                    self.recorder
                        .metric(Event::WakeInference, "error", None, Value::Null);
                    self.level.send_replace(0.0);
                    return Err(error.into());
                }
            };
            if metrics {
                for miss in misses.observe(&wake) {
                    self.recorder.metric(
                        Event::WakeNearMiss,
                        miss.head,
                        None,
                        json!({"peak": round3(miss.peak), "threshold": round3(miss.threshold)}),
                    );
                }
                if let Some(summary) = latency.record(Instant::now(), wake.inference) {
                    self.latency_line(summary);
                }
            }
            if self.refuse_quiet(&wake) {
                continue;
            }
            if wake.event == Some(WakeEvent::Ambiguous) {
                if !self.ambiguous {
                    (self.notice)("Wake was ambiguous; still listening.");
                    let dictation = self.recorder.begin();
                    self.clip_wake(&dictation);
                    let mut fields = scores(&wake);
                    fields["dictation"] = json!(dictation.to_string());
                    self.recorder
                        .metric(Event::WakeNothing, "ambiguous", None, fields);
                }
                self.ambiguous = true;
                continue;
            }
            self.ambiguous = false;
            match wake.event {
                Some(WakeEvent::Capture) => {
                    let dictation = self.recorder.begin();
                    self.clip_wake(&dictation);
                    self.woke = Some(Woke {
                        at: Instant::now(),
                        dictation,
                    });
                    self.gate.set_enabled(false);
                    self.level.send_replace(0.0);
                    return Ok(WakeTrigger::Capture);
                }
                Some(WakeEvent::Conversation) => {
                    let dictation = self.recorder.begin();
                    self.clip_wake(&dictation);
                    self.level.send_replace(0.0);
                    return Ok(WakeTrigger::Conversation);
                }
                Some(WakeEvent::Ambiguous) | None => {}
            }
        }
    }

    /// Starts the capture the way a wake word does: the gate shuts so nothing said before the
    /// cue is sent.
    fn begin_hold(&mut self, press: Press) {
        let at = Instant::now();
        self.held = Some(Held {
            press: press.number,
            kind: press.kind,
            at,
            release: press.release,
        });
        self.woke = Some(Woke {
            at,
            dictation: self.recorder.begin(),
        });
        self.gate.set_enabled(false);
        self.level.send_replace(0.0);
    }

    fn latency_line(&self, summary: LatencySummary) {
        self.recorder.metric(
            Event::WakeInference,
            "summary",
            None,
            json!({
                "count": summary.count,
                "p50_ms": millis(summary.p50),
                "p95_ms": millis(summary.p95),
                "max_ms": millis(summary.max),
                "sound_ms": millis(summary.sound),
                "quiet_ms": millis(summary.quiet),
                "switches": summary.switches,
            }),
        );
    }

    /// Keeps `frame` for the wake level check and, when audio is recorded, the wake clip.
    fn remember(&mut self, frame: &Frame) {
        if self.wake_clip.len() == WAKE_CLIP_FRAMES {
            self.wake_clip.pop_front();
        }
        self.wake_clip.push_back(*frame);
    }

    /// Whether `wake` is too quiet to be speech and is dropped, recording the first frame of each
    /// run of refused wakes.
    fn refuse_quiet(&mut self, wake: &WakeFrame) -> bool {
        let level = self.wake_peak();
        let Some(event) = wake.event.filter(|_| level < WAKE_FLOOR_PEAK) else {
            self.refused = false;
            return false;
        };
        if !self.refused {
            self.refused = true;
            let head = match event {
                WakeEvent::Capture => "capture",
                WakeEvent::Conversation => "conversation",
                WakeEvent::Ambiguous => "ambiguous",
            };
            let mut fields = scores(wake);
            fields["level"] = json!(level);
            fields["floor"] = json!(WAKE_FLOOR_PEAK);
            self.recorder.metric(Event::WakeRefused, head, None, fields);
        }
        true
    }

    /// The largest sample magnitude over the last [`WAKE_FLOOR_FRAMES`] frames.
    fn wake_peak(&self) -> u16 {
        let recent = self.wake_clip.iter().rev().take(WAKE_FLOOR_FRAMES);
        recent
            .flat_map(|frame| frame.iter().map(|sample| sample.unsigned_abs()))
            .max()
            .unwrap_or(0)
    }

    /// Records the frames up to and including the wake frame, oldest first.
    fn clip_wake(&self, dictation: &Dictation) {
        if !self.recorder.kinds().audio {
            return;
        }
        let mut pcm = Vec::with_capacity(self.wake_clip.len() * FRAME_SAMPLES);
        for frame in &self.wake_clip {
            pcm.extend_from_slice(frame);
        }
        self.recorder.wake_audio(dictation, pcm);
    }

    fn note_level(&mut self, frame: &Frame, latency: &mut Latency) {
        if !self.recorder.kinds().metrics {
            return;
        }
        self.level_frames += 1;
        self.level_activity |= frame.iter().any(|s| s.unsigned_abs() >= ACTIVITY_PEAK);
        if self.level_frames == LEVEL_WINDOW {
            latency.level(self.level_activity, LEVEL_SPAN);
            self.level_frames = 0;
            self.level_activity = false;
        }
    }

    fn not_ready(&self) -> CaptureError {
        CaptureError::NotReady {
            waited: READY_TIMEOUT,
            speech: self.speech.clone(),
        }
    }

    /// Records how the recognizer's connection ended and how long the wait for it took.
    fn ready_metric(&self, connection: (&str, Option<Duration>), started: Instant) {
        self.recorder.metric(
            Event::TranscriptionReady,
            connection.0,
            Some(connection.1.unwrap_or_else(|| started.elapsed())),
            json!({"entry": self.speech.name()}),
        );
    }

    /// Starts the ending spotter on a fresh capture, unless the key holds it: a hold has no
    /// spoken endings to double-check.
    fn watch_for(
        spotter: &mut Option<Box<dyn KeywordSpotter>>,
        pauses: EndingPauses,
        held: bool,
    ) -> Option<Watch> {
        let spotter = spotter.as_mut().filter(|_| !held)?;
        spotter.reset();
        Some(Watch::new(pauses))
    }

    /// Plays the wake cue while the recognizer connects, then takes the user's dictation until
    /// they end it, cancel it, or something fails. After [`WakeTrigger::Hold`] releasing the key
    /// ends it, pasting what was said at once; no spoken phrase is an ending during a hold, so
    /// only [`EngineHandle::cancel`] cancels one.
    ///
    /// Call after [`listen`](Self::listen) returned [`WakeTrigger::Capture`] or
    /// [`WakeTrigger::Hold`]. Cancellation through [`EngineHandle::cancel`] applies to this call
    /// only. Returns to listening in every case.
    pub async fn capture(&mut self, frames: &mut impl FrameSource) -> CaptureOutcome {
        self.cancel.borrow_and_update();
        self.set_state(EngineState::StartingCapture);
        let started = Instant::now();
        let held = self.held.take();
        self.hold_end = None;
        self.held_for = None;
        let woke = self.woke.take().unwrap_or_else(|| Woke {
            at: started,
            dictation: self.recorder.begin(),
        });
        if held.is_some() {
            self.recorder.metric(
                Event::HoldStart,
                "ok",
                None,
                json!({
                    "dictation": woke.dictation.to_string(),
                    "key": match held.as_ref().map(|held| held.kind) {
                        Some(HoldKind::Write) => "write",
                        _ => "hold",
                    },
                }),
            );
        }
        let outcome = match self.start(frames, &woke, held.as_ref()).await {
            Ok((mut live, backlog, released)) => {
                let (done, decided) = self
                    .drive(frames, &mut live, backlog, held.as_ref(), released)
                    .await;
                self.conclude(done, decided, Some(live), &woke.dictation)
                    .await
            }
            Err((done, decided)) => self.conclude(done, decided, None, &woke.dictation).await,
        };
        let pressed = u64::from(self.key.take().is_some()) + self.key.take_lost();
        if pressed > 0 {
            self.ignore(Refusal::Capturing, pressed);
        }
        self.gate.set_enabled(true);
        self.level.send_replace(0.0);
        self.set_state(EngineState::Listening);
        let (flow, fields) = match &outcome {
            CaptureOutcome::Accepted(_) => ("ok", Value::Null),
            CaptureOutcome::Cancelled { .. } => ("cancelled", Value::Null),
            CaptureOutcome::Failed { error, .. } => ("failed", json!({"reason": error.kind()})),
        };
        self.recorder
            .metric(Event::CaptureFlow, flow, Some(started.elapsed()), fields);
        if let Some(held) = held {
            self.recorder.metric(
                Event::HoldEnd,
                self.hold_end.take().unwrap_or("other"),
                Some(self.held_for.take().unwrap_or_else(|| held.at.elapsed())),
                json!({"dictation": woke.dictation.to_string()}),
            );
        }
        if let Some(reason) = nothing_delivered(&outcome) {
            self.recorder.metric(
                Event::WakeNothing,
                reason,
                None,
                json!({"dictation": woke.dictation.to_string()}),
            );
        }
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
                notice: live.notice.clone(),
            },
        });
    }

    /// A future that plays `cue` unless it is switched off; `trigger` is when the event that
    /// called for the cue happened.
    fn cue(&self, cue: Cue, trigger: Instant) -> LocalBoxFuture<'static, CueEnd> {
        let player = Rc::clone(&self.cues);
        let on = self.cues_on.enabled(cue);
        async move {
            let started = Instant::now();
            let result = if on { player.play(cue).await } else { Ok(None) };
            CueEnd {
                cue,
                played: on,
                took: started.elapsed(),
                waited: started.saturating_duration_since(trigger),
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
        let mut fields = json!({"cue": cue_name(end.cue)});
        // `duration_ms` is play to drained; these tell our code, the device and the user's wait apart.
        if let Ok(Some(timing)) = &end.result {
            fields["start_ms"] = json!(millis(timing.start));
            fields["output_ms"] = json!(timing.output.map(millis));
            fields["audible_ms"] = json!(timing.audible().map(millis));
            fields["perceived_ms"] =
                json!(timing.audible().map(|audible| millis(end.waited + audible)));
        }
        self.recorder.metric(event, outcome, Some(end.took), fields);
    }

    /// Reports a cue that failed to play; the capture goes on without it.
    fn cue_failed(&self, end: &CueEnd, then: &str) {
        if let Err(error) = &end.result {
            (self.notice)(&format!("{error} {then}"));
        }
    }

    /// Plays the cue that ends a capture; false when the user cancelled while it played.
    async fn end_cue(&mut self, cue: Cue, decided: Instant) -> bool {
        let started = Instant::now();
        let played = self.cue(cue, decided);
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

    /// The transcript a capture accumulates: during a hold every word is text, while a wake
    /// word's capture honours the spoken endings.
    fn capture_session(&self, held: bool) -> CaptureSession {
        if held {
            CaptureSession::without_endings()
        } else {
            CaptureSession::new(self.endings.clone(), self.pauses, self.scripts.clone())
        }
    }

    /// Plays the wake cue while the recognizer connects, then returns the open recognition, the
    /// frames that arrived after the cue while it was still connecting, and when the hold's key
    /// came up in the meantime, if speech was already waiting.
    async fn start(
        &mut self,
        frames: &mut impl FrameSource,
        woke: &Woke,
        held: Option<&Held>,
    ) -> Result<(Live<R::Session>, Vec<Queued>, Option<Instant>), (Done, Instant)> {
        let started = Instant::now();
        let deadline = started + READY_TIMEOUT;
        let recognizer = &self.recognizer;
        let mut connect = pin!(async move {
            let at = Instant::now();
            (recognizer.connect().await, at.elapsed())
        });
        let mut wake_cue = self.cue(Cue::Wake, woke.at);
        let mut session = None;
        let mut cue_ended = false;
        let mut backlog = Vec::new();
        let mut released = None;
        let mut connection: (&str, Option<Duration>) = ("cancelled", None);
        let stop = loop {
            tokio::select! {
                biased;
                _ = self.cancel.changed() => break Some(Done::Stopped),
                end = &mut wake_cue, if !cue_ended => {
                    cue_ended = true;
                    // The cue engine does not restore a gate that was already off.
                    self.gate.set_enabled(true);
                    self.cue_metric(Event::ReadyCue, &end);
                    self.cue_failed(&end, "The capture continues.");
                }
                () = tokio::time::sleep_until(deadline) => break Some(Done::Failed(self.not_ready())),
                frame = frames.next(), if cue_ended && released.is_none() => match frame {
                    Ok(queued) if !held.is_some_and(|held| held.release.precedes(queued.order)) => backlog.push(queued),
                    Ok(_) => {}
                    Err(error) => break Some(Done::Failed(error.into())),
                },
                () = self.key.over(held.map_or(0, |held| held.press)), if held.is_some() && released.is_none() => {
                    if let Some(held) = held
                        && let Err(error) = take_queued_before(frames, &held.release, &mut backlog)
                    {
                        break Some(Done::Failed(error.into()));
                    }
                    if woke.at.elapsed() >= MIN_HOLD && speaks(&backlog) {
                        released = Some(Instant::now());
                    } else {
                        let (error, how) = released_early(woke.at);
                        self.hold_end = Some(how);
                        self.held_for = Some(woke.at.elapsed());
                        break Some(Done::Failed(error));
                    }
                }
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
            }
            // A release not yet dealt with comes first, whichever branch the select saw first.
            let release_pending =
                released.is_none() && held.is_some_and(|held| self.key.is_over(held.press));
            if cue_ended && session.is_some() && !release_pending {
                break None;
            }
        };
        let decided = Instant::now();
        self.ready_metric(connection, started);
        if let Some(done) = stop {
            if let Some(session) = session {
                session.cancel().await;
            }
            return Err((done, decided));
        }
        let Some(session) = session else {
            return Err((Done::Failed(self.not_ready()), decided));
        };
        let live = Live {
            session,
            dictation: woke.dictation.clone(),
            text: self.capture_session(held.is_some()),
            partial: None,
            sent: Vec::new(),
            keep_sent: !self.secondary.is_empty() || self.recorder.kinds().audio,
            held: VecDeque::new(),
            speech_run: 0,
            relayed: false,
            speech: false,
            last_audio: Instant::now(),
            started: Instant::now(),
            last_words: Instant::now(),
            heard: Heard::default(),
            cues: FuturesUnordered::new(),
            release: held.map_or(Release::Over, |_| Release::Down),
            cutoff: held.map(|held| held.release.clone()),
            watch: Self::watch_for(&mut self.spotter, self.pauses, held.is_some()),
            spot: SpotStats::default(),
            notice: held.is_none().then(|| self.spotter_off.take()).flatten(),
        };
        self.publish(&live);
        Ok((live, backlog, released))
    }

    /// Runs the capture until it ends.
    async fn drive(
        &mut self,
        frames: &mut impl FrameSource,
        live: &mut Live<R::Session>,
        backlog: Vec<Queued>,
        held: Option<&Held>,
        released: Option<Instant>,
    ) -> (Done, Instant) {
        for queued in backlog {
            if let Err(stop) = self.on_frame(live, &queued).await {
                return (stop.into(), Instant::now());
            }
        }
        if let (Some(at), Some(held)) = (released, held) {
            self.held_for = Some(at.duration_since(held.at));
            if let Err(stop) = self.feed(live, &RELEASE_SILENCE).await {
                return (stop.into(), Instant::now());
            }
            live.release = Release::Tail(Instant::now());
        }
        let mut key_open = true;
        loop {
            let limit = if live.text.pending().is_some() {
                MIC_WATCHDOG
            } else {
                MIC_TIMEOUT
            };
            let watchdog = live.cues.is_empty().then(|| live.last_audio + limit);
            let tail = live.tail();
            let step = tokio::select! {
                biased;
                _ = self.cancel.changed() => Step::Cancel,
                changed = self.key.changed(), if key_open => Step::Key(changed.is_ok()),
                event = live.session.next_event() => Step::Event(event),
                frame = frames.next() => Step::Frame(frame),
                answer = next_recorded(&mut self.recorded_only) => Step::Recorded(answer),
                Some(end) = live.cues.next() => Step::Cue(end),
                () = until(live.text.ready_at()) => Step::Quiet,
                () = until(live.watch.as_ref().and_then(Watch::ready_at)) => Step::Disputed,
                () = until(watchdog) => Step::Silent,
                () = until(tail) => Step::Tail,
                () = tokio::time::sleep_until(live.last_words + self.limits.idle()) => Step::Idle,
                () = tokio::time::sleep_until(live.started + self.limits.max()) => Step::TooLong,
            };
            let done = match step {
                Step::Cancel => Some(Done::Stopped),
                Step::Event(Err(error)) => Some(Done::Failed(error.into())),
                Step::Event(Ok(RecognizerEvent::Partial(text))) => {
                    if has_words(&text) {
                        live.last_words = Instant::now();
                        if !std::mem::replace(&mut live.heard.words_came, true) {
                            self.lag(Event::PartialLag, live.heard.first.take());
                        }
                    }
                    self.on_partial(live, text);
                    None
                }
                Step::Event(Ok(RecognizerEvent::Final(text))) => {
                    if has_words(&text) {
                        live.last_words = Instant::now();
                        self.lag(Event::FinalLag, live.heard.last.take());
                    }
                    let done = self.on_final(live, &text);
                    done.or_else(|| {
                        matches!(live.release, Release::Tail(_))
                            .then(|| self.finish_hold(live, held))
                    })
                }
                Step::Frame(Err(error)) => Some(Done::Failed(error.into())),
                Step::Frame(Ok(queued)) => self.on_frame(live, &queued).await.err().map(Done::from),
                Step::Cue(end) => {
                    live.last_audio = Instant::now();
                    self.cue_metric(Event::Cue, &end);
                    self.cue_failed(&end, "The capture continues.");
                    None
                }
                Step::Recorded(answer) => {
                    self.record_late(answer);
                    None
                }
                Step::Quiet => self.on_quiet(live),
                Step::Disputed => {
                    self.on_disputed(live);
                    None
                }
                Step::Silent => Some(Done::Failed(CaptureError::MicStopped)),
                Step::Idle => Some(Done::Failed(idle_error(self.limits.idle(), &live.text))),
                Step::TooLong => Some(Done::Failed(CaptureError::TooLong(self.limits.max()))),
                Step::Tail => Some(self.finish_hold(live, held)),
                Step::Key(open) => {
                    key_open = open;
                    self.on_key(live, held, frames).await
                }
            };
            if let Some(done) = done {
                return (done, Instant::now());
            }
        }
    }

    /// The dictation key changed during a capture: its release ends a hold, and a press during a
    /// dictation by wake word is refused.
    async fn on_key(
        &mut self,
        live: &mut Live<R::Session>,
        held: Option<&Held>,
        frames: &mut impl FrameSource,
    ) -> Option<Done> {
        let Some(held) = held else {
            let pressed = u64::from(self.key.take().is_some()) + self.key.take_lost();
            if pressed > 0 {
                self.ignore(Refusal::Capturing, pressed);
            }
            return None;
        };
        if self.key.is_over(held.press) && matches!(live.release, Release::Down) {
            return self.on_release(live, held, frames).await;
        }
        None
    }

    /// The key came up: a slip of the finger ends the capture; otherwise pre-release audio is
    /// drained, silence frames are sent at once to trigger the final, and the engine waits for
    /// that final bounded by [`RELEASE_LIMIT`].
    async fn on_release(
        &mut self,
        live: &mut Live<R::Session>,
        held: &Held,
        frames: &mut impl FrameSource,
    ) -> Option<Done> {
        let now = Instant::now();
        self.held_for = Some(now.duration_since(held.at));
        if now.duration_since(held.at) < MIN_HOLD {
            self.hold_end = Some("too_short");
            return Some(Done::Failed(CaptureError::HoldTooShort));
        }
        let mut queued = Vec::new();
        if let Err(error) = take_queued_before(frames, &held.release, &mut queued) {
            return Some(Done::Failed(error.into()));
        }
        for frame in &queued {
            if let Err(stop) = self.on_frame(live, frame).await {
                return Some(Done::from(stop));
            }
        }
        if let Err(stop) = self.feed(live, &RELEASE_SILENCE).await {
            return Some(Done::from(stop));
        }
        live.release = Release::Tail(Instant::now());
        None
    }

    /// Ends a released hold: what was said, with the words still being recognized, is pasted.
    fn finish_hold(&mut self, live: &mut Live<R::Session>, held: Option<&Held>) -> Done {
        if let Some(partial) = live.partial.take().filter(|text| has_words(text))
            && let Some(done) = self.on_final(live, &partial)
        {
            return done;
        }
        let text = live.text.text();
        if !has_words(&text) {
            self.hold_end = Some("empty");
            return Done::Failed(CaptureError::NothingHeard);
        }
        self.hold_end = Some("released");
        Done::Accepted(CaptureRequest {
            route: held.map_or(CaptureRoute::Paste, Held::route),
            text,
        })
    }

    /// Records how long after `since` the recognizer answered, when speech was heard since.
    fn lag(&self, event: Event, since: Option<Instant>) {
        if let Some(since) = since {
            self.recorder
                .metric(event, "ok", Some(since.elapsed()), Value::Null);
        }
    }

    /// Says that `count` presses of the dictation key started nothing, and why.
    fn ignore(&mut self, refusal: Refusal, count: u64) {
        refusal.record(&self.recorder, count);
        (self.notice)(refusal.notice());
        self.ignored.push(refusal);
    }

    /// Says so for presses that were over before the key was looked at.
    fn ignore_lost(&mut self) {
        let lost = self.key.take_lost();
        if lost > 0 {
            self.ignore(Refusal::TooQuick, lost);
        }
    }

    fn queue_cue(&self, live: &Live<R::Session>, cue: Cue) {
        live.cues.push(self.cue(cue, Instant::now()));
    }

    fn on_partial(&self, live: &mut Live<R::Session>, text: String) {
        if has_words(&text) {
            live.notice = None;
        }
        if has_words(&text)
            && let Some(done) = live
                .watch
                .as_mut()
                .and_then(|watch| watch.words(Instant::now()))
        {
            self.spot_resolved(&done);
        }
        if live.text.on_partial(&text) {
            self.ending_state("partial_cancelled");
            self.queue_cue(live, Cue::Cancelled);
        }
        live.partial = Some(text).filter(|text| !text.trim().is_empty());
        self.publish(live);
    }

    fn on_final(&self, live: &mut Live<R::Session>, text: &str) -> Option<Done> {
        self.recorder
            .text(&live.dictation, format!("heard: {text}"));
        live.partial = None;
        if has_words(text) {
            live.notice = None;
        }
        let now = Instant::now();
        let before = live.watch.is_some().then(|| live.text.clone());
        let step = live.text.on_final(text, now, live.speech);
        if let (Some(before), Some(watch)) = (before, live.watch.as_mut())
            && let Some(done) =
                watch.final_arrived(Effect::of(&before, &live.text, &step), now, live.speech)
        {
            self.spot_resolved(&done);
        }
        match step {
            Err(full) => return Some(Done::Failed(full.into())),
            Ok(FinalStep::Cancelled { dictated }) => return Some(Done::NeverMind(dictated)),
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
                self.ending_state(match (pending.window, pending.without_ending) {
                    (QuietWindow::WaitingForQuiet, false) => "waiting_for_quiet",
                    (QuietWindow::WaitingForQuiet, true) => "waiting_for_quiet_without_ending",
                    (QuietWindow::CountingDown { .. }, false) => "pending",
                    (QuietWindow::CountingDown { .. }, true) => "pending_without_ending",
                });
                self.queue_cue(live, Cue::Provisional);
            }
        }
        self.publish(live);
        None
    }

    fn on_quiet(&self, live: &mut Live<R::Session>) -> Option<Done> {
        let now = Instant::now();
        let since = live.text.quiet_since();
        let without_ending = live.text.pending().is_some_and(|p| p.without_ending);
        let request = live.text.poll_ready(now)?;
        self.ending_state(if without_ending {
            "accepted_without_ending"
        } else {
            "accepted"
        });
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
    async fn on_frame(&mut self, live: &mut Live<R::Session>, queued: &Queued) -> Result<(), Stop> {
        let now = Instant::now();
        live.last_audio = now;
        let frame = &queued.frame;
        self.level.send_replace(loudness(frame));
        // Frames queued before the release are still the hold's, however late they are read. Once
        // the key is up nothing queued after it is said to the recognizer: what is still being
        // recognized finishes, and an aside spoken after the release stays out of the text.
        if live.after_release(queued.order) {
            return Ok(());
        }
        let pending = live.text.pending().is_some();
        let disputed = live.watch.as_ref().is_some_and(Watch::disputed);
        live.speech = self.vad.speech(frame, pending || disputed);
        if let Some(watch) = live.watch.as_mut() {
            if live.speech {
                watch.speaking();
            } else {
                watch.quiet_frame(now);
            }
        }
        if live.speech {
            live.heard.last = Some(now);
            if !live.heard.words_came {
                live.heard.first.get_or_insert(now);
            }
        }
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
            self.pause_for_speech(live);
            return self.send(live, std::slice::from_ref(frame)).await;
        }
        if live.speech_run >= CONTINUATION_FRAMES {
            live.relayed = true;
            self.ending_state("local_speech_relayed");
            self.pause_for_speech(live);
            let out: Vec<Frame> = live.held.drain(..).collect();
            return self.send(live, &out).await;
        }
        Ok(())
    }

    /// Stops a running countdown while the user is heard speaking, before any words come back.
    fn pause_for_speech(&self, live: &mut Live<R::Session>) {
        if live.speech && live.text.on_local_speech() {
            self.ending_state("local_speech_paused");
            self.publish(live);
        }
    }

    /// Sends `frames` and keeps them as what the user said.
    async fn send(&mut self, live: &mut Live<R::Session>, frames: &[Frame]) -> Result<(), Stop> {
        self.feed(live, frames).await?;
        self.spot(live, frames);
        if live.keep_sent {
            live.sent.extend_from_slice(frames);
        }
        Ok(())
    }

    /// Gives the ending spotter the frames the recognizer just took. It never changes the
    /// capture: a hit only starts a comparison with the transcript.
    fn spot(&mut self, live: &mut Live<R::Session>, frames: &[Frame]) {
        let (Some(spotter), Some(watch)) = (self.spotter.as_mut(), live.watch.as_mut()) else {
            return;
        };
        let now = Instant::now();
        let mut resolved = Vec::new();
        let mut broken = None;
        for frame in frames {
            watch.sent(1, now, live.speech);
            let started = std::time::Instant::now();
            let heard = spotter.push(frame);
            let took = started.elapsed();
            live.spot.frames += 1;
            live.spot.total += took;
            live.spot.slowest = live.spot.slowest.max(took);
            match heard {
                Ok(Some(hit)) => {
                    live.spot.hits += 1;
                    resolved.extend(watch.hit(&hit, now, live.speech));
                }
                Ok(None) => {}
                Err(error) => {
                    broken = Some(error);
                    break;
                }
            }
        }
        for done in &resolved {
            self.spot_resolved(done);
        }
        if let Some(error) = broken {
            live.watch = None;
            self.recorder.metric(
                Event::EndingSpotter,
                "error",
                None,
                json!({"call": "capture"}),
            );
            live.notice = Some(format!(
                "Ending phrase detection stopped: {error} Endings still work from the transcript."
            ));
            self.publish(live);
        }
    }

    /// Records how a heard ending ended; the user hears nothing of it.
    fn spot_resolved(&self, done: &Resolution) {
        self.recorder.metric(
            Event::EndingSpotter,
            done.outcome.name(),
            Some(done.since_hit),
            json!({
                "call": "hit",
                "ending": done.ending,
                "confidence": round3(done.confidence),
                "covering_after_ms": done.covering_after.map(millis),
            }),
        );
    }

    /// The quiet interval of a disputed ending passed: tells the user, once, that the transcript
    /// did not take it, on the overlay until the next words. Nothing in the capture changes.
    fn on_disputed(&self, live: &mut Live<R::Session>) {
        let Some((notice, done)) = live
            .watch
            .as_mut()
            .and_then(|watch| watch.notice(Instant::now()))
        else {
            return;
        };
        live.notice = Some(format!(
            "Heard \u{201c}{}\u{201d}, but the transcript did not end with it. Nothing was removed; say the ending again.",
            notice.phrase
        ));
        self.publish(live);
        self.queue_cue(live, Cue::Cancelled);
        self.spot_resolved(&done);
    }

    /// Settles what the spotter still waits for and records its work on the capture.
    fn close_spotting(&self, live: &mut Live<R::Session>) {
        if let Some(done) = live
            .watch
            .as_mut()
            .and_then(|watch| watch.finish(Instant::now()))
        {
            self.spot_resolved(&done);
        }
        live.spot.record(&self.recorder);
    }

    /// Sends `frames` to the recognizer only, giving up when the user cancels or a capture limit
    /// passes while the recognizer is not taking audio.
    async fn feed(&mut self, live: &mut Live<R::Session>, frames: &[Frame]) -> Result<(), Stop> {
        let limits = self.limits;
        for frame in frames {
            tokio::select! {
                biased;
                _ = self.cancel.changed() => return Err(Stop::Cancelled),
                sent = live.session.send_frame(frame) => sent.map_err(|e| Stop::Failed(e.into()))?,
                () = tokio::time::sleep_until(live.last_words + limits.idle()) => {
                    return Err(Stop::Failed(idle_error(limits.idle(), &live.text)));
                }
                () = tokio::time::sleep_until(live.started + limits.max()) => {
                    return Err(Stop::Failed(CaptureError::TooLong(limits.max())));
                }
            }
        }
        Ok(())
    }

    /// Plays the cue that ends the capture, releases the recognizer and settles the outcome.
    async fn conclude(
        &mut self,
        done: Done,
        decided: Instant,
        mut live: Option<Live<R::Session>>,
        dictation: &Dictation,
    ) -> CaptureOutcome {
        if let Some(live) = live.as_mut() {
            self.close_spotting(live);
        }
        let (session, sent, scratched, recoverable_text, has_endings) = match live {
            // Dropping the rest stops any cue still playing.
            Some(live) => (
                Some(live.session),
                live.sent,
                live.text.scratched(),
                recoverable(&live.text),
                live.text.has_endings(),
            ),
            None => (None, Vec::new(), false, None, true),
        };
        if !sent.is_empty() {
            self.recorder
                .speech(dictation, sent.as_flattened().to_vec());
        }
        let outcome = match done {
            Done::Stopped => {
                if let Some(session) = session {
                    session.cancel().await;
                }
                CaptureOutcome::Cancelled {
                    dictated: recoverable_text,
                }
            }
            Done::NeverMind(dictated) => {
                self.gate.set_enabled(false);
                self.end_with(Cue::Cancelled, decided, session, false).await;
                CaptureOutcome::Cancelled { dictated }
            }
            Done::Failed(error) => {
                self.gate.set_enabled(false);
                self.end_with(Cue::Error, decided, session, false).await;
                CaptureOutcome::Failed {
                    error,
                    recoverable_text,
                }
            }
            Done::Accepted(request) => {
                self.gate.set_enabled(false);
                if self.end_with(Cue::Accepted, decided, session, true).await {
                    match self
                        .reconcile(request, &sent, scratched, has_endings, dictation)
                        .await
                    {
                        CaptureOutcome::Accepted(request)
                            if request.route == CaptureRoute::Write =>
                        {
                            self.write(request.text).await
                        }
                        outcome => outcome,
                    }
                } else {
                    CaptureOutcome::cancelled_with(request.text)
                }
            }
        };
        self.recorder.text(
            dictation,
            match &outcome {
                CaptureOutcome::Accepted(request) => {
                    format!("accepted ({}): {}", request.route.name(), request.text)
                }
                CaptureOutcome::Cancelled { .. } => "cancelled".to_owned(),
                CaptureOutcome::Failed { error, .. } => format!("failed: {error}"),
            },
        );
        outcome
    }

    /// Plays `cue` while releasing `session`: closed when the capture was accepted, cancelled
    /// otherwise. False when the user cancelled while the cue played.
    async fn end_with(
        &mut self,
        cue: Cue,
        decided: Instant,
        session: Option<R::Session>,
        close: bool,
    ) -> bool {
        let release = async {
            match session {
                Some(session) if close => session.close().await,
                Some(session) => session.cancel().await,
                None => {}
            }
        };
        let (completed, ()) = tokio::join!(self.end_cue(cue, decided), release);
        completed
    }

    /// Replaces the accepted text with the first ticked entry's second transcription that agrees
    /// with the ending the user spoke, as soon as that is known: the entries after it are not
    /// waited for. They run on and are recorded when they finish. Keeps the live transcript when
    /// none agrees, and says so.
    async fn reconcile(
        &mut self,
        request: CaptureRequest,
        sent: &[Frame],
        scratched: bool,
        has_endings: bool,
        dictation: &Dictation,
    ) -> CaptureOutcome {
        if self.secondary.is_empty() || scratched {
            return CaptureOutcome::Accepted(request);
        }
        self.set_state(EngineState::TranscribingSecondary {
            route: request.route,
        });
        let started = Instant::now();
        let mut running = self.ask_all(&request, sent, has_endings, dictation);
        let mut slots: Vec<Option<Judged>> = self.secondary.iter().map(|_| None).collect();
        let winner = loop {
            match earliest_usable(&slots) {
                Usable::Waiting => {}
                Usable::Entry(index) => break Some(index),
                Usable::Nobody => break None,
            }
            tokio::select! {
                biased;
                _ = self.cancel.changed() => {
                    for entry in &self.secondary {
                        self.record_second_pass(entry, "cancelled", started.elapsed(), false);
                    }
                    return CaptureOutcome::cancelled_with(request.text);
                }
                answer = next_recorded(&mut self.recorded_only) => self.record_late(answer),
                Some(joined) = running.join_next() => {
                    let Some(answer) = settled(joined) else {
                        continue;
                    };
                    let judged = self.judge(answer);
                    if let Some(slot) = slots.get_mut(judged.answer.index) {
                        *slot = Some(judged);
                    }
                }
            }
        };
        let winner_name = winner
            .and_then(|index| self.secondary.get(index))
            .map(|entry| format!("\"{}\"", entry.name));
        let mut failed = Vec::new();
        let mut apart = Vec::new();
        for (index, judged) in slots.iter().enumerate() {
            let Some(judged) = judged else {
                continue;
            };
            self.record_answer(judged, winner == Some(index));
            if judged.outcome != "ok" {
                if winner.is_some_and(|winner| index > winner) {
                    apart.push(judged.answer.entry.name.as_str());
                } else {
                    failed.push(format!("\"{}\"", judged.answer.entry.name));
                }
            }
        }
        if !failed.is_empty() {
            let used = winner_name.map_or_else(
                || "the live transcript".to_owned(),
                |name| format!("the answer of {name}"),
            );
            (self.notice)(&format!(
                "The second transcription failed for {}. Using {used}.",
                failed.join(", ")
            ));
        }
        for name in apart {
            self.tell_only_recorded(name);
        }
        let text = winner
            .and_then(|index| slots.get_mut(index)?.as_mut())
            .and_then(|judged| judged.agreed.take());
        self.recorded_only.push(running);
        match text {
            Some(text) => CaptureOutcome::Accepted(CaptureRequest {
                route: request.route,
                text,
            }),
            None => CaptureOutcome::Accepted(request),
        }
    }

    /// Sends the capture to every ticked entry at once, each request in a task of its own.
    fn ask_all(
        &self,
        request: &CaptureRequest,
        sent: &[Frame],
        has_endings: bool,
        dictation: &Dictation,
    ) -> JoinSet<Answer> {
        let sent: Arc<[Frame]> = Arc::from(sent);
        let mut running = JoinSet::new();
        for (index, entry) in self.secondary.iter().enumerate() {
            let (http, entry, sent, dictation) = (
                self.http.clone(),
                entry.clone(),
                Arc::clone(&sent),
                dictation.clone(),
            );
            let route = request.route;
            running.spawn(async move {
                let asked = Instant::now();
                let heard = secondary::transcribe(&http, &entry.settings, &sent).await;
                Answer {
                    index,
                    entry,
                    dictation,
                    route,
                    has_endings,
                    heard,
                    took: asked.elapsed(),
                }
            });
        }
        running
    }

    fn judge(&self, answer: Answer) -> Judged {
        let (outcome, agreed) = match &answer.heard {
            Ok(raw) => match self.agreed(raw, answer.route, answer.has_endings) {
                Some(agreed) => ("ok", Some(agreed)),
                None => ("error", None),
            },
            Err(SecondaryError::Timeout) => ("timeout", None),
            Err(_) => ("error", None),
        };
        Judged {
            answer,
            outcome,
            agreed,
        }
    }

    fn record_answer(&self, judged: &Judged, used: bool) {
        let answer = &judged.answer;
        if let Ok(raw) = &answer.heard {
            self.recorder.text(
                &answer.dictation,
                format!("second transcription ({}): {raw}", answer.entry.name),
            );
        }
        self.record_second_pass(&answer.entry, judged.outcome, answer.took, used);
    }

    /// Records a second pass that finished after the paste, and tells the user when it failed.
    fn record_late(&self, answer: Answer) {
        let judged = self.judge(answer);
        self.record_answer(&judged, false);
        if judged.outcome != "ok" {
            self.tell_only_recorded(&judged.answer.entry.name);
        }
    }

    fn tell_only_recorded(&self, name: &str) {
        (self.notice)(&format!(
            "The second transcription for \"{name}\" failed; it was only recorded."
        ));
    }

    /// Has the writing model put `words` into proper text and hands it on as a paste. Nothing is
    /// pasted unless text came back whole within [`writing::LIMIT`]: otherwise the words are kept
    /// for the clipboard with the reason, or, when the user cancels meanwhile, as cancelled.
    async fn write(&mut self, words: String) -> CaptureOutcome {
        if !has_words(&words) {
            return CaptureOutcome::Accepted(CaptureRequest {
                route: CaptureRoute::Paste,
                text: words,
            });
        }
        let Some(settings) = self.writing.clone() else {
            return self.not_written(&words, Some(WritingError::NotSetUp), None);
        };
        self.set_state(EngineState::Writing {
            text: String::new(),
        });
        let started = Instant::now();
        let deadline = started + writing::LIMIT;
        let mut first = FirstPieces::default();
        let mut text = String::new();
        let mut reply = match bounded(
            &mut self.cancel,
            deadline,
            settings.start(&self.http, &words),
        )
        .await
        .finish()
        {
            Ok(reply) => reply,
            Err(stop) => return self.not_written(&words, stop, Some((started, first))),
        };
        loop {
            match bounded(&mut self.cancel, deadline, reply.next())
                .await
                .finish()
            {
                Ok(Some(Piece::Thinking)) => {
                    if first.text.is_none() {
                        self.set_state(EngineState::Thinking);
                    }
                    first.reasoning.get_or_insert_with(|| started.elapsed());
                }
                Ok(Some(Piece::Text(piece))) => {
                    first.text.get_or_insert_with(|| started.elapsed());
                    text.push_str(&piece);
                    self.set_state(EngineState::Writing { text: text.clone() });
                }
                Ok(None) => break,
                Err(stop) => {
                    let stop = stop.map(|error| first.explain(error));
                    return self.not_written(&words, stop, Some((started, first)));
                }
            }
        }
        let written = text.trim();
        if written.is_empty() {
            return self.not_written(&words, Some(WritingError::Empty), Some((started, first)));
        }
        self.writing_metric("ok", started, first);
        CaptureOutcome::Accepted(CaptureRequest {
            route: CaptureRoute::Paste,
            text: written.to_owned(),
        })
    }

    fn writing_metric(&self, outcome: &str, started: Instant, first: FirstPieces) {
        let Some(settings) = &self.writing else {
            return;
        };
        self.recorder.metric(
            Event::Writing,
            outcome,
            Some(started.elapsed()),
            json!({
                "model": settings.model(),
                "first_token_ms": first.text.map(millis),
                "first_reasoning_ms": first.reasoning.map(millis),
            }),
        );
    }

    /// The outcome of a writing call that gave no text: `error`, or a cancel when `None`.
    /// `call` is when the call started and what had arrived, `None` when none was made.
    fn not_written(
        &self,
        words: &str,
        error: Option<WritingError>,
        call: Option<(Instant, FirstPieces)>,
    ) -> CaptureOutcome {
        let outcome = error.map_or("cancelled", WritingError::kind);
        match call {
            Some((started, first)) => self.writing_metric(outcome, started, first),
            None => self
                .recorder
                .metric(Event::Writing, outcome, None, Value::Null),
        }
        match error {
            None => CaptureOutcome::cancelled_with(words.to_owned()),
            Some(error) => CaptureOutcome::Failed {
                error: CaptureError::Writing(error),
                recoverable_text: Some(words.to_owned()),
            },
        }
    }

    fn record_second_pass(
        &self,
        entry: &SecondPassEntry,
        outcome: &'static str,
        took: Duration,
        used: bool,
    ) {
        self.recorder.metric(
            Event::SecondaryStt,
            outcome,
            Some(took),
            json!({
                "entry": entry.name,
                "model": entry.settings.model(),
                "api": match entry.settings.api() {
                    SecondaryApi::OpenAi => "openai",
                    SecondaryApi::Meta => "meta",
                },
                "used": used,
            }),
        );
    }

    /// The secondary transcript without its ending phrase, when it ends the way the user did.
    /// A capture without endings (a hold) agrees with the whole transcript as dictated text.
    fn agreed(&self, raw: &str, route: CaptureRoute, has_endings: bool) -> Option<String> {
        if !has_endings {
            let mut parser = CaptureSession::without_endings();
            return match parser.on_final(raw, Instant::now(), false) {
                Ok(_) => Some(parser.text()).filter(|text| !text.is_empty()),
                Err(_) => None,
            };
        }
        let mut parser = CaptureSession::new(self.endings.clone(), self.pauses, Vec::new());
        let mut text = match parser.on_final(raw, Instant::now(), false).ok()? {
            FinalStep::Armed(pending) if pending.route == route => pending.text,
            FinalStep::Continue { .. } if !parser.scratched() => {
                return Some(parser.text()).filter(|text| !text.is_empty());
            }
            _ => return None,
        };
        // A second pass can hear the spoken ending twice ("Yes. Submit. Submit."); every trailing copy is the ending.
        loop {
            let mut parser = CaptureSession::new(self.endings.clone(), self.pauses, Vec::new());
            match parser.on_final(&text, Instant::now(), false) {
                Ok(FinalStep::Armed(pending)) if pending.route == route => text = pending.text,
                _ => break,
            }
        }
        (!text.is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognizer::ServerUrl;

    #[tokio::test]
    async fn spotter_totals_record_exact_timings_and_nothing_when_no_frame_was_processed() {
        for frames in [0, 2] {
            let dir = std::env::temp_dir()
                .join(format!("thalvi-spot-stats-{}-{frames}", std::process::id()));
            let recorder = Recorder::in_dir(
                RecordKinds {
                    metrics: true,
                    ..RecordKinds::default()
                },
                &dir,
            )
            .unwrap();
            let stats = if frames == 0 {
                SpotStats::default()
            } else {
                SpotStats {
                    frames,
                    hits: 1,
                    total: Duration::from_micros(3456),
                    slowest: Duration::from_micros(2345),
                }
            };
            stats.record(&recorder);
            recorder.finish().await;
            let path = dir.join("metrics.jsonl");
            if frames == 0 {
                assert!(!path.exists());
            } else {
                let lines = std::fs::read_to_string(&path).unwrap();
                let rows: Vec<Value> = lines
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(rows.len(), 1);
                let row = &rows[0];
                assert_eq!(row["call"], "capture");
                assert_eq!(row["frames"], 2);
                assert_eq!(row["hits"], 1);
                assert!((row["duration_ms"].as_f64().unwrap() - 3.456).abs() < 1e-9);
                assert_eq!(row["step_mean_ms"], 1.728);
                assert_eq!(row["step_max_ms"], 2.345);
            }
            if dir.exists() {
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }
    #[test]
    fn not_ready_names_what_waited_and_the_setting_to_fix() {
        let cases = [
            (
                SpeechSource::thalvi_server(
                    "Office",
                    &ServerUrl::parse("ws://127.0.0.1:9").unwrap(),
                ),
                "The Thalvi server \"Office\" at ws://127.0.0.1:9 was not ready after 8 seconds. Nothing was delivered. Start the Thalvi server, or fix its address under Settings → Speech.",
            ),
            (
                SpeechSource::direct("Cybrex2", "ws://100.64.0.2:8012".to_owned()),
                "The speech server \"Cybrex2\" at ws://100.64.0.2:8012 was not ready after 8 seconds. Nothing was delivered. Check that it is running, or fix its address under Settings → Speech.",
            ),
            (
                SpeechSource::on_device("On this computer", "nemotron_160ms".to_owned()),
                "The speech model \"On this computer\" (nemotron_160ms) was not ready after 8 seconds. Nothing was delivered. Wait for it to finish loading, or pick another under Settings → Speech.",
            ),
        ];
        for (speech, expected) in cases {
            let error = CaptureError::NotReady {
                waited: READY_TIMEOUT,
                speech,
            };
            assert_eq!(error.to_string(), expected);
        }
    }
}

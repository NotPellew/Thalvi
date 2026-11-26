//! Coordinates wake scoring, voice activity, cues, the recognizer and the silence timer into typed capture outcomes.
//!
//! One `!Send` owner runs on a current-thread runtime and `LocalSet`, so the voice activity and
//! wake detectors never cross a thread and every timer follows the runtime's clock.

pub mod fakes;
mod final_pass;
mod streaming;
mod submit_preview;

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
    Endings, FinalStep, PendingEnding, QuietWindow, has_words,
};
use crate::claim::{Effect, Resolution, Watch};
use crate::config::{DesktopConfig, Script};
use crate::cues::{Cue, CueEngine, CueError, CueTiming, Cues, Mic};
use crate::entries::{ExtraEntry, LiveEntry, LiveKind};
use crate::frame::{FRAME_SAMPLES, Frame, Order, Queued};
use crate::hold::{Cutoff, HoldKind, KeyState, Press, Presses, Refusal};
use crate::merge;
use crate::metrics::{Dictation, Event, Metrics, RecordKinds, Recorder};
use crate::models::CatalogSpeech;
use crate::numbers;
use crate::phrases::WordList;
use crate::recognizer::{
    Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, SpeechSource,
};
use crate::report::UserError;
use crate::secondary::{self, Language, SecondaryApi, SecondaryError};
use crate::spotter::KeywordSpotter;
use crate::transducer::SpeechChoice;
use crate::vad::VadDetector;
use crate::wake::{WakeEngine, WakeError, WakeEvent, WakeFrame};
use crate::wake_stats::{Latency, LatencySummary, NearMisses};
use crate::writing::{self, Destination, Endpoint, Piece, Writing, WritingError};
use final_pass::{FinalPass, Record as FinalPassRecord};
pub use streaming::StreamingExtra;
use streaming::Streams;
use submit_preview::Presentation;
pub use submit_preview::{PreviewError, SubmitPreview};

/// How long the wake cue and a recognizer that is not loading a model have to be ready before the capture is abandoned.
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

/// How long Submit waits for the overlay to confirm that the full review text is on screen.
const PREVIEW_WAIT: Duration = Duration::from_secs(1);

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

    /// Whether the frames now come from a file rather than a microphone.
    fn fed(&self) -> bool {
        false
    }
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
    /// timing is none when the cue never reached the output. With [`Mic::Muted`] the microphone
    /// hears nothing while it plays.
    fn play(&self, cue: Cue, mic: Mic)
    -> impl Future<Output = Result<Option<CueTiming>, CueError>>;

    /// The latest cue's first-sample timing if it was called at or after `since`.
    fn timing_since(&self, since: std::time::Instant) -> Option<CueTiming>;
}

/// Plays cues on the default output through [`CueEngine`].
pub struct CpalCuePlayer(Arc<CueEngine>);

impl CpalCuePlayer {
    /// Plays the cues switched on in `cues`, muting `gate` while a [`Mic::Muted`] one plays.
    #[must_use]
    pub fn new(cues: Cues, gate: InputGate) -> Self {
        let engine = Arc::new(CueEngine::new(cues, gate));
        let warming = Arc::clone(&engine);
        tokio::task::spawn_blocking(move || warming.warm_up());
        Self(engine)
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
    fn timing_since(&self, since: std::time::Instant) -> Option<CueTiming> {
        self.0.timing_since(since)
    }

    async fn play(&self, cue: Cue, mic: Mic) -> Result<Option<CueTiming>, CueError> {
        let called = std::time::Instant::now();
        let engine = Arc::clone(&self.0);
        let mut stop = StopOnDrop(Some(Arc::clone(&self.0)));
        let played = tokio::task::spawn_blocking(move || engine.play(cue, mic, called)).await;
        stop.0 = None;
        // A panic in the playback thread leaves the output in an unknown state.
        played.unwrap_or(Err(CueError::Stalled))
    }
}

/// A score to three decimals.
fn round3(score: f32) -> f64 {
    (f64::from(score) * 1000.0).round() / 1000.0
}

/// The dictation head's score for one frame, to three decimals.
fn scores(wake: &WakeFrame) -> Value {
    json!({"capture": round3(wake.capture)})
}

/// What the engine does with a wake word, for the caller to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeTrigger {
    /// The caller binds its capture target, then calls [`Engine::capture`].
    Capture,
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
    /// The wake cue has played and the dictation waits for the speech model to be read from disk;
    /// what the user says is kept.
    LoadingModel,
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
        /// For Submit, the overlay's report that the full text is on screen.
        preview: Option<SubmitPreview>,
    },
    /// The finished text is being shown before Submit can send it.
    ConfirmingSubmit {
        text: String,
        pause: Duration,
        preview: SubmitPreview,
    },
    /// The extra recognizers' texts are being merged with the live text, as far as they arrive in time.
    Merging {
        route: CaptureRoute,
    },
    FinalPass {
        route: CaptureRoute,
    },
    TranscribingLive {
        route: CaptureRoute,
    },
    /// The writing model is putting the words into proper text; `text` is what has streamed in.
    Writing {
        text: String,
    },
    /// The writing model is cleaning a dictation before it is pasted; the reply is pasted whole.
    CleaningUp,
    /// The writing model is reasoning and has written no text yet.
    Thinking,
    /// The app-owned chat runtime is loading, independently of generation.
    StartingChat {
        again: bool,
    },
}

/// Whether the live recognizer is Parakeet, which leaves out an English ending spoken after another language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndingListener {
    Parakeet,
    Other,
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
    #[error(
        "{} is still downloading ({percent}%). Nothing was delivered. Dictate again once it has finished.",
        speech.subject()
    )]
    ModelDownloading { speech: SpeechSource, percent: u8 },
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
    #[error("{}", ending_notice(*idle, *listener))]
    NoEnding {
        idle: Duration,
        listener: EndingListener,
    },
    #[error("Stopped after {} of silence.", plain_duration(*.0))]
    HeldSilence(Duration),
    #[error("Capture stopped after {} without an ending.", plain_duration(*.0))]
    TooLong(Duration),
    #[error(transparent)]
    SubmitPreview(#[from] PreviewError),
    #[error(transparent)]
    Full(#[from] CaptureFullError),
}

impl UserError for CaptureError {
    fn headline(&self) -> String {
        match self {
            Self::Recognizer(error) => return error.headline(),
            Self::NotReady { speech, .. } if speech.is_on_device() => {
                "Speech model not ready — Settings"
            }
            Self::NotReady { .. } => "Speech server not ready — Settings",
            Self::ModelDownloading { .. } => "Speech model still downloading",
            Self::Audio(error) => return error.headline(),
            Self::Writing(error) => return error.headline(),
            Self::MicStopped => "Microphone stopped mid-capture",
            Self::HoldTooShort => "Key released too soon",
            Self::NothingHeard | Self::NoWords(_) => "No words heard",
            Self::NoEnding { .. } => "No ending heard",
            Self::HeldSilence(_) => "Stopped after silence",
            Self::TooLong(_) => "Capture too long — not delivered",
            Self::SubmitPreview(_) => "Submit needs review",
            Self::Full(_) => "Capture full — finish or cancel",
        }
        .to_owned()
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

/// The ending timeout notice, with the cause and the fix when Parakeet is listening.
fn ending_notice(idle: Duration, listener: EndingListener) -> String {
    let notice = format!("Didn't hear an ending for {}.", plain_duration(idle));
    match listener {
        EndingListener::Other => notice,
        EndingListener::Parakeet => format!(
            "{notice} Parakeet may miss an English ending after other languages; add one in your language in Settings → Endings."
        ),
    }
}

impl CaptureError {
    /// A short name for the metrics, without any text.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Recognizer(RecognizerError::Behind { .. }) => "cant_keep_up",
            Self::Recognizer(_) => "recognizer",
            Self::NotReady { .. } => "not_ready",
            Self::ModelDownloading { .. } => "model_downloading",
            Self::Audio(_) => "audio",
            Self::MicStopped => "mic_stopped",
            Self::Writing(_) => "writing",
            Self::HoldTooShort => "hold_too_short",
            Self::NothingHeard => "hold_empty",
            Self::NoWords(_) | Self::NoEnding { .. } => "no_ending",
            Self::HeldSilence(_) => "held_silence",
            Self::TooLong(_) => "too_long",
            Self::SubmitPreview(_) => "submit_preview",
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
    fn headline(&self) -> String {
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
    pub chat: Option<crate::chat::Chat>,
    /// Whether the dictation key is down.
    pub hold: watch::Receiver<KeyState>,
    /// Tells the user about something that did not stop the capture.
    pub notice: Box<dyn Fn(&str)>,
    /// Listens for the ending phrases beside the recognizer; none when it could not be loaded.
    pub spotter: Option<Box<dyn KeywordSpotter>>,
    /// Why there is no spotter, for the first capture to tell the user once.
    pub spotter_off: Option<String>,
    /// Recognizers for the enabled streaming extras, or for a streaming final pass.
    pub extras: Vec<StreamingExtra<R>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListenState {
    Stopped,
    Active,
}

/// How a delivered dictation's number words are written: the switch, the word list whose tokens
/// stay, and the language the recognizer was told.
struct NumberRules {
    as_digits: bool,
    words: Vec<String>,
    language: Option<Language>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubmitConfirmation {
    Live,
    Settled,
}

/// Wake listening and capture, owning everything they share.
pub struct Engine<R, W, C> {
    recognizer: R,
    wake: W,
    cues: Rc<C>,
    ending_cues: JoinSet<()>,
    cues_on: Cues,
    gate: InputGate,
    vad: VadDetector,
    endings: Endings,
    pauses: EndingPauses,
    scripts: Vec<Script>,
    limits: CaptureLimits,
    /// The extras that are switched on, and the chat endpoint that merges their texts when merge is on.
    extras: Option<Extras>,
    final_pass: Option<Arc<FinalPass<R>>>,
    final_tasks: JoinSet<()>,
    final_record: Option<FinalPassRecord>,
    final_dictation: Option<Dictation>,
    streaming: Vec<StreamingExtra<R>>,
    /// The streaming extras of the capture in progress.
    streams: Streams,
    /// Extra requests the delivery did not wait for, one set per capture. Each runs as its own
    /// task, so its latency is its own, and is recorded when a loop of the engine collects it.
    /// Dropping the engine aborts them.
    recorded_only: Vec<JoinSet<Answer>>,
    writing: Option<Writing>,
    /// How a delivered paste, submit or write writes its number words.
    number_rules: NumberRules,
    /// The language of the capture's words: the entry hint until the recognizer or final pass
    /// reports the language it detected.
    language: Option<Language>,
    /// Whether a Submit is settled after the merge or final pass, so its own confirmation comes
    /// once that text exists.
    submit_confirmation: SubmitConfirmation,
    chat: Option<crate::chat::Chat>,
    speech: SpeechSource,
    /// The live recognizer's bearing on an ending spoken in another language.
    ending_listener: EndingListener,
    http: reqwest::Client,
    recorder: Recorder,
    notice: Rc<dyn Fn(&str)>,
    spotter: Option<Box<dyn KeywordSpotter>>,
    spotter_off: Option<String>,
    state: watch::Sender<EngineState>,
    level: watch::Sender<f32>,
    /// Cancels the previous ending cue before the next capture consumes this version.
    cancel_tx: Arc<watch::Sender<u64>>,
    cancel: watch::Receiver<u64>,
    level_frames: u64,
    level_activity: bool,
    capture_threshold: f32,
    /// Whether the wake word starts a capture; off, idle audio is not scored at all.
    wake_words: bool,
    /// Whether the previous frame's wake was refused as too quiet.
    refused: bool,
    /// The wake word that opened the next capture.
    woke: Option<Woke>,
    /// The window the capture in progress was bound to, for choosing a writing prompt.
    writing_window: Option<crate::host::WindowApp>,
    /// The latest frames scored while listening.
    wake_clip: VecDeque<Queued>,
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
    /// What the last merge told the user, until the caller takes it to show beside the delivery.
    merge_note: Option<String>,
    /// What the last dictation cleanup told the user, until the caller takes it to show beside the paste.
    cleanup_note: Option<String>,
    /// How the last cleanup ended (`cleaned` or `raw: <reason>`), and the words as recognized.
    cleanup_record: Option<(String, String)>,
}

/// A press of the dictation key that opened a capture.
struct Held {
    press: u64,
    kind: HoldKind,
    at: Instant,
    /// Where this press was released among the queued frames, however many presses followed.
    release: Cutoff,
    /// The frames heard since the key went down, when it waited to be told from a chord: the
    /// hold's first words, said before it could start.
    heard: Vec<Queued>,
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

fn cue_metric(metrics: &Metrics, event: Event, end: &CueEnd, outcome: &str) {
    if !end.played {
        return;
    }
    let mut fields = json!({"cue": cue_name(end.cue)});
    // `duration_ms` is play to drained; these tell our code, the device and the user's wait apart.
    if let Ok(Some(timing)) = &end.result {
        if timing.rebuilt {
            fields["rebuilt"] = json!(true);
        }
        fields["start_ms"] = json!(millis(timing.start));
        fields["output_ms"] = json!(timing.output.map(millis));
        fields["audible_ms"] = json!(timing.audible().map(millis));
        fields["perceived_ms"] =
            json!(timing.audible().map(|audible| millis(end.waited + audible)));
    }
    metrics.metric(event, outcome, Some(end.took), fields);
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
    capture_end: Option<Instant>,
    /// The release of the hold that opened this capture; frames queued after it are not speech
    /// of the hold.
    cutoff: Option<Cutoff>,
    /// What the ending spotter's hits mean; none when it does not listen to this capture.
    watch: Option<Watch>,
    spot: SpotStats,
    /// Shown with the capture until the next words are heard.
    notice: Option<String>,
    /// For a live Submit, the overlay's confirmation that the full review text is visible.
    submit_preview: Option<LivePreview>,
}

/// What a live Submit waits for from the overlay before the quiet pause can commit.
struct LivePreview {
    preview: SubmitPreview,
    watch: watch::Receiver<Presentation>,
    deadline: Instant,
}

impl LivePreview {
    fn new(now: Instant) -> Self {
        let preview = SubmitPreview::default();
        let watch = preview.watch();
        Self {
            preview,
            watch,
            deadline: now + PREVIEW_WAIT,
        }
    }

    fn waiting(&self) -> bool {
        *self.watch.borrow() == Presentation::Waiting
    }

    fn deadline(&self) -> Option<Instant> {
        self.waiting().then_some(self.deadline)
    }
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

struct QuietEnding {
    request: Option<CaptureRequest>,
    without_ending: bool,
    pause: Option<Duration>,
}

impl<S: RecognizerSession> Live<S> {
    /// Whether a dictation key opened this capture, so its release ends it.
    fn is_hold(&self) -> bool {
        !matches!(self.release, Release::Over)
    }

    /// Whether a frame queued at `order` came after the key went up and is not the hold's to
    /// send.
    fn after_release(&self, order: Order) -> bool {
        self.is_hold()
            && self
                .cutoff
                .as_ref()
                .is_some_and(|cutoff| cutoff.precedes(order))
    }

    /// When a released hold stops waiting for words.
    fn tail(&self) -> Option<Instant> {
        match self.release {
            Release::Tail(at) => Some(
                at + if self.session.buffers_audio() {
                    self.session.final_wait()
                } else {
                    RELEASE_LIMIT
                },
            ),
            _ => None,
        }
    }

    fn take_quiet_ending(&mut self) -> QuietEnding {
        let now = Instant::now();
        let pause = self
            .text
            .quiet_since()
            .map(|since| now.duration_since(since));
        let without_ending = self.text.pending().is_some_and(|p| p.without_ending);
        let ended = self.text.ready_at();
        let request = self.text.poll_ready(now);
        if request.is_some() {
            self.capture_end = self.capture_end.or(ended);
        }
        QuietEnding {
            request,
            without_ending,
            pause,
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
    /// A submit must be merged and cleaned before its confirmation is shown.
    Prepare(CaptureRequest),
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
    /// The overlay reported on a live Submit preview.
    Preview(Presentation),
    /// A live Submit preview was not reported shown in time.
    PreviewTimeout,
    /// An extra that was not waited for finished.
    Recorded(Answer),
}

/// What an extra recognizer is called in notices, recordings and metrics.
#[derive(Clone)]
struct Extra {
    name: String,
    model: String,
    api: &'static str,
}

impl Extra {
    fn upload(entry: &ExtraEntry) -> Self {
        Self {
            name: entry.name.clone(),
            model: entry.settings.model().to_owned(),
            api: match entry.settings.api() {
                SecondaryApi::OpenAi => "openai",
                SecondaryApi::Meta => "meta",
            },
        }
    }

    fn streaming(entry: &LiveEntry) -> Self {
        let (model, api) = match &entry.kind {
            LiveKind::Realtime(realtime) => (realtime.model().to_owned(), "realtime"),
            LiveKind::OpenAi(live) => (live.settings.model().to_owned(), "open_ai_live"),
            LiveKind::OnDevice(choice) => (choice.label(), "on_device"),
        };
        Self {
            name: entry.name.clone(),
            model,
            api,
        }
    }
}

/// The extras that are switched on, and the chat endpoint that merges their texts when merge is on.
#[derive(Clone)]
struct Extras {
    /// The extras that transcribe the finished capture.
    uploads: Vec<ExtraEntry>,
    /// Every extra, the uploads first and then the streaming ones, in the order of their slots.
    all: Vec<Extra>,
    /// The endpoint the merge asks; `None` while merge is off, which records the extras instead.
    merge: Option<Writing>,
    /// The word list every request into this set carries.
    words: WordList,
}

impl Extras {
    /// `None` unless an extra is on. A merge with no endpoint cannot be built: the config refuses
    /// merge on with an extra on and no endpoint, and an endpoint-less writing setting is dropped.
    fn of<'a>(
        config: &DesktopConfig,
        streaming: impl Iterator<Item = &'a LiveEntry>,
    ) -> Option<Self> {
        let uploads: Vec<_> = config.stt.entries.ticked().cloned().collect();
        let extras: Vec<_> = uploads
            .iter()
            .map(Extra::upload)
            .chain(streaming.map(Extra::streaming))
            .collect();
        let merge = config
            .writing
            .clone()
            .filter(|writing| config.stt.merge && Writing::has_endpoint(writing));
        (!extras.is_empty()).then_some(Self {
            uploads,
            all: extras,
            merge,
            words: config.stt.phrases.clone(),
        })
    }
}

/// Why a capture was delivered as the live text and not merged.
enum Skipped {
    /// Fewer than two texts were left; the string says why each extra gave none.
    TooFew(String),
    Timeout,
    Failed(WritingError),
    Unfaithful,
}

impl Skipped {
    fn outcome(&self) -> &'static str {
        match self {
            Self::TooFew(_) => "too_few",
            Self::Timeout => "timeout",
            Self::Failed(_) => "error",
            Self::Unfaithful => "unfaithful",
        }
    }

    fn reason(&self) -> String {
        match self {
            Self::TooFew(left_out) => format!("fewer than two texts arrived: {left_out}"),
            Self::Timeout => "the chat model took too long".to_owned(),
            Self::Failed(WritingError::Unreachable) => {
                "the chat model could not be reached".to_owned()
            }
            Self::Failed(WritingError::Status(code)) => {
                format!("the chat model failed with status {code}")
            }
            Self::Failed(WritingError::BadResponse) => {
                "the chat model sent an answer Thalvi could not read".to_owned()
            }
            Self::Failed(_) => "the chat model returned no text".to_owned(),
            Self::Unfaithful => {
                "the chat model used a word none of the recognizers heard".to_owned()
            }
        }
    }
}

/// How asking the chat model to merge ended.
enum Asked {
    Merged {
        text: String,
        chat: Duration,
    },
    Skipped {
        why: Skipped,
        chat: Option<Duration>,
    },
    Cancelled,
}

/// Why an extra gave no text.
#[derive(Clone, Copy)]
enum Unheard {
    Timeout,
    Failed,
    /// The capture was cancelled before the extra answered.
    Cancelled,
}

impl From<SecondaryError> for Unheard {
    fn from(error: SecondaryError) -> Self {
        match error {
            SecondaryError::Timeout => Self::Timeout,
            _ => Self::Failed,
        }
    }
}

/// One entry's reply to a capture, with what is needed to judge and record it when it arrives.
struct Answer {
    /// The entry's place among the switched-on ones.
    index: usize,
    extra: Extra,
    dictation: Dictation,
    route: CaptureRoute,
    has_endings: bool,
    heard: Result<String, Unheard>,
    /// For an upload, the request's time; for a streaming extra, from the capture's end to its text.
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

/// The next answer of an extra the merge did not wait for; never ready while none is running.
async fn next_recorded(sets: &mut Vec<JoinSet<Answer>>) -> Answer {
    std::future::poll_fn(|cx| {
        for at in 0..sets.len() {
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
                    Poll::Pending => break,
                }
            }
        }
        Poll::Pending
    })
    .await
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

/// Resolves with how far the download is once the model load downloads; never while it only reads
/// from disk, or when there is no load.
async fn download_started(load: &mut Option<watch::Receiver<crate::device::Step>>) -> u8 {
    if let Some(load) = load
        && let Ok(step) = load
            .wait_for(|step| step.download_percent().is_some())
            .await
        && let Some(percent) = step.download_percent()
    {
        return percent;
    }
    pending().await
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

/// The capture heard no new words for `idle`: words were heard but never their ending, or none at
/// all. A hold needs no spoken ending, so it only reports the silence.
fn idle_error(
    idle: Duration,
    text: &CaptureSession,
    listener: EndingListener,
    hold: bool,
) -> CaptureError {
    if recoverable(text).is_none() {
        CaptureError::NoWords(idle)
    } else if hold {
        CaptureError::HeldSilence(idle)
    } else {
        CaptureError::NoEnding { idle, listener }
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
    R: Recognizer + 'static,
    R::Session: 'static,
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
        let extras = if config.stt.entries.final_pass().is_none() {
            Extras::of(config, parts.extras.iter().map(|extra| &extra.entry))
        } else {
            None
        };
        let final_pass = FinalPass::of(config, &parts.extras).map(Arc::new);
        let engine = Self {
            recognizer: parts.recognizer,
            wake: parts.wake,
            cues: Rc::new(parts.cues),
            ending_cues: JoinSet::new(),
            cues_on: config.cues,
            gate: parts.gate,
            vad: VadDetector::new(),
            endings: config.endings.clone(),
            pauses: config.ending_pause,
            scripts: config.scripts.clone(),
            limits: config.capture_limits,
            extras,
            final_pass,
            final_tasks: JoinSet::new(),
            final_record: None,
            final_dictation: None,
            streaming: parts.extras,
            streams: Streams::default(),
            recorded_only: Vec::new(),
            writing: config.writing.clone(),
            number_rules: NumberRules {
                as_digits: config
                    .writing
                    .as_ref()
                    .is_none_or(Writing::numbers_as_digits),
                words: config.stt.phrases.phrases().to_vec(),
                language: config.stt.entries.active_live().language(),
            },
            language: config.stt.entries.active_live().language(),
            submit_confirmation: SubmitConfirmation::Live,
            chat: parts.chat,
            speech: config.speech_source(),
            ending_listener: if matches!(
                &config.stt.entries.active_live().kind,
                LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::ParakeetTdtV3))
            ) {
                EndingListener::Parakeet
            } else {
                EndingListener::Other
            },
            http: parts.http,
            recorder,
            notice: Rc::from(parts.notice),
            spotter: parts.spotter,
            spotter_off: parts.spotter_off,
            state,
            level,
            cancel_tx,
            cancel,
            level_frames: 0,
            level_activity: false,
            capture_threshold: config.wake.capture().threshold.get(),
            wake_words: config.wake.enabled(),
            refused: false,
            woke: None,
            writing_window: None,
            wake_clip: VecDeque::new(),
            key: Presses::new(parts.hold),
            listening: ListenState::Stopped,
            held: None,
            hold_end: None,
            held_for: None,
            ignored: Vec::new(),
            merge_note: None,
            cleanup_note: None,
            cleanup_record: None,
        };
        (engine, handle)
    }

    /// Stops and removes the local chat model between captures.
    ///
    /// # Errors
    /// Why its file could not be deleted.
    pub async fn remove_chat(&mut self) -> std::io::Result<()> {
        if let Some(chat) = self.chat.as_mut() {
            return chat.remove().await;
        }
        let store = crate::models::ModelStore::per_user().map_err(std::io::Error::other)?;
        crate::chat::remove_model(&store, &crate::chat::Build::Cpu.files()).await
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

    /// Switches the wake word on or off from the next [`listen`](Self::listen) on; the hold keys
    /// work either way. Taking `&mut self` keeps it from running during a capture.
    pub fn set_wake_words(&mut self, on: bool) {
        self.wake_words = on;
    }

    /// What the merge of the capture that just ended told the user, if it did: the delivery that
    /// follows would otherwise replace the notice on the overlay.
    pub fn take_merge_note(&mut self) -> Option<String> {
        self.merge_note.take()
    }

    /// Records the final pass once delivery has settled and returns its fallback notice, if any.
    pub fn settle_final_pass(
        &mut self,
        delivered: Option<CaptureRoute>,
        cancelled: bool,
    ) -> Option<String> {
        let record = self.final_record.take()?;
        let dictation = self.final_dictation.take()?;
        record.emit(
            &self.recorder,
            self.final_pass.as_ref()?.label(),
            &dictation,
            delivered,
            cancelled,
            self.speech.name(),
        )
    }

    /// What the cleanup of the capture that just ended told the user, if it fell back to the raw
    /// words: the delivery that follows would otherwise replace the notice on the overlay.
    pub fn take_cleanup_note(&mut self) -> Option<String> {
        self.cleanup_note.take()
    }

    /// How the cleanup of the capture that just ended went, and the words as recognized.
    pub fn take_cleanup(&mut self) -> Option<(String, String)> {
        self.cleanup_record.take()
    }

    /// The presses of the dictation key that started nothing since this was last called: each
    /// was already told through the notice and the metrics.
    pub fn take_ignored(&mut self) -> Vec<Refusal> {
        std::mem::take(&mut self.ignored)
    }

    /// Writes the recordings still queued and abandons extras still running; call once at
    /// shutdown.
    pub async fn finish(mut self) {
        while self.ending_cues.join_next().await.is_some() {}
        drop(self.recorded_only);
        drop(self.final_tasks);
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
        let mut misses = NearMisses::new(self.capture_threshold);
        let mut latency = Latency::default();
        let heard = self.scan(frames, &mut misses, &mut latency).await;
        if heard.is_ok() && !self.ending_cues.is_empty() {
            self.cancel_tx
                .send_modify(|count| *count = count.wrapping_add(1));
            while self.ending_cues.join_next().await.is_some() {}
            if matches!(heard, Ok(WakeTrigger::Hold)) && !self.gate.is_enabled() {
                self.gate.set_enabled(true);
            }
        }
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
            // The biased key branch is polled first; spending budget lets frames and timers run even if it stays ready.
            tokio::task::consume_budget().await;
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
            let queued = match frame {
                Ok(queued) => queued,
                Err(error) => {
                    self.level.send_replace(0.0);
                    return Err(error.into());
                }
            };
            let frame = queued.frame;
            self.level.send_replace(loudness(&frame));
            self.note_level(&frame, latency);
            self.remember(&queued);
            // Only the hold keys start a capture: the wake model has nothing to score for.
            if !self.wake_words {
                continue;
            }
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
                None => {}
            }
        }
    }

    /// Starts the capture, leaving the microphone open: the user talks from the press on.
    fn begin_hold(&mut self, press: Press) {
        let at = Instant::now();
        let heard = press.since.map_or_else(Vec::new, |since| {
            self.wake_clip
                .iter()
                .filter(|queued| queued.order >= since)
                .copied()
                .collect()
        });
        self.held = Some(Held {
            press: press.number,
            kind: press.kind,
            at,
            release: press.release,
            heard,
        });
        self.woke = Some(Woke {
            at,
            dictation: self.recorder.begin(),
        });
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
    fn remember(&mut self, queued: &Queued) {
        if self.wake_clip.len() == WAKE_CLIP_FRAMES {
            self.wake_clip.pop_front();
        }
        self.wake_clip.push_back(*queued);
    }

    /// Whether `wake` is too quiet to be speech and is dropped, recording the first frame of each
    /// run of refused wakes.
    fn refuse_quiet(&mut self, wake: &WakeFrame) -> bool {
        let level = self.wake_peak();
        if wake.event.is_none() || level >= WAKE_FLOOR_PEAK {
            self.refused = false;
            return false;
        }
        if !self.refused {
            self.refused = true;
            let mut fields = scores(wake);
            fields["level"] = json!(level);
            fields["floor"] = json!(WAKE_FLOOR_PEAK);
            self.recorder
                .metric(Event::WakeRefused, "capture", None, fields);
        }
        true
    }

    /// The largest sample magnitude over the last [`WAKE_FLOOR_FRAMES`] frames.
    fn wake_peak(&self) -> u16 {
        let recent = self.wake_clip.iter().rev().take(WAKE_FLOOR_FRAMES);
        recent
            .flat_map(|queued| queued.frame.iter().map(|sample| sample.unsigned_abs()))
            .max()
            .unwrap_or(0)
    }

    /// Records the frames up to and including the wake frame, oldest first.
    fn clip_wake(&self, dictation: &Dictation) {
        if !self.recorder.kinds().audio {
            return;
        }
        let mut pcm = Vec::with_capacity(self.wake_clip.len() * FRAME_SAMPLES);
        for queued in &self.wake_clip {
            pcm.extend_from_slice(&queued.frame);
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

    fn model_downloading(&self, percent: u8) -> CaptureError {
        CaptureError::ModelDownloading {
            speech: self.speech.clone(),
            percent,
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
    /// only. Returns to listening in every case. `window` is the app bound at wake, used only to
    /// choose a writing prompt; `None` when nothing was bound or this desktop names no app.
    /// Run within a [`tokio::task::LocalSet`] so the ending cue can continue during delivery.
    pub async fn capture(
        &mut self,
        frames: &mut impl FrameSource,
        window: Option<crate::host::WindowApp>,
    ) -> CaptureOutcome {
        self.writing_window = window;
        self.cancel.borrow_and_update();
        self.set_state(EngineState::StartingCapture);
        let source = if frames.fed() { "file" } else { "microphone" };
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
        self.final_dictation = self.final_pass.as_ref().map(|_| woke.dictation.clone());
        self.final_record = None;
        self.streams = self
            .extras
            .as_ref()
            .map_or_else(Streams::default, |extras| {
                Streams::start(&self.streaming, extras.uploads.len(), &woke.dictation)
            });
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
        self.skip_unfinished_final_pass(&outcome);
        self.drop_streams();
        let pressed = u64::from(self.key.take().is_some()) + self.key.take_lost();
        if pressed > 0 {
            self.ignore(Refusal::Capturing, pressed);
        }
        while self.ending_cues.try_join_next().is_some() {}
        if self.ending_cues.is_empty() {
            self.gate.set_enabled(true);
        }
        self.level.send_replace(0.0);
        self.set_state(EngineState::Listening);
        let route = match &outcome {
            CaptureOutcome::Accepted(request) => Some(request.route.name()),
            _ => None,
        };
        let (flow, mut fields) = match &outcome {
            CaptureOutcome::Accepted(_) => ("ok", json!({"source": source})),
            CaptureOutcome::Cancelled { .. } => ("cancelled", json!({"source": source})),
            CaptureOutcome::Failed { error, .. } => {
                ("failed", json!({"reason": error.kind(), "source": source}))
            }
        };
        if let Some(route) = route {
            fields["route"] = json!(route);
        }
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
            Some(pending)
                if self.submit_confirmation == SubmitConfirmation::Settled
                    && pending.route == CaptureRoute::Submit =>
            {
                EngineState::Capturing {
                    committed: pending.text.clone(),
                    partial: live.partial.clone(),
                    notice: live.notice.clone(),
                }
            }
            Some(pending) => EngineState::PendingEnding {
                route: pending.route,
                text: pending.text.clone(),
                waiting_for_quiet: pending.window == QuietWindow::WaitingForQuiet,
                preview: live
                    .submit_preview
                    .as_ref()
                    .map(|live| live.preview.clone()),
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
    fn cue(&self, cue: Cue, mic: Mic, trigger: Instant) -> LocalBoxFuture<'static, CueEnd> {
        let player = Rc::clone(&self.cues);
        let on = self.cues_on.enabled(cue);
        async move {
            let started = Instant::now();
            let result = if on {
                player.play(cue, mic).await
            } else {
                Ok(None)
            };
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
        let outcome = if end.result.is_ok() { "ok" } else { "error" };
        cue_metric(&self.recorder.metrics(), event, end, outcome);
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
        let played = self.cue(cue, Mic::Muted, decided);
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
    /// frames that arrived meanwhile (after the cue for a wake word, from the press for a hold, which
    /// the user talks over), and when the hold's key
    /// came up in the meantime, if speech was already waiting. A recognizer that loads a model on
    /// this computer is waited for as long as the load reads from disk, and refused as soon as it
    /// downloads.
    #[expect(
        clippy::too_many_lines,
        reason = "one select over every way the start can end"
    )]
    async fn start(
        &mut self,
        frames: &mut impl FrameSource,
        woke: &Woke,
        held: Option<&Held>,
    ) -> Result<(Live<R::Session>, Vec<Queued>, Option<Instant>), (Done, Instant)> {
        let started = Instant::now();
        let deadline = started + READY_TIMEOUT;
        let mut load = self.recognizer.loading();
        let loads_model = load.is_some();
        let recognizer = &self.recognizer;
        let mut connect = pin!(async move {
            let at = Instant::now();
            (recognizer.connect().await, at.elapsed())
        });
        let mic = if held.is_some() {
            Mic::Open
        } else {
            Mic::Muted
        };
        let mut wake_cue = self.cue(Cue::Wake, mic, woke.at);
        let mut session = None;
        let mut cue_ended = false;
        let mut backlog = held.map_or_else(Vec::new, |held| held.heard.clone());
        let mut released = None;
        let mut connection: (&str, Option<Duration>) = ("cancelled", None);
        let stop = loop {
            tokio::select! {
                biased;
                _ = self.cancel.changed() => break Some(Done::Stopped),
                end = &mut wake_cue, if !cue_ended => {
                    cue_ended = true;
                    // The cue engine does not restore a gate that was already off; resetting an open one drops the frames said over the cue.
                    if mic == Mic::Muted {
                        self.gate.set_enabled(true);
                    }
                    self.cue_metric(Event::ReadyCue, &end);
                    self.cue_failed(&end, "The capture continues.");
                }
                () = until((session.is_some() || !loads_model).then_some(deadline)) => break Some(Done::Failed(self.not_ready())),
                // Keep reading after release so a long load cannot fill the queue; the cutoff drops what follows.
                frame = frames.next(), if cue_ended => match frame {
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
                percent = download_started(&mut load), if session.is_none() => {
                    connection = ("downloading", None);
                    break Some(Done::Failed(self.model_downloading(percent)));
                }
            }
            // A release not yet dealt with comes first, whichever branch the select saw first.
            let release_pending =
                released.is_none() && held.is_some_and(|held| self.key.is_over(held.press));
            if cue_ended && session.is_some() && !release_pending {
                break None;
            }
            if cue_ended && session.is_none() && loads_model {
                self.set_state(EngineState::LoadingModel);
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
        self.language = self.number_rules.language;
        self.submit_confirmation = SubmitConfirmation::Live;
        let buffers_audio = session.buffers_audio();
        let live = Live {
            session,
            dictation: woke.dictation.clone(),
            text: self.capture_session(held.is_some()),
            partial: None,
            sent: Vec::new(),
            keep_sent: self.keeps_sent(buffers_audio),
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
            capture_end: None,
            cutoff: held.map(|held| held.release.clone()),
            watch: Self::watch_for(&mut self.spotter, self.pauses, held.is_some()),
            spot: SpotStats::default(),
            notice: held.is_none().then(|| self.spotter_off.take()).flatten(),
            submit_preview: None,
        };
        self.publish(&live);
        Ok((live, backlog, released))
    }

    /// Whether the frames sent are kept: for an upload extra, the recordings or a buffering recognizer.
    fn keeps_sent(&self, buffers_audio: bool) -> bool {
        self.final_pass.is_some()
            || self
                .extras
                .as_ref()
                .is_some_and(|extras| !extras.uploads.is_empty())
            || self.recorder.kinds().audio
            || buffers_audio
    }

    /// Sends frames buffered while connecting and any hold release that happened before ready.
    async fn start_drive(
        &mut self,
        live: &mut Live<R::Session>,
        backlog: Vec<Queued>,
        held: Option<&Held>,
        released: Option<Instant>,
    ) -> Option<Done> {
        for queued in backlog {
            if let Err(stop) = self.on_frame(live, &queued).await {
                return Some(stop.into());
            }
        }
        if let (Some(at), Some(held)) = (released, held) {
            self.held_for = Some(at.duration_since(held.at));
            if live.session.buffers_audio() {
                self.streams.send(&RELEASE_SILENCE);
                if let Err(error) = live.session.finish_audio().await {
                    return Some(Done::Failed(error.into()));
                }
                self.set_state(EngineState::TranscribingLive {
                    route: held.route(),
                });
            } else if let Err(stop) = self.feed(live, &RELEASE_SILENCE).await {
                return Some(stop.into());
            }
            live.release = Release::Tail(Instant::now());
        }
        None
    }

    fn on_preview(live: &mut Live<R::Session>, presentation: Presentation) -> Option<Done> {
        match presentation {
            Presentation::Shown => {
                if let Some(pending) = live.text.pending_mut()
                    && matches!(pending.window, QuietWindow::CountingDown { .. })
                {
                    pending.window = QuietWindow::CountingDown {
                        since: Instant::now(),
                    };
                }
                None
            }
            Presentation::Failed(error) => Some(Done::Failed(error.into())),
            Presentation::Waiting => None,
        }
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
        if let Some(done) = self.start_drive(live, backlog, held, released).await {
            return (done, Instant::now());
        }
        let mut key_open = true;
        loop {
            // The biased branches are polled in order; spending budget lets the later ones and the timers run even if one stays ready.
            tokio::task::consume_budget().await;
            let limit = if live.text.pending().is_some() {
                MIC_WATCHDOG
            } else {
                MIC_TIMEOUT
            };
            let waiting_on_live_upload =
                live.session.buffers_audio() && matches!(live.release, Release::Tail(_));
            let watchdog =
                (!waiting_on_live_upload && live.cues.is_empty()).then(|| live.last_audio + limit);
            let tail = live.tail();
            let preview_deadline = live.submit_preview.as_ref().and_then(LivePreview::deadline);
            let quiet_at = live.text.ready_at().filter(|_| preview_deadline.is_none());
            let preview_watch = live.submit_preview.as_mut().map(|p| &mut p.watch);
            let step = tokio::select! {
                biased;
                _ = self.cancel.changed() => Step::Cancel,
                changed = self.key.changed(), if key_open => Step::Key(changed.is_ok()),
                // Inline, so the branch is ready only when the overlay reports a change.
                preview = async move {
                    let Some(watch) = preview_watch else { return std::future::pending().await };
                    let unavailable = Presentation::Failed(PreviewError::Unavailable);
                    watch.changed().await.map_or(unavailable, |()| *watch.borrow_and_update())
                } => Step::Preview(preview),
                event = live.session.next_event() => Step::Event(event),
                frame = frames.next() => Step::Frame(frame),
                answer = next_recorded(&mut self.recorded_only) => Step::Recorded(answer),
                Some(end) = live.cues.next() => Step::Cue(end),
                () = until(quiet_at) => Step::Quiet,
                () = until(preview_deadline) => Step::PreviewTimeout,
                () = until(live.watch.as_ref().and_then(Watch::ready_at)) => Step::Disputed,
                () = until(watchdog) => Step::Silent,
                () = until(tail) => Step::Tail,
                () = tokio::time::sleep_until(live.last_words + self.limits.idle()), if !live.session.buffers_audio() => Step::Idle,
                () = tokio::time::sleep_until(live.started + self.limits.max()) => Step::TooLong,
            };
            let done = match step {
                Step::Cancel => Some(Done::Stopped),
                Step::Preview(presentation) => Self::on_preview(live, presentation),
                Step::PreviewTimeout => Some(Done::Failed(PreviewError::Unavailable.into())),
                Step::Event(event) => self.on_event(live, held, event),
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
                Step::Quiet => match Self::quiet_allowed(live) {
                    Ok(()) => self.on_quiet(live.take_quiet_ending()),
                    Err(held) => held,
                },
                Step::Disputed => {
                    self.on_disputed(live);
                    None
                }
                Step::Silent => Some(Done::Failed(CaptureError::MicStopped)),
                Step::Idle => Some(Done::Failed(idle_error(
                    self.limits.idle(),
                    &live.text,
                    self.ending_listener,
                    live.is_hold(),
                ))),
                Step::TooLong => Some(Done::Failed(CaptureError::TooLong(self.limits.max()))),
                Step::Tail => Some(if live.session.buffers_audio() {
                    Done::Failed(CaptureError::Recognizer(RecognizerError::said(
                        "The speech server took too long to transcribe this capture. Nothing was delivered.",
                    )))
                } else {
                    self.finish_hold(live, held)
                }),
                Step::Key(open) => {
                    key_open = open;
                    self.on_key(live, held, frames).await
                }
            };
            if let Some(done) = done {
                let Done::Prepare(request) = done else {
                    return (done, Instant::now());
                };
                // `decided` is the dictation final. The confirmation shows whatever this returns.
                let decided = Instant::now();
                if let Some(done) = self.prepare_submit(live, request, decided).await {
                    return (done, decided);
                }
            }
        }
    }

    fn on_event(
        &mut self,
        live: &mut Live<R::Session>,
        held: Option<&Held>,
        event: Result<RecognizerEvent, RecognizerError>,
    ) -> Option<Done> {
        match event {
            Err(error) => Some(Done::Failed(error.into())),
            Ok(RecognizerEvent::Partial(text)) => {
                if has_words(&text) {
                    live.last_words = Instant::now();
                    if !std::mem::replace(&mut live.heard.words_came, true) {
                        self.lag(Event::PartialLag, live.heard.first.take());
                    }
                }
                self.on_partial(live, text);
                None
            }
            Ok(RecognizerEvent::Final(text)) => {
                if let Some(language) = live.session.language() {
                    self.language = Some(language);
                }
                if has_words(&text) {
                    live.last_words = Instant::now();
                    self.lag(Event::FinalLag, live.heard.last.take());
                }
                let done = self.on_final(live, &text);
                done.or_else(|| {
                    matches!(live.release, Release::Tail(_)).then(|| {
                        if held.is_none()
                            && let Some(pending) = live.text.pending()
                        {
                            Done::Accepted(CaptureRequest {
                                route: pending.route,
                                text: pending.text.clone(),
                            })
                        } else {
                            self.finish_hold(live, held)
                        }
                    })
                })
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
        if live.session.buffers_audio() {
            self.streams.send(&RELEASE_SILENCE);
            if let Err(error) = live.session.finish_audio().await {
                return Some(Done::Failed(error.into()));
            }
            self.set_state(EngineState::TranscribingLive {
                route: held.route(),
            });
        } else if let Err(stop) = self.feed(live, &RELEASE_SILENCE).await {
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
        live.cues.push(self.cue(cue, Mic::Muted, Instant::now()));
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
            live.submit_preview = None;
            self.ending_state("partial_cancelled");
            self.queue_cue(live, Cue::Cancelled);
        }
        live.partial = Some(text).filter(|text| !text.trim().is_empty());
        self.publish(live);
    }

    fn on_final(&mut self, live: &mut Live<R::Session>, text: &str) -> Option<Done> {
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
                    live.submit_preview = None;
                    self.ending_state("final_cancelled");
                    self.queue_cue(live, Cue::Cancelled);
                }
            }
            Ok(FinalStep::Armed(mut pending)) => {
                live.submit_preview = None;
                // A Submit whose text only exists after the merge or final pass is formatted and
                // cleaned when it settles, and confirmed then. An ordinary submit's confirmation
                // shows its formatted text, which is sent as shown.
                let settled = pending.route == CaptureRoute::Submit
                    && (self.final_pass.is_some()
                        || (self.merge_on()
                            && self.number_rules.as_digits
                            && has_words(&pending.text)));
                self.submit_confirmation = if settled {
                    SubmitConfirmation::Settled
                } else {
                    SubmitConfirmation::Live
                };
                if pending.route == CaptureRoute::Submit && !settled && self.number_rules.as_digits
                {
                    pending.text = self.text_with_numbers(&pending.text);
                    if let Some(stored) = live.text.pending_mut() {
                        stored.text.clone_from(&pending.text);
                    }
                }
                if live.relayed {
                    live.relayed = false;
                    live.held.clear();
                    live.speech_run = 0;
                }
                live.last_audio = now;
                // A submit is settled (merged, then cleaned) before its confirmation is shown.
                if self.cleans_before_confirm(&pending) {
                    return Some(Done::Prepare(CaptureRequest {
                        route: pending.route,
                        text: pending.text,
                    }));
                }
                self.arm_pending(live);
            }
        }
        self.publish(live);
        None
    }

    /// Whether a quiet ending may be taken: a waiting Submit preview holds it (`Err(None)`), and
    /// a failed one ends the capture instead.
    fn quiet_allowed(live: &Live<R::Session>) -> Result<(), Option<Done>> {
        match live
            .submit_preview
            .as_ref()
            .map(|preview| *preview.watch.borrow())
        {
            None | Some(Presentation::Shown) => Ok(()),
            Some(Presentation::Waiting) => Err(None),
            Some(Presentation::Failed(error)) => Err(Some(Done::Failed(error.into()))),
        }
    }

    fn on_quiet(&self, ending: QuietEnding) -> Option<Done> {
        let request = ending.request?;
        self.ending_state(if ending.without_ending {
            "accepted_without_ending"
        } else {
            "accepted"
        });
        if let Some(pause) = ending.pause {
            self.recorder
                .metric(Event::EndingPause, "ok", Some(pause), Value::Null);
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
            self.send(live, &out).await?;
            if live.session.buffers_audio()
                && matches!(live.release, Release::Over)
                && !live.speech
                && live.heard.last.is_some_and(|last| {
                    now.duration_since(last) >= self.pauses.get(CaptureRoute::Paste)
                })
            {
                live.session
                    .finish_audio()
                    .await
                    .map_err(|error| Stop::Failed(error.into()))?;
                live.capture_end = Some(now);
                live.release = Release::Tail(now);
                self.set_state(EngineState::TranscribingLive {
                    route: CaptureRoute::Paste,
                });
            }
            return Ok(());
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

    /// Sends `frames` to the recognizer and the streaming extras, giving up when the user cancels
    /// or a capture limit passes while the recognizer is not taking audio.
    async fn feed(&mut self, live: &mut Live<R::Session>, frames: &[Frame]) -> Result<(), Stop> {
        let limits = self.limits;
        self.streams.send(frames);
        for frame in frames {
            tokio::select! {
                biased;
                _ = self.cancel.changed() => return Err(Stop::Cancelled),
                sent = live.session.send_frame(frame) => sent.map_err(|e| Stop::Failed(e.into()))?,
                () = tokio::time::sleep_until(live.last_words + limits.idle()) => {
                    return Err(Stop::Failed(idle_error(
                        limits.idle(),
                        &live.text,
                        self.ending_listener,
                        live.is_hold(),
                    )));
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
        let (session, mut sent, scratched, recoverable_text, has_endings, ended) = match live {
            // Dropping the rest stops any cue still playing.
            Some(live) => (
                Some(live.session),
                live.sent,
                live.text.scratched(),
                recoverable(&live.text),
                live.text.has_endings(),
                live.cutoff
                    .as_ref()
                    .and_then(Cutoff::released_at)
                    .or(live.capture_end)
                    .unwrap_or(decided),
            ),
            None => (None, Vec::new(), false, None, true, decided),
        };
        if !sent.is_empty() {
            self.recorder
                .speech(dictation, sent.as_flattened().to_vec());
        }
        if !matches!(done, Done::Accepted(_)) {
            self.drop_streams();
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
                // A task acts on the text the user heard before it ran; a merge could pick another action.
                let extras = self.extras.clone().filter(|_| {
                    !scratched && has_words(&request.text) && request.route != CaptureRoute::Task
                });
                if extras.is_some() {
                    self.streams.finish(request.route, has_endings);
                } else {
                    self.drop_streams();
                }
                let final_work =
                    self.start_final_pass(&request, &mut sent, dictation, ended, scratched);
                if self.end_with(Cue::Accepted, decided, session, true).await {
                    let resolved = match extras {
                        Some(extras) => {
                            self.deliver_with_extras(extras, request, &sent, has_endings, dictation)
                                .await
                        }
                        None => match final_work {
                            Some(work) => self.wait_final_pass(request, work, has_endings).await,
                            None => CaptureOutcome::Accepted(request),
                        },
                    };
                    self.finish_transcript(resolved, decided).await
                } else {
                    if let Some(work) = final_work {
                        self.final_record = Some(work.cancel());
                    }
                    self.drop_streams();
                    CaptureOutcome::cancelled_with(request.text)
                }
            }
            // A submit is prepared inside the capture, before its confirmation.
            Done::Prepare(_) => {
                CaptureOutcome::cancelled_with(recoverable_text.unwrap_or_default())
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

    async fn finish_transcript(
        &mut self,
        outcome: CaptureOutcome,
        decided: Instant,
    ) -> CaptureOutcome {
        let outcome = match outcome {
            CaptureOutcome::Accepted(request)
                if self.number_rules.as_digits && self.formats_on_delivery(request.route) =>
            {
                CaptureOutcome::Accepted(self.delivered_with_numbers(&request))
            }
            other => other,
        };
        match outcome {
            CaptureOutcome::Accepted(request)
                if request.route == CaptureRoute::Submit
                    && self.submit_confirmation == SubmitConfirmation::Settled =>
            {
                self.confirm_final_submit(request).await
            }
            CaptureOutcome::Accepted(request) if request.route == CaptureRoute::Write => {
                self.write(request.text).await
            }
            // A submit is cleaned before its confirmation is shown.
            CaptureOutcome::Accepted(request)
                if request.route == CaptureRoute::Paste
                    && self
                        .writing
                        .as_ref()
                        .is_some_and(writing::Writing::clean_dictation)
                    && has_words(&request.text) =>
            {
                let cleanup_start = if self.final_pass.is_some() {
                    Instant::now()
                } else {
                    decided
                };
                self.clean(request, cleanup_start).await
            }
            outcome => outcome,
        }
    }

    /// Plays `cue` while releasing `session`: closed when the capture was accepted, cancelled
    /// otherwise. Accepted captures wait only for release; the cue keeps playing independently.
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
        if !close {
            let (completed, ()) = tokio::join!(self.end_cue(cue, decided), release);
            return completed;
        }
        while self.ending_cues.try_join_next().is_some() {}
        let played = self.cue(cue, Mic::Muted, decided);
        let metrics = self.recorder.metrics();
        let notice = Rc::clone(&self.notice);
        let mut cancel = self.cancel.clone();
        let gate = self.gate.clone();
        let state = self.state.clone();
        let cues = Rc::clone(&self.cues);
        self.ending_cues.spawn_local(async move {
            {
                let called = std::time::Instant::now();
                let started = Instant::now();
                tokio::pin!(played);
                tokio::select! {
                    biased;
                    _ = cancel.changed() => {
                        let end = CueEnd {
                            cue,
                            played: true,
                            took: started.elapsed(),
                            waited: started.saturating_duration_since(decided),
                            result: Ok(cues.timing_since(called)),
                        };
                        cue_metric(&metrics, Event::EndCue, &end, "cancelled");
                    }
                    end = &mut played => {
                        let outcome = if end.result.is_ok() { "ok" } else { "error" };
                        cue_metric(&metrics, Event::EndCue, &end, outcome);
                        if let Err(error) = end.result {
                            notice(&format!("{error} The capture continues."));
                        }
                    }
                }
            }
            if *state.borrow() == EngineState::Listening {
                gate.set_enabled(true);
            }
        });
        tokio::pin!(release);
        tokio::select! {
            biased;
            _ = self.cancel.changed() => {
                release.await;
                false
            }
            () = &mut release => true,
        }
    }

    /// Merges the live text with the texts of the switched-on extras that arrive in time: the chat
    /// model chooses between what the recognizers heard, and the live text is kept, with a notice,
    /// when that cannot be done faithfully. The route is the live transcript's and stays it.
    async fn merge(
        &mut self,
        writing: &Writing,
        extras: &Extras,
        request: CaptureRequest,
        sent: &[Frame],
        has_endings: bool,
        dictation: &Dictation,
    ) -> CaptureOutcome {
        self.set_state(EngineState::Merging {
            route: request.route,
        });
        let started = Instant::now();
        let mut running = self.ask_all(
            &extras.uploads,
            &extras.words,
            &request,
            sent,
            has_endings,
            dictation,
        );
        let mut streams = std::mem::take(&mut self.streams);
        let mut slots: Vec<Option<Judged>> = extras.all.iter().map(|_| None).collect();
        while slots.iter().any(Option::is_none) {
            tokio::select! {
                biased;
                _ = self.cancel.changed() => {
                    self.recorded_only.push(streams.cancel());
                    return self.merge_cancelled(writing, extras, &slots, started, request.text);
                }
                () = tokio::time::sleep_until(started + merge::EXTRAS_WAIT) => break,
                answer = next_recorded(&mut self.recorded_only) => self.record_late(answer),
                Some(joined) = running.join_next() => self.fill(&mut slots, joined),
                Some(joined) = streams.tasks.join_next() => self.fill(&mut slots, joined),
            }
        }
        let mut inputs = vec![request.text.clone()];
        let mut left_out = Vec::new();
        for (extra, slot) in extras.all.iter().zip(&slots) {
            match slot.as_ref().and_then(|judged| judged.agreed.as_ref()) {
                Some(text) => inputs.push(text.clone()),
                None => left_out.push(match slot {
                    None => format!("\"{}\" was too slow", extra.name),
                    Some(judged) if judged.answer.heard.is_err() => {
                        format!("\"{}\" failed", extra.name)
                    }
                    Some(_) => format!("\"{}\" heard another ending", extra.name),
                }),
            }
        }
        let left_out = left_out.join(", ");
        let asked = if inputs.len() < 2 {
            Asked::Skipped {
                why: Skipped::TooFew(left_out.clone()),
                chat: None,
            }
        } else {
            self.ask_merge(writing, &inputs, &extras.words, started + merge::LIMIT)
                .await
        };
        if matches!(asked, Asked::Cancelled) {
            self.recorded_only.push(streams.cancel());
            return self.merge_cancelled(writing, extras, &slots, started, request.text);
        }
        let merged = matches!(asked, Asked::Merged { .. });
        for judged in slots.iter().flatten() {
            self.record_answer(judged, merged && judged.agreed.is_some());
        }
        self.recorded_only.push(running);
        self.recorded_only.push(streams.tasks);
        match asked {
            Asked::Merged { text, chat } => {
                self.merge_metric("merged", started, writing, inputs.len(), Some(chat));
                self.recorder.text(dictation, format!("merged: {text}"));
                if !left_out.is_empty() {
                    self.tell_merge(format!("Merged without some texts: {left_out}."));
                }
                CaptureOutcome::Accepted(CaptureRequest {
                    route: request.route,
                    text,
                })
            }
            Asked::Skipped { why, chat } => {
                self.merge_metric(why.outcome(), started, writing, inputs.len(), chat);
                self.tell_merge(format!(
                    "Merge skipped: {}. Used the live text.",
                    why.reason()
                ));
                CaptureOutcome::Accepted(request)
            }
            Asked::Cancelled => CaptureOutcome::cancelled_with(request.text),
        }
    }

    /// Delivers an accepted capture: merged with the extras' texts when merge is on, or at once
    /// with the extras only recorded.
    async fn deliver_with_extras(
        &mut self,
        extras: Extras,
        request: CaptureRequest,
        sent: &[Frame],
        has_endings: bool,
        dictation: &Dictation,
    ) -> CaptureOutcome {
        if let Some(writing) = &extras.merge {
            self.merge(writing, &extras, request, sent, has_endings, dictation)
                .await
        } else {
            self.record_only(&extras, &request, sent, has_endings, dictation);
            CaptureOutcome::Accepted(request)
        }
    }

    /// Sends the capture to every extra and records what each answers, without waiting: the live
    /// text is delivered at once.
    fn record_only(
        &mut self,
        extras: &Extras,
        request: &CaptureRequest,
        sent: &[Frame],
        has_endings: bool,
        dictation: &Dictation,
    ) {
        let running = self.ask_all(
            &extras.uploads,
            &extras.words,
            request,
            sent,
            has_endings,
            dictation,
        );
        let streams = std::mem::take(&mut self.streams);
        self.recorded_only.push(running);
        self.recorded_only.push(streams.tasks);
    }

    /// Whether the chat model merges the extras' texts before delivery.
    fn merge_on(&self) -> bool {
        self.extras
            .as_ref()
            .is_some_and(|extras| extras.merge.is_some())
    }

    fn tell_merge(&mut self, note: String) {
        (self.notice)(&note);
        self.merge_note = Some(note);
    }

    /// Records the extras that had answered as they did, the others and the merge as cancelled;
    /// nothing is delivered.
    fn merge_cancelled(
        &self,
        writing: &Writing,
        extras: &Extras,
        slots: &[Option<Judged>],
        started: Instant,
        text: String,
    ) -> CaptureOutcome {
        for (index, (extra, slot)) in extras.all.iter().zip(slots).enumerate() {
            match slot {
                Some(judged) => self.record_answer(judged, false),
                // A streaming extra's cancelled answer is recorded when its task ends.
                None if index >= extras.uploads.len() => {}
                None => self.record_extra(extra, "cancelled", started.elapsed(), false),
            }
        }
        let heard = slots
            .iter()
            .flatten()
            .filter(|judged| judged.agreed.is_some())
            .count();
        self.merge_metric("cancelled", started, writing, heard + 1, None);
        CaptureOutcome::cancelled_with(text)
    }

    /// Asks the chat endpoint to merge `inputs`, the live text first, within `deadline`, and checks
    /// that its text invents no word.
    async fn ask_merge(
        &mut self,
        writing: &Writing,
        inputs: &[String],
        words: &WordList,
        deadline: Instant,
    ) -> Asked {
        let loading = Instant::now();
        let previous = self.state.borrow().clone();
        let endpoint = match self.endpoint(writing).await {
            Ok(endpoint) => endpoint,
            Err(None) => return Asked::Cancelled,
            Err(Some(error)) => {
                return Asked::Skipped {
                    why: Skipped::Failed(error),
                    chat: None,
                };
            }
        };
        let deadline = if matches!(writing.destination(), Destination::Computer { .. }) {
            deadline + loading.elapsed()
        } else {
            deadline
        };
        self.set_state(previous);
        let asked = Instant::now();
        let stopped = |stop: Option<WritingError>| match stop {
            None => Asked::Cancelled,
            Some(error) => Asked::Skipped {
                why: match error {
                    WritingError::Timeout => Skipped::Timeout,
                    error => Skipped::Failed(error),
                },
                chat: Some(asked.elapsed()),
            },
        };
        let mut reply = match bounded(
            &mut self.cancel,
            deadline,
            endpoint.start_merge(&self.http, inputs, words),
        )
        .await
        .finish()
        {
            Ok(reply) => reply,
            Err(stop) => return stopped(stop),
        };
        let mut text = String::new();
        loop {
            match bounded(&mut self.cancel, deadline, reply.next())
                .await
                .finish()
            {
                Ok(Some(Piece::Text(piece))) => text.push_str(&piece),
                Ok(Some(Piece::Thinking)) => {}
                Ok(None) => break,
                Err(stop) => return stopped(stop),
            }
        }
        let text = text.trim();
        let skipped = |why| Asked::Skipped {
            why,
            chat: Some(asked.elapsed()),
        };
        if !has_words(text) {
            skipped(Skipped::Failed(WritingError::Empty))
        } else if !merge::faithful(inputs, text) {
            skipped(Skipped::Unfaithful)
        } else {
            Asked::Merged {
                text: text.to_owned(),
                chat: asked.elapsed(),
            }
        }
    }

    /// Records one merge: `chat` is the chat call's own latency, the duration what the merge added
    /// after the ending.
    fn merge_metric(
        &self,
        outcome: &str,
        started: Instant,
        writing: &Writing,
        inputs: usize,
        chat: Option<Duration>,
    ) {
        self.recorder.metric(
            Event::Merge,
            outcome,
            Some(started.elapsed()),
            json!({
                "model": writing.model(),
                "inputs": inputs,
                "chat_ms": chat.map(millis),
            }),
        );
    }

    /// Sends the capture to every extra at once, each request in a task of its own.
    fn ask_all(
        &self,
        uploads: &[ExtraEntry],
        words: &WordList,
        request: &CaptureRequest,
        sent: &[Frame],
        has_endings: bool,
        dictation: &Dictation,
    ) -> JoinSet<Answer> {
        let sent: Arc<[Frame]> = Arc::from(sent);
        let mut running = JoinSet::new();
        for (index, entry) in uploads.iter().enumerate() {
            let (http, entry, sent, dictation) = (
                self.http.clone(),
                entry.clone(),
                Arc::clone(&sent),
                dictation.clone(),
            );
            let words = words.clone();
            let route = request.route;
            running.spawn(async move {
                let asked = Instant::now();
                let heard = secondary::transcribe(&http, &entry.settings, &sent, &words)
                    .await
                    .map(|transcript| transcript.text)
                    .map_err(Unheard::from);
                Answer {
                    index,
                    extra: Extra::upload(&entry),
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

    /// Puts the answer in its slot, judged.
    fn fill(&self, slots: &mut [Option<Judged>], joined: Result<Answer, JoinError>) {
        if let Some(answer) = settled(joined) {
            let judged = self.judge(answer);
            if let Some(slot) = slots.get_mut(judged.answer.index) {
                *slot = Some(judged);
            }
        }
    }

    /// Has the streaming extras of the capture drop their sessions; each is recorded as cancelled
    /// once its task has.
    fn drop_streams(&mut self) {
        let tasks = std::mem::take(&mut self.streams).cancel();
        if !tasks.is_empty() {
            self.recorded_only.push(tasks);
        }
    }

    fn judge(&self, answer: Answer) -> Judged {
        let (outcome, agreed) = match &answer.heard {
            Ok(raw) => match self.agreed(raw, answer.route, answer.has_endings, false) {
                Some(agreed) => ("ok", Some(agreed)),
                None => ("error", None),
            },
            Err(Unheard::Timeout) => ("timeout", None),
            Err(Unheard::Cancelled) => ("cancelled", None),
            Err(Unheard::Failed) => ("error", None),
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
                format!("extra ({}): {raw}", answer.extra.name),
            );
        }
        self.record_extra(&answer.extra, judged.outcome, answer.took, used);
    }

    /// Records an extra that finished after the delivery did not wait for it.
    fn record_late(&self, answer: Answer) {
        let judged = self.judge(answer);
        self.record_answer(&judged, false);
    }

    /// Whether an armed submit is cleaned before its confirmation, so that confirmation shows
    /// the text that will be sent. Paste is cleaned after its pause, in [`Self::conclude`].
    fn cleans_before_confirm(&self, pending: &PendingEnding) -> bool {
        pending.route == CaptureRoute::Submit
            && self.submit_confirmation == SubmitConfirmation::Live
            && self.final_pass.is_none()
            && self
                .writing
                .as_ref()
                .is_some_and(writing::Writing::clean_dictation)
            && has_words(&pending.text)
    }

    /// Cleans an armed submit before its confirmation is published, so that confirmation is the
    /// text that will be sent. A cancel sends nothing. `decided` is the dictation final, where
    /// the 3 s limit starts.
    async fn prepare_submit(
        &mut self,
        live: &mut Live<R::Session>,
        request: CaptureRequest,
        decided: Instant,
    ) -> Option<Done> {
        match self.clean(request, decided).await {
            CaptureOutcome::Cancelled { .. } => Some(Done::Stopped),
            CaptureOutcome::Failed { error, .. } => Some(Done::Failed(error)),
            CaptureOutcome::Accepted(cleaned) => {
                if let Some(pending) = live.text.pending_mut() {
                    pending.text = cleaned.text;
                    // The pause is the confirmation of this text, so it starts now. Time spent
                    // cleaning does not count toward it.
                    if matches!(pending.window, QuietWindow::CountingDown { .. }) {
                        pending.window = QuietWindow::CountingDown {
                            since: Instant::now(),
                        };
                    }
                }
                self.arm_pending(live);
                self.publish(live);
                None
            }
        }
    }

    async fn confirm_final_submit(&mut self, request: CaptureRequest) -> CaptureOutcome {
        let outcome = if self.writing.as_ref().is_some_and(Writing::clean_dictation)
            && has_words(&request.text)
        {
            self.clean(request, Instant::now()).await
        } else {
            CaptureOutcome::Accepted(request)
        };
        let CaptureOutcome::Accepted(request) = outcome else {
            return outcome;
        };
        let submit = CaptureRoute::Submit;
        let pause = self
            .pauses
            .get(submit)
            .max(EndingPauses::default().get(submit));
        let preview = SubmitPreview::default();
        let mut presentation = preview.watch();
        self.set_state(EngineState::ConfirmingSubmit {
            text: request.text.clone(),
            pause,
            preview,
        });
        let shown = tokio::select! {
            biased;
            _ = self.cancel.changed() => return CaptureOutcome::cancelled_with(request.text),
            result = presentation.wait_for(|state| *state != Presentation::Waiting) => {
                result.map_or(Presentation::Failed(PreviewError::Unavailable), |state| *state)
            }
            () = tokio::time::sleep(PREVIEW_WAIT) => Presentation::Failed(PreviewError::Unavailable),
        };
        let failure = match shown {
            Presentation::Failed(error) => error,
            Presentation::Waiting => PreviewError::Unavailable,
            Presentation::Shown => tokio::select! {
                biased;
                _ = self.cancel.changed() => return CaptureOutcome::cancelled_with(request.text),
                result = presentation.wait_for(|state| matches!(state, Presentation::Failed(_))) => {
                    match result.as_deref() {
                        Ok(Presentation::Failed(error)) => *error,
                        _ => PreviewError::Unavailable,
                    }
                }
                () = tokio::time::sleep(pause) => return CaptureOutcome::Accepted(request),
            },
        };
        CaptureOutcome::Failed {
            error: failure.into(),
            recoverable_text: Some(request.text),
        }
    }

    /// Records and announces an armed ending. The caller publishes afterwards.
    fn arm_pending(&self, live: &mut Live<R::Session>) {
        let Some(pending) = live.text.pending() else {
            live.submit_preview = None;
            return;
        };
        let route = pending.route;
        let window = pending.window;
        let without_ending = pending.without_ending;
        live.submit_preview = (route == CaptureRoute::Submit
            && self.submit_confirmation == SubmitConfirmation::Live)
            .then(|| LivePreview::new(Instant::now()));
        self.ending_state(match (window, without_ending) {
            (QuietWindow::WaitingForQuiet, false) => "waiting_for_quiet",
            (QuietWindow::WaitingForQuiet, true) => "waiting_for_quiet_without_ending",
            (QuietWindow::CountingDown { .. }, false) => "pending",
            (QuietWindow::CountingDown { .. }, true) => "pending_without_ending",
        });
        self.queue_cue(live, Cue::Provisional);
    }

    /// The text about to be delivered, with its number words as digits. A task is never
    /// formatted. Counts how many numbers changed, never the text.
    fn delivered_with_numbers(&self, request: &CaptureRequest) -> CaptureRequest {
        CaptureRequest {
            route: request.route,
            text: self.text_with_numbers(&request.text),
        }
    }

    /// Whether delivery formats this route's text: a task never is, and an ordinary submit was
    /// formatted before its confirmation, so only the settled ones still wait for formatting.
    fn formats_on_delivery(&self, route: CaptureRoute) -> bool {
        matches!(route, CaptureRoute::Paste | CaptureRoute::Write)
            || (route == CaptureRoute::Submit
                && self.submit_confirmation == SubmitConfirmation::Settled)
    }

    /// `text` with its number words as digits; callers only pass formatted routes with digits on.
    fn text_with_numbers(&self, text: &str) -> String {
        let formatted = numbers::format(text, self.language, &self.number_rules.words);
        if formatted.rewritten > 0 {
            self.recorder.metric(
                Event::Numbers,
                "ok",
                None,
                json!({"count": formatted.rewritten}),
            );
        }
        formatted.text
    }

    /// Sends the text that would be delivered through the writing endpoint with the built-in
    /// cleanup prompt. The whole wait, including the endpoint becoming ready, is bounded by
    /// [`writing::CLEANUP_LIMIT`] from `decided` (the dictation final). A complete reply inside
    /// that limit is delivered instead; an error or the limit pastes the raw words and says so;
    /// a cancel pastes nothing.
    async fn clean(&mut self, request: CaptureRequest, decided: Instant) -> CaptureOutcome {
        let raw = request.text.clone();
        let deadline = decided + writing::CLEANUP_LIMIT;
        let Some(settings) = self.writing.clone() else {
            return CaptureOutcome::Accepted(request);
        };
        let endpoint = match self.endpoint_by(&settings, Some(deadline)).await {
            Ok(endpoint) => endpoint,
            Err(None) => return self.cleanup_cancelled(request.route, raw).await,
            Err(Some(error)) => return self.cleanup_fell_back(request, error, decided, None),
        };
        if Instant::now() >= deadline {
            return self.cleanup_fell_back(request, WritingError::Timeout, decided, None);
        }
        self.set_state(EngineState::CleaningUp);
        let started = Instant::now();
        let mut reply = match bounded(
            &mut self.cancel,
            deadline,
            endpoint.ask_with(&self.http, writing::CLEANUP_PROMPT, &raw),
        )
        .await
        {
            Bounded::Done(Ok(reply)) => reply,
            Bounded::Cancelled => return self.cleanup_cancelled(request.route, raw).await,
            Bounded::TimedOut => {
                return self.cleanup_fell_back(
                    request,
                    WritingError::Timeout,
                    decided,
                    Some(started),
                );
            }
            Bounded::Done(Err(error)) => {
                return self.cleanup_fell_back(request, error, decided, Some(started));
            }
        };
        let mut text = String::new();
        loop {
            match bounded(&mut self.cancel, deadline, reply.next()).await {
                Bounded::Done(Ok(Some(Piece::Text(piece)))) => text.push_str(&piece),
                Bounded::Done(Ok(Some(Piece::Thinking))) => {}
                Bounded::Done(Ok(None)) => break,
                Bounded::Cancelled => return self.cleanup_cancelled(request.route, raw).await,
                Bounded::TimedOut => {
                    return self.cleanup_fell_back(
                        request,
                        WritingError::Timeout,
                        decided,
                        Some(started),
                    );
                }
                Bounded::Done(Err(error)) => {
                    return self.cleanup_fell_back(request, error, decided, Some(started));
                }
            }
        }
        let cleaned = text.trim();
        if cleaned.is_empty() {
            return self.cleanup_fell_back(request, WritingError::Empty, decided, Some(started));
        }
        self.record_cleanup("cleaned", decided, Some(started));
        self.cleanup_record = Some(("cleaned".to_owned(), raw.clone()));
        CaptureOutcome::Accepted(CaptureRequest {
            route: request.route,
            text: cleaned.to_owned(),
        })
    }

    /// A cancel during cleanup pastes nothing. A paste has already played its accepted cue, so
    /// this plays the cancelled one without waiting on the cancel watch: that change was already
    /// consumed. A submit is cleaned before its confirmation, so no accepted cue has played and
    /// none is played here.
    async fn cleanup_cancelled(&mut self, route: CaptureRoute, raw: String) -> CaptureOutcome {
        if route == CaptureRoute::Submit {
            return CaptureOutcome::cancelled_with(raw);
        }
        let started = Instant::now();
        let end = self.cue(Cue::Cancelled, Mic::Muted, started).await;
        self.cue_metric(Event::EndCue, &end);
        self.cue_failed(&end, "Nothing was pasted.");
        CaptureOutcome::cancelled_with(raw)
    }

    /// The raw words, with a notice the caller shows beside the paste.
    fn cleanup_fell_back(
        &mut self,
        request: CaptureRequest,
        error: WritingError,
        decided: Instant,
        asked: Option<Instant>,
    ) -> CaptureOutcome {
        let reason = match error {
            // The shared timeout says 15 seconds, which is the explicit-write limit.
            WritingError::Timeout => {
                format!(
                    "The cleanup took longer than {} seconds.",
                    writing::CLEANUP_LIMIT.as_secs()
                )
            }
            other => other.to_string(),
        };
        let note = format!("Cleanup failed: {reason} — pasted as said");
        (self.notice)(&note);
        self.cleanup_note = Some(note);
        self.record_cleanup(error.kind(), decided, asked);
        self.cleanup_record = Some((format!("raw: {reason}"), request.text.clone()));
        CaptureOutcome::Accepted(request)
    }

    /// `model_ms` is the chat call's own latency, absent when no call was made; `perceived_ms`
    /// is from the dictation final (`decided`) until the text to paste is known.
    fn record_cleanup(&self, outcome: &str, decided: Instant, asked: Option<Instant>) {
        let Some(settings) = &self.writing else {
            return;
        };
        self.recorder.metric(
            Event::Cleanup,
            outcome,
            asked.map(|asked| asked.elapsed()),
            json!({
                "model": settings.model(),
                "model_ms": asked.map(|asked| millis(asked.elapsed())),
                "perceived_ms": millis(decided.elapsed()),
            }),
        );
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
        let endpoint = match self.endpoint(&settings).await {
            Ok(endpoint) => endpoint,
            Err(stop) => return self.not_written(&words, stop, None),
        };
        self.set_state(EngineState::Writing {
            text: String::new(),
        });
        let started = Instant::now();
        let deadline = started + writing::LIMIT;
        let mut first = FirstPieces::default();
        let mut text = String::new();
        let prompt = settings.prompt_for(&words, self.writing_window.as_ref());
        let source = settings.prompt_source(&words, self.writing_window.as_ref());
        let mut reply = match bounded(
            &mut self.cancel,
            deadline,
            endpoint.ask_with(&self.http, prompt, &words),
        )
        .await
        .finish()
        {
            Ok(reply) => reply,
            Err(stop) => {
                return self.not_written(&words, stop, Some((started, first, Some(source))));
            }
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
                    return self.not_written(&words, stop, Some((started, first, Some(source))));
                }
            }
        }
        let written = text.trim();
        if written.is_empty() {
            return self.not_written(
                &words,
                Some(WritingError::Empty),
                Some((started, first, Some(source))),
            );
        }
        self.writing_metric("ok", started, first, Some(source));
        CaptureOutcome::Accepted(CaptureRequest {
            route: CaptureRoute::Paste,
            text: written.to_owned(),
        })
    }

    async fn endpoint<'a>(
        &mut self,
        writing: &'a Writing,
    ) -> Result<std::borrow::Cow<'a, Endpoint>, Option<WritingError>> {
        self.endpoint_by(writing, None).await
    }

    /// [`Self::endpoint`], also giving up at `deadline` with [`WritingError::Timeout`]. A cancel
    /// still wins that tie. `None` waits as long as the runtime takes to become ready.
    async fn endpoint_by<'a>(
        &mut self,
        writing: &'a Writing,
        deadline: Option<Instant>,
    ) -> Result<std::borrow::Cow<'a, Endpoint>, Option<WritingError>> {
        if let Destination::Manual(endpoint) = writing.destination() {
            return Ok(std::borrow::Cow::Borrowed(endpoint));
        }
        if matches!(writing.destination(), Destination::None) {
            return Err(Some(WritingError::NotSetUp));
        }
        let Some(chat) = self.chat.as_mut() else {
            return Err(Some(WritingError::NotInstalled));
        };
        let state = self.state.clone();
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = self.cancel.changed() => {
                self.recorder.metric(Event::ChatLoad, "cancelled", Some(started.elapsed()), json!({"build": chat.build().name()}));
                return Err(None);
            }
            () = until(deadline) => return Err(Some(WritingError::Timeout)),
            result = chat.ready(|again| { state.send_replace(EngineState::StartingChat { again }); }) => result,
        };
        match result {
            Ok(ready) => {
                if let Some(took) = ready.started {
                    self.recorder.metric(
                        Event::ChatLoad,
                        "ok",
                        Some(took),
                        json!({"build": chat.build().name()}),
                    );
                }
                Ok(std::borrow::Cow::Owned(ready.endpoint))
            }
            Err(failed) => {
                self.recorder.metric(
                    Event::ChatLoad,
                    failed.error.kind(),
                    failed.after,
                    json!({"build": chat.build().name()}),
                );
                Err(Some(failed.error))
            }
        }
    }

    fn writing_metric(
        &self,
        outcome: &str,
        started: Instant,
        first: FirstPieces,
        source: Option<writing::PromptSource>,
    ) {
        let Some(settings) = &self.writing else {
            return;
        };
        let mut fields = json!({
            "model": settings.model(),
            "first_token_ms": first.text.map(millis),
            "first_reasoning_ms": first.reasoning.map(millis),
        });
        if let Some(source) = source {
            fields["prompt_source"] = json!(source.as_str());
        }
        self.recorder
            .metric(Event::Writing, outcome, Some(started.elapsed()), fields);
    }

    /// The outcome of a writing call that gave no text: `error`, or a cancel when `None`.
    /// `call` is when the call started, what had arrived and which prompt was chosen, `None`
    /// when none was made.
    fn not_written(
        &self,
        words: &str,
        error: Option<WritingError>,
        call: Option<(Instant, FirstPieces, Option<writing::PromptSource>)>,
    ) -> CaptureOutcome {
        let outcome = error.map_or("cancelled", WritingError::kind);
        match call {
            Some((started, first, source)) => self.writing_metric(outcome, started, first, source),
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

    fn record_extra(&self, extra: &Extra, outcome: &'static str, took: Duration, used: bool) {
        self.recorder.metric(
            Event::ExtraStt,
            outcome,
            Some(took),
            json!({
                "entry": extra.name,
                "model": extra.model,
                "api": extra.api,
                "used": used,
            }),
        );
    }

    /// An extra's transcript without its ending phrase, when it ends the way the user did.
    /// A capture without endings (a hold) agrees with the whole transcript as dictated text.
    fn agreed(
        &self,
        raw: &str,
        route: CaptureRoute,
        has_endings: bool,
        require_ending: bool,
    ) -> Option<String> {
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
            FinalStep::Continue { .. } if !require_ending && !parser.scratched() => {
                return Some(parser.text()).filter(|text| !text.is_empty());
            }
            _ => return None,
        };
        // An extra can hear the spoken ending twice ("Yes. Submit. Submit."); every trailing copy is the ending.
        loop {
            let mut parser = CaptureSession::new(self.endings.clone(), self.pauses, Vec::new());
            let Ok(FinalStep::Armed(PendingEnding {
                route: ending_route,
                text: stripped,
                without_ending: false,
                ..
            })) = parser.on_final(&text, Instant::now(), false)
            else {
                break;
            };
            if ending_route != route {
                break;
            }
            text = stripped;
        }
        if require_ending {
            if let Some(before) = text
                .strip_suffix('.')
                .filter(|before| !before.ends_with('.'))
            {
                text.truncate(before.len());
            }
            has_words(&text).then_some(text)
        } else {
            (!text.is_empty()).then_some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fakes::{FakeCuePlayer, FakeFrames, FakeWake};
    use crate::hold::KeyState;
    use crate::recognizer::FakeRecognizer;

    #[test]
    fn a_task_ending_that_arms_itself_stops_stripping_when_text_is_unchanged() {
        let config = DesktopConfig::from_json_str("{}").unwrap();
        let (engine, _) = Engine::new(
            Parts {
                recognizer: FakeRecognizer::new().0,
                wake: FakeWake::new().0,
                cues: FakeCuePlayer::new().0,
                gate: InputGate::default(),
                chat: None,
                http: reqwest::Client::new(),
                hold: watch::channel(KeyState::default()).1,
                notice: Box::new(|_| {}),
                spotter: None,
                spotter_off: None,
                extras: Vec::new(),
            },
            &config,
            Recorder::disabled(),
        );
        assert_eq!(
            engine
                .agreed("open google.com do that", CaptureRoute::Task, true, false)
                .as_deref(),
            Some("open google.com"),
        );
    }

    #[tokio::test]
    async fn dictation_wakes_when_its_score_crosses() {
        let config = DesktopConfig::from_json_str("{}").expect("default config");
        let gate = InputGate::default();
        let (mut frames, feed) = FakeFrames::new(&gate);
        let (wake, control) = FakeWake::new();
        control.score_and_detect(0.8, config.wake.capture().threshold.get());
        let (recognizer, _) = FakeRecognizer::new();
        let (cues, _) = FakeCuePlayer::new();
        let (_hold_tx, hold) = watch::channel(KeyState::default());
        let (mut engine, _) = Engine::new(
            Parts {
                recognizer,
                wake,
                cues,
                gate,
                http: reqwest::Client::new(),
                chat: None,
                hold,
                notice: Box::new(|_| {}),
                spotter: None,
                spotter_off: None,
                extras: Vec::new(),
            },
            &config,
            Recorder::disabled(),
        );
        feed.send([2_000; FRAME_SAMPLES]);
        feed.fail(AudioError::Closed);

        let heard = tokio::time::timeout(Duration::from_secs(10), engine.listen(&mut frames))
            .await
            .expect("the queued frame is scored even while the hold key is idle");
        assert!(matches!(heard, Ok(WakeTrigger::Capture)));
        let outcome =
            tokio::time::timeout(Duration::from_secs(10), engine.capture(&mut frames, None))
                .await
                .expect("the closed microphone ends the capture even though no input stays quiet");
        assert!(
            matches!(outcome, CaptureOutcome::Failed { .. }),
            "{outcome:?}"
        );
    }

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
                SpeechSource::direct("Office", "ws://127.0.0.1:9".to_owned()),
                "The speech server \"Office\" at ws://127.0.0.1:9 was not ready after 8 seconds. Nothing was delivered. Check that it is running, or fix its address under Settings → Recognition.",
            ),
            (
                SpeechSource::direct("Cybrex2", "ws://100.64.0.2:8012".to_owned()),
                "The speech server \"Cybrex2\" at ws://100.64.0.2:8012 was not ready after 8 seconds. Nothing was delivered. Check that it is running, or fix its address under Settings → Recognition.",
            ),
            (
                SpeechSource::on_device("On this computer", "nemotron_160ms".to_owned()),
                "The speech model \"On this computer\" (nemotron_160ms) was not ready after 8 seconds. Nothing was delivered. Wait for it to finish loading, or pick another under Settings → Recognition.",
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

#[cfg(test)]
mod chat_tests;

//! Owns the on-device recognizer: runs a streaming speech model on a worker thread behind the recognizer seam.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{mpsc as events, watch};
use tokio::task::JoinSet;

use crate::device::{Device, Placement, Step};
use crate::frame::Frame;
use crate::metrics::{Event, Metrics};
use crate::recognizer::{
    RELEASE_LIMIT, Recognizer, RecognizerError, RecognizerEvent, RecognizerSession,
};

/// How far behind the microphone the model may fall before the capture stops.
const KEEP_UP: Duration = Duration::from_secs(3);

/// Maximum 80 ms frames drained together for a stream that coalesces queued steps (1.28 s).
const MAX_BATCH_FRAMES: usize = 16;

/// When a frame went to the model, on the runtime's clock so a paused one can move the lag.
type Sent = tokio::time::Instant;

/// The frames sent to the model and not processed yet, oldest first; the front's age is the lag.
#[derive(Default)]
struct Backlog {
    waiting: Mutex<VecDeque<Sent>>,
    /// Frames handed to the model so far.
    sent: AtomicU64,
    /// Frames the model has finished: each one's text has been produced, if it had any.
    finished: AtomicU64,
    /// Frames handed to the model when the hold's key came up; none of the flush after them counts.
    released: AtomicU64,
    /// The stream keeps the end of its audio back until it hears the input end.
    holds_text_for_end: bool,
    /// The hold's input ended, so the model still owes the text it held back for the end.
    ending: AtomicBool,
    /// The model's answer for the end of the hold's audio has been delivered.
    ended: AtomicBool,
    /// The lag this recognition stopped at, once the model fell further behind than a capture may.
    fell_behind: Mutex<Option<Duration>>,
}

impl Backlog {
    /// Stamps the next frame, or refuses it when the oldest unprocessed one is older than [`KEEP_UP`].
    fn send(&self, model: &str) -> Result<(), RecognizerError> {
        let now = Sent::now();
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(oldest) = waiting.front() {
            let lag = now.saturating_duration_since(*oldest);
            if lag > KEEP_UP {
                *self
                    .fell_behind
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(lag);
                return Err(RecognizerError::Behind {
                    model: model.to_owned(),
                    lag,
                });
            }
        }
        waiting.push_back(now);
        self.sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The model finished the oldest frame it had not processed.
    #[cfg(test)]
    fn took(&self) {
        self.took_many(1);
    }

    /// The model finished the oldest `count` frames it had not processed.
    fn took_many(&self, count: usize) {
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        let mut done = 0_u64;
        for _ in 0..count {
            if waiting.pop_front().is_some() {
                done += 1;
            }
        }
        if done > 0 {
            self.finished.fetch_add(done, Ordering::Relaxed);
        }
    }

    /// The hold's key came up: the frames sent so far are its last, before its release flush.
    fn hold_released(&self) {
        self.released
            .store(self.sent.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// The release flush was sent: a stream that holds text back still owes it for the end.
    fn input_ended(&self) {
        self.ending.store(true, Ordering::Relaxed);
    }

    /// The model's answer for the end of its input reached the capture.
    fn ended(&self) {
        self.ended.store(true, Ordering::Relaxed);
    }

    /// Whether the model still owes a result for the hold's audio: a frame sent before its release
    /// not finished, or the text a stream holds back until it has heard the end.
    fn behind(&self) -> bool {
        self.finished.load(Ordering::Relaxed) < self.released.load(Ordering::Relaxed)
            || (self.holds_text_for_end
                && self.ending.load(Ordering::Relaxed)
                && !self.ended.load(Ordering::Relaxed))
    }

    /// The lag this recognition stopped at, once the model fell further behind than a capture may.
    fn lag_stop(&self) -> Option<Duration> {
        *self
            .fell_behind
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A speech model that can start independent recognition streams.
pub trait SpeechModel: Send + Sync + 'static {
    type Stream: SpeechStream;

    /// Where the model runs.
    fn device(&self) -> Device;

    /// Where the GPU put the nodes of each network of the model; none on the CPU.
    fn placement(&self) -> Option<Placement> {
        None
    }

    /// How long a final may take after the release: a decoder that re-hears the end of the stretch
    /// takes longer than the release flush.
    fn release_limit(&self) -> Duration {
        RELEASE_LIMIT
    }

    /// A stream with fresh state; the model's weights are shared.
    fn stream(&self) -> Self::Stream;
}

/// One recognition in progress.
pub trait SpeechStream: Send + 'static {
    /// Takes the next 80 ms frame and returns the text it produced, if any. A `Partial` replaces
    /// the previous one; a `Final` ends a stretch of speech and the next text starts afresh.
    ///
    /// # Errors
    /// Fails when the model cannot continue; the stream is then abandoned.
    fn accept(&mut self, frame: &Frame) -> Result<Vec<RecognizerEvent>, RecognizerError>;

    /// Takes one or more consecutive 80 ms frames in order. Only streams whose
    /// [`batches_frames`](Self::batches_frames) is true receive more than one frame at once.
    ///
    /// # Errors
    /// Fails when the model cannot continue; the stream is then abandoned.
    fn accept_batch(&mut self, frames: &[Frame]) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        let mut events = Vec::new();
        for frame in frames {
            events.extend(self.accept(frame)?);
        }
        Ok(events)
    }

    /// Whether queued frames can be handed to [`accept_batch`](Self::accept_batch) together so a
    /// sliding-window encoder catches up in one pass instead of one per step.
    fn batches_frames(&self) -> bool {
        false
    }

    /// The input is complete: no more frames will come. Answers with the text the model held back
    /// for the end of the audio, if any.
    ///
    /// # Errors
    /// Fails when the model cannot continue; the stream is then abandoned.
    fn end_input(&mut self) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        Ok(Vec::new())
    }

    /// Whether the stream keeps the end of its audio back until it is told the input ended, so its
    /// partials alone would drop the last words.
    fn holds_text_for_end(&self) -> bool {
        false
    }
}

/// Where a speech model comes from; loading may download it, and is retried until it works.
pub trait ModelSource: Send + Sync + 'static {
    type Model: SpeechModel;

    /// Plans where the model runs and loads it there, telling `step` what it is doing.
    ///
    /// # Errors
    /// Fails in plain words when the model cannot be fetched or loaded.
    fn load(
        &self,
        step: impl Fn(Step) + Send + Sync,
    ) -> impl Future<Output = Result<Self::Model, RecognizerError>> + Send;
}

/// The model a recognizer has loaded, if it still has one.
type Loaded<M> = Arc<Mutex<Option<Arc<M>>>>;

/// How the load in progress ended, once it has.
type Outcome<M> = Option<Result<Arc<M>, RecognizerError>>;

/// Recognizes speech on this machine, one recognition at a time.
pub struct LocalRecognizer<S: ModelSource> {
    shared: Arc<Shared<S>>,
    /// The loads under way, which go on when the dictation that asked for one is gone and stop with the recognizer.
    loads: Mutex<JoinSet<()>>,
}

struct Shared<S: ModelSource> {
    source: S,
    /// Emptied when a recognition on the model fails, so the next one plans and loads again.
    model: Loaded<S::Model>,
    /// The load in progress, which every caller that needs the model waits for, so all of them get its outcome.
    running: Mutex<Option<watch::Receiver<Outcome<S::Model>>>>,
    steps: watch::Sender<Step>,
    name: String,
    /// How the user knows the model, for the notice when it falls behind; `name` labels metrics.
    title: String,
    metrics: Metrics,
}

impl<S: ModelSource> LocalRecognizer<S> {
    /// `name` labels the model in metrics; `title` is how the user knows it.
    #[must_use]
    pub fn new(
        source: S,
        name: impl Into<String>,
        title: impl Into<String>,
        metrics: Metrics,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                source,
                model: Arc::default(),
                running: Mutex::new(None),
                steps: watch::channel(Step::Reading).0,
                name: name.into(),
                title: title.into(),
                metrics,
            }),
            loads: Mutex::new(JoinSet::new()),
        }
    }

    #[must_use]
    pub fn source(&self) -> &S {
        &self.shared.source
    }

    /// The name that labels the model in metrics.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// What the load in progress is doing; [`Step::Reading`] while none runs.
    #[must_use]
    pub fn steps(&self) -> watch::Receiver<Step> {
        self.shared.steps.subscribe()
    }

    /// Where the model runs now, or `None` when it is not loaded: not yet, failed to load, or lost to an error.
    #[must_use]
    pub fn device(&self) -> Option<Device> {
        self.shared.loaded().map(|model| model.device())
    }

    /// Fetches and loads the model now instead of at the first dictation; later calls are free.
    ///
    /// # Errors
    /// Fails in plain words when the model cannot be fetched or loaded; the next call tries again.
    pub async fn load(&self) -> Result<(), RecognizerError> {
        self.model().await.map(|_| ())
    }

    /// The model, loaded by a task of its own: a caller that stops waiting leaves the load running.
    async fn model(&self) -> Result<Arc<S::Model>, RecognizerError> {
        if let Some(model) = self.shared.loaded() {
            return Ok(model);
        }
        let mut outcome = {
            let mut running = self
                .shared
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            running
                .get_or_insert_with(|| {
                    let (done, outcome) = watch::channel(None);
                    let shared = Arc::clone(&self.shared);
                    self.loads
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .spawn(async move {
                            let loaded = shared.load().await;
                            shared
                                .running
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .take();
                            done.send_replace(Some(loaded));
                        });
                    outcome
                })
                .clone()
        };
        let ended = outcome
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|ended| ended.clone());
        ended.unwrap_or_else(|| {
            Err(RecognizerError::said(
                "Loading the speech model stopped unexpectedly.",
            ))
        })
    }
}

impl<S: ModelSource> Shared<S> {
    fn loaded(&self) -> Option<Arc<S::Model>> {
        self.model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn tell(&self, step: Step) {
        self.steps
            .send_if_modified(|current| std::mem::replace(current, step) != step);
    }

    async fn load(&self) -> Result<Arc<S::Model>, RecognizerError> {
        if let Some(model) = self.loaded() {
            return Ok(model);
        }
        let started = Instant::now();
        self.tell(Step::Reading);
        let loaded = self.source.load(|step| self.tell(step)).await;
        let fields = json!({"model": self.name});
        self.metrics.metric(
            Event::SpeechLoad,
            if loaded.is_ok() { "ok" } else { "error" },
            Some(started.elapsed()),
            match &loaded {
                Ok(model) => {
                    let fields = model.device().tag(fields);
                    model
                        .placement()
                        .map_or(fields.clone(), |placement| placement.tag(fields))
                }
                Err(_) => fields,
            },
        );
        if loaded.is_err() {
            self.tell(Step::Reading);
        }
        let model = Arc::new(loaded?);
        *self.model.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&model));
        Ok(model)
    }
}

impl<S: ModelSource> Recognizer for LocalRecognizer<S> {
    type Session = LocalSession;

    async fn connect(&self) -> Result<LocalSession, RecognizerError> {
        let model = self.model().await?;
        let release_limit = model.release_limit();
        let (frames_tx, frames_rx) = mpsc::channel();
        let (events_tx, events_rx) = events::unbounded_channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stream = model.stream();
        let backlog = Arc::new(Backlog {
            holds_text_for_end: stream.holds_text_for_end(),
            ..Backlog::default()
        });
        let cache = Arc::clone(&self.shared.model);
        let work = Work {
            stream,
            device: model.device(),
            frames: frames_rx,
            events: events_tx,
            cancelled: Arc::clone(&cancelled),
            backlog: Arc::clone(&backlog),
            name: self.shared.name.clone(),
            metrics: self.shared.metrics.clone(),
            forget: Box::new(move || {
                *cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }),
        };
        std::thread::Builder::new()
            .name("thalvi-local-stt".to_owned())
            .spawn(move || work.run())
            .map_err(|_| RecognizerError::said("Speech recognition could not start."))?;
        Ok(LocalSession {
            frames: frames_tx,
            events: events_rx,
            cancelled,
            release_limit,
            backlog,
            model: self.shared.title.clone(),
        })
    }

    fn loading(&self) -> Option<watch::Receiver<Step>> {
        Some(self.steps())
    }
}

/// What the worker takes next: a frame, or the end of the hold's input.
enum Order {
    Frame(Instant, Box<Frame>),
    End,
}

/// An open local recognition.
pub struct LocalSession {
    frames: mpsc::Sender<Order>,
    events: events::UnboundedReceiver<Result<RecognizerEvent, RecognizerError>>,
    cancelled: Arc<AtomicBool>,
    release_limit: Duration,
    backlog: Arc<Backlog>,
    /// The model's name, for the notice when it falls behind.
    model: String,
}

fn stopped() -> RecognizerError {
    RecognizerError::said("Speech recognition stopped unexpectedly. Nothing was delivered.")
}

impl RecognizerSession for LocalSession {
    fn release_limit(&self) -> Duration {
        self.release_limit
    }

    fn behind(&self) -> bool {
        self.backlog.behind()
    }

    fn needs_release_silence(&self) -> bool {
        false
    }

    fn hold_released(&mut self) {
        self.backlog.hold_released();
    }

    fn input_ended(&mut self) {
        self.backlog.input_ended();
        let _ = self.frames.send(Order::End);
    }

    fn send_frame(
        &mut self,
        frame: &Frame,
    ) -> impl Future<Output = Result<(), RecognizerError>> + Send {
        let sent = self.backlog.send(&self.model).and_then(|()| {
            self.frames
                .send(Order::Frame(Instant::now(), Box::new(*frame)))
                .map_err(|_| stopped())
        });
        std::future::ready(sent)
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        let event = self.events.recv().await.unwrap_or_else(|| Err(stopped()));
        if matches!(&event, Ok(RecognizerEvent::Ended)) {
            self.backlog.ended();
        }
        event
    }

    fn cancel(self) -> impl Future<Output = ()> + Send {
        self.cancelled.store(true, Ordering::Relaxed);
        std::future::ready(())
    }

    fn close(self) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }
}

struct Work<S> {
    stream: S,
    device: Device,
    frames: mpsc::Receiver<Order>,
    events: events::UnboundedSender<Result<RecognizerEvent, RecognizerError>>,
    cancelled: Arc<AtomicBool>,
    backlog: Arc<Backlog>,
    name: String,
    metrics: Metrics,
    /// Drops the model from its recognizer; a model whose recognition failed may have lost its device.
    forget: Box<dyn Fn() + Send>,
}

/// What one recognition cost, for the opt-in metrics.
#[derive(Default)]
struct Tally {
    frames: u32,
    model: Duration,
    slowest: Duration,
    /// Longest wait between a frame being sent and the text it led to; model latency plus queueing.
    late: Duration,
}

impl<S: SpeechStream> Work<S> {
    fn end_input(&mut self) -> bool {
        match self.stream.end_input() {
            Ok(texts) => {
                for text in texts {
                    let _ = self.events.send(Ok(text));
                }
                let _ = self.events.send(Ok(RecognizerEvent::Ended));
                true
            }
            Err(error) => {
                (self.forget)();
                let _ = self.events.send(Err(error));
                false
            }
        }
    }

    fn run(mut self) {
        let mut tally = Tally::default();
        let mut outcome = "ok";
        let batches = self.stream.batches_frames();
        while let Ok(order) = self.frames.recv() {
            if self.cancelled.load(Ordering::Relaxed) {
                outcome = "cancelled";
                break;
            }
            let Order::Frame(sent, frame) = order else {
                if !self.end_input() {
                    outcome = "error";
                    break;
                }
                continue;
            };
            let mut batch = vec![*frame];
            let mut saw_end = false;
            if batches {
                while batch.len() < MAX_BATCH_FRAMES {
                    match self.frames.try_recv() {
                        Ok(Order::Frame(_, next)) => batch.push(*next),
                        Ok(Order::End) => {
                            saw_end = true;
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
            let started = Instant::now();
            let result = if let [only] = batch.as_slice() {
                self.stream.accept(only)
            } else {
                self.stream.accept_batch(&batch)
            };
            let took = started.elapsed();
            let count = batch.len();
            self.backlog.took_many(count);
            tally.frames += u32::try_from(count).unwrap_or(u32::MAX);
            tally.model += took;
            tally.slowest = tally.slowest.max(took);
            match result {
                Ok(texts) => {
                    if !texts.is_empty() {
                        tally.late = tally.late.max(sent.elapsed());
                    }
                    for text in texts {
                        let _ = self.events.send(Ok(text));
                    }
                }
                Err(error) => {
                    (self.forget)();
                    let _ = self.events.send(Err(error));
                    outcome = "error";
                    break;
                }
            }
            if saw_end {
                if self.cancelled.load(Ordering::Relaxed) {
                    outcome = "cancelled";
                    break;
                }
                if !self.end_input() {
                    outcome = "error";
                    break;
                }
            }
        }
        if outcome == "ok" && self.cancelled.load(Ordering::Relaxed) {
            outcome = "cancelled";
        }
        let behind = self.backlog.lag_stop();
        if behind.is_some() {
            outcome = "cant_keep_up";
        }
        self.report(&tally, outcome, behind);
    }

    fn report(&self, tally: &Tally, outcome: &str, behind: Option<Duration>) {
        let mean = tally
            .model
            .checked_div(tally.frames)
            .unwrap_or(Duration::ZERO);
        let mut fields = json!({
            "engine": "local",
            "model": self.name,
            "frames": tally.frames,
            "model_mean_ms": mean.as_secs_f64() * 1000.0,
            "model_max_ms": tally.slowest.as_secs_f64() * 1000.0,
            "text_late_max_ms": tally.late.as_secs_f64() * 1000.0,
        });
        if let Some(lag) = behind {
            fields["lag_ms"] = json!(lag.as_secs_f64() * 1000.0);
        }
        self.metrics.metric(
            Event::LocalStt,
            outcome,
            Some(tally.model),
            self.device.tag(fields),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hold_is_behind_until_the_stream_that_holds_text_back_answers_the_end_of_its_audio() {
        let backlog = Backlog {
            holds_text_for_end: true,
            ..Backlog::default()
        };
        backlog.send("model").unwrap();
        backlog.send("model").unwrap();
        backlog.hold_released();
        assert!(backlog.behind(), "both frames are still unanswered");
        backlog.took();
        assert!(backlog.behind(), "one of the two is still unanswered");
        backlog.took();
        assert!(
            !backlog.behind(),
            "every frame from before the release has its result"
        );

        backlog.send("model").unwrap();
        backlog.input_ended();
        assert!(
            backlog.behind(),
            "the stream has not answered the end of its audio yet"
        );
        backlog.took();
        assert!(
            backlog.behind(),
            "the model still owes the text it held back for the end"
        );
        backlog.ended();
        assert!(
            !backlog.behind(),
            "the model has answered the end of the hold's audio"
        );
    }

    #[test]
    fn a_stream_that_emits_its_words_as_it_hears_them_is_caught_up_at_release() {
        let backlog = Backlog::default();
        backlog.send("model").unwrap();
        backlog.took();
        backlog.hold_released();
        backlog.input_ended();
        assert!(
            !backlog.behind(),
            "a stream whose partials carry the last words does not owe a result for the flush"
        );
    }
}

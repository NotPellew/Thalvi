//! Owns the on-device recognizer: runs a streaming speech model on a worker thread behind the recognizer seam.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::OnceCell;
use tokio::sync::mpsc as events;

use crate::frame::Frame;
use crate::metrics::{Event, Metrics};
use crate::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerSession};

/// A speech model that can start independent recognition streams.
pub trait SpeechModel: Send + Sync + 'static {
    type Stream: SpeechStream;

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
}

/// Where a speech model comes from; loading may download it, and is retried until it works.
pub trait ModelSource: Send + Sync + 'static {
    type Model: SpeechModel;

    /// # Errors
    /// Fails in plain words when the model cannot be fetched or loaded.
    fn load(&self) -> impl Future<Output = Result<Self::Model, RecognizerError>> + Send;
}

/// Recognizes speech on this machine, one recognition at a time.
pub struct LocalRecognizer<S: ModelSource> {
    source: S,
    model: OnceCell<Arc<S::Model>>,
    name: String,
    metrics: Metrics,
}

impl<S: ModelSource> LocalRecognizer<S> {
    /// `name` labels the model in metrics.
    #[must_use]
    pub fn new(source: S, name: impl Into<String>, metrics: Metrics) -> Self {
        Self {
            source,
            model: OnceCell::new(),
            name: name.into(),
            metrics,
        }
    }

    /// Fetches and loads the model now instead of at the first dictation; later calls are free.
    ///
    /// # Errors
    /// Fails in plain words when the model cannot be fetched or loaded; the next call tries again.
    pub async fn load(&self) -> Result<(), RecognizerError> {
        self.model().await.map(|_| ())
    }

    async fn model(&self) -> Result<&Arc<S::Model>, RecognizerError> {
        self.model
            .get_or_try_init(|| async {
                let started = Instant::now();
                let loaded = self.source.load().await;
                self.metrics.metric(
                    Event::SpeechLoad,
                    if loaded.is_ok() { "ok" } else { "error" },
                    Some(started.elapsed()),
                    json!({"model": self.name}),
                );
                loaded.map(Arc::new)
            })
            .await
    }
}

impl<S: ModelSource> Recognizer for LocalRecognizer<S> {
    type Session = LocalSession;

    async fn connect(&self) -> Result<LocalSession, RecognizerError> {
        let model = self.model().await?;
        let (frames_tx, frames_rx) = mpsc::channel();
        let (events_tx, events_rx) = events::unbounded_channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let work = Work {
            stream: model.stream(),
            frames: frames_rx,
            events: events_tx,
            cancelled: Arc::clone(&cancelled),
            name: self.name.clone(),
            metrics: self.metrics.clone(),
        };
        std::thread::Builder::new()
            .name("thalvi-local-stt".to_owned())
            .spawn(move || work.run())
            .map_err(|_| RecognizerError::new("Speech recognition could not start."))?;
        Ok(LocalSession {
            frames: frames_tx,
            events: events_rx,
            cancelled,
        })
    }
}

/// An open local recognition.
pub struct LocalSession {
    frames: mpsc::Sender<(Instant, Frame)>,
    events: events::UnboundedReceiver<Result<RecognizerEvent, RecognizerError>>,
    cancelled: Arc<AtomicBool>,
}

fn stopped() -> RecognizerError {
    RecognizerError::new("Speech recognition stopped unexpectedly. Nothing was delivered.")
}

impl RecognizerSession for LocalSession {
    fn send_frame(
        &mut self,
        frame: &Frame,
    ) -> impl Future<Output = Result<(), RecognizerError>> + Send {
        std::future::ready(
            self.frames
                .send((Instant::now(), *frame))
                .map_err(|_| stopped()),
        )
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        self.events.recv().await.unwrap_or_else(|| Err(stopped()))
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
    frames: mpsc::Receiver<(Instant, Frame)>,
    events: events::UnboundedSender<Result<RecognizerEvent, RecognizerError>>,
    cancelled: Arc<AtomicBool>,
    name: String,
    metrics: Metrics,
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
    fn run(mut self) {
        let mut tally = Tally::default();
        let mut outcome = "ok";
        while let Ok((sent, frame)) = self.frames.recv() {
            if self.cancelled.load(Ordering::Relaxed) {
                outcome = "cancelled";
                break;
            }
            let started = Instant::now();
            let result = self.stream.accept(&frame);
            let took = started.elapsed();
            tally.frames += 1;
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
                    let _ = self.events.send(Err(error));
                    outcome = "error";
                    break;
                }
            }
        }
        if outcome == "ok" && self.cancelled.load(Ordering::Relaxed) {
            outcome = "cancelled";
        }
        self.report(&tally, outcome);
    }

    fn report(&self, tally: &Tally, outcome: &str) {
        let mean = tally
            .model
            .checked_div(tally.frames)
            .unwrap_or(Duration::ZERO);
        self.metrics.metric(
            Event::LocalStt,
            outcome,
            Some(tally.model),
            json!({
                "engine": "local",
                "device": "cpu",
                "model": self.name,
                "frames": tally.frames,
                "model_mean_ms": mean.as_secs_f64() * 1000.0,
                "model_max_ms": tally.slowest.as_secs_f64() * 1000.0,
                "text_late_max_ms": tally.late.as_secs_f64() * 1000.0,
            }),
        );
    }
}

//! Owns the choice of recognizer that the active live entry makes.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::device::{Adapters, Device, Step};
use crate::entries::{LiveEntry, LiveKind};
use crate::frame::Frame;
use crate::local::{LocalRecognizer, LocalSession, ModelSource};
use crate::metrics::Metrics;
use crate::models::ModelStore;
use crate::openai_live::{OpenAiRecognizer, OpenAiSession};
use crate::phrases::WordList;
use crate::realtime::{RealtimeRecognizer, RealtimeSession};
use crate::recognizer::{
    Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, SpeechSource,
};
use crate::transducer::{SpeechChoice, TransducerSource};
use crate::whisper::{WhisperRecognizer, WhisperSession};

/// A recognizer whose model loads on this computer, which can be loaded ahead of the first dictation.
pub trait Preload: Send + Sync {
    /// The name that labels the model in metrics.
    fn name(&self) -> &str;

    /// What the load in progress is doing; [`Step::Reading`] while none runs.
    fn steps(&self) -> watch::Receiver<Step>;

    /// Where the model runs now, or `None` when it is not loaded.
    fn device(&self) -> Option<Device>;

    /// Fetches and loads the model now instead of at the first dictation; later calls are free.
    fn load(&self) -> Pin<Box<dyn Future<Output = Result<(), RecognizerError>> + Send + '_>>;
}

impl<P: Preload + ?Sized> Preload for Arc<P> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn steps(&self) -> watch::Receiver<Step> {
        (**self).steps()
    }

    fn device(&self) -> Option<Device> {
        (**self).device()
    }

    fn load(&self) -> Pin<Box<dyn Future<Output = Result<(), RecognizerError>> + Send + '_>> {
        (**self).load()
    }
}

impl<S: ModelSource> Preload for LocalRecognizer<S> {
    fn name(&self) -> &str {
        Self::name(self)
    }

    fn steps(&self) -> watch::Receiver<Step> {
        Self::steps(self)
    }

    fn device(&self) -> Option<Device> {
        Self::device(self)
    }

    fn load(&self) -> Pin<Box<dyn Future<Output = Result<(), RecognizerError>> + Send + '_>> {
        Box::pin(Self::load(self))
    }
}

/// The recognizer a config asks for.
pub enum Chosen {
    Realtime(RealtimeRecognizer),
    OpenAi(OpenAiRecognizer),
    Local {
        recognizer: Arc<LocalRecognizer<TransducerSource>>,
        speech: SpeechSource,
    },
    Whisper {
        recognizer: Arc<WhisperRecognizer>,
        speech: SpeechSource,
    },
}

/// An open recognition of any kind.
pub enum ChosenSession {
    Realtime(Box<RealtimeSession>),
    OpenAi(Box<OpenAiSession>),
    Local(LocalSession, SpeechSource),
    Whisper(Box<WhisperSession>, SpeechSource),
}

impl Chosen {
    /// The recognizer for `entry`; nothing is fetched or loaded until [`Self::local`]'s `load`
    /// or the first dictation. `adapters` keeps what the load's check of the graphics adapter finds;
    /// `helpers` holds the `cpu` and `vulkan` folders of the Whisper helpers that the build packages.
    #[must_use]
    pub fn new(
        entry: &LiveEntry,
        store: ModelStore,
        http: reqwest::Client,
        metrics: Metrics,
        phrases: &WordList,
        adapters: Adapters,
        helpers: &Path,
    ) -> Self {
        match &entry.kind {
            LiveKind::Realtime(realtime) => {
                Self::Realtime(RealtimeRecognizer::new(&entry.name, realtime, phrases))
            }
            LiveKind::OpenAi(live) => Self::OpenAi(OpenAiRecognizer::new(
                &entry.name,
                live.clone(),
                http,
                metrics,
                phrases,
            )),
            LiveKind::OnDevice(SpeechChoice::Whisper(backend)) => Self::Whisper {
                recognizer: Arc::new(WhisperRecognizer::new(
                    *backend, helpers, store, metrics, phrases,
                )),
                speech: entry.source(),
            },
            LiveKind::OnDevice(choice) => Self::Local {
                recognizer: Arc::new(LocalRecognizer::new(
                    TransducerSource::new(store, choice.clone(), phrases, adapters),
                    choice.label(),
                    choice.title(),
                    metrics,
                )),
                speech: entry.source(),
            },
        }
    }

    /// The recognizer that loads a model on this computer, to load it ahead of the first dictation.
    #[must_use]
    pub fn local(&self) -> Option<Arc<dyn Preload>> {
        match self {
            Self::Local { recognizer, .. } => Some(Arc::clone(recognizer) as Arc<dyn Preload>),
            Self::Whisper { recognizer, .. } => Some(Arc::clone(recognizer) as Arc<dyn Preload>),
            Self::Realtime(_) | Self::OpenAi(_) => None,
        }
    }
}

/// The local recognizer words its failures itself; the entry's name goes with them.
fn named(speech: &SpeechSource, error: RecognizerError) -> RecognizerError {
    match error {
        RecognizerError::Said(message) => RecognizerError::Reported {
            speech: speech.clone(),
            message,
        },
        error => error,
    }
}

impl Recognizer for Chosen {
    type Session = ChosenSession;

    async fn connect(&self) -> Result<ChosenSession, RecognizerError> {
        match self {
            Self::Realtime(realtime) => realtime
                .connect()
                .await
                .map(|session| ChosenSession::Realtime(Box::new(session))),
            Self::OpenAi(recognizer) => recognizer
                .connect()
                .await
                .map(|session| ChosenSession::OpenAi(Box::new(session))),
            Self::Local { recognizer, speech } => recognizer
                .connect()
                .await
                .map(|session| ChosenSession::Local(session, speech.clone()))
                .map_err(|error| named(speech, error)),
            Self::Whisper { recognizer, speech } => recognizer
                .connect()
                .await
                .map(|session| ChosenSession::Whisper(Box::new(session), speech.clone()))
                .map_err(|error| named(speech, error)),
        }
    }

    fn loading(&self) -> Option<watch::Receiver<Step>> {
        self.local().map(|local| local.steps())
    }
}

impl RecognizerSession for ChosenSession {
    fn buffers_audio(&self) -> bool {
        matches!(self, Self::OpenAi(_) | Self::Whisper(..))
    }

    fn final_wait(&self) -> Duration {
        match self {
            Self::Realtime(session) => session.final_wait(),
            Self::OpenAi(session) => session.final_wait(),
            Self::Local(session, _) => session.final_wait(),
            Self::Whisper(session, _) => session.final_wait(),
        }
    }

    fn release_limit(&self) -> Duration {
        match self {
            Self::Realtime(session) => session.release_limit(),
            Self::OpenAi(session) => session.release_limit(),
            Self::Local(session, _) => session.release_limit(),
            Self::Whisper(session, _) => session.release_limit(),
        }
    }

    fn behind(&self) -> bool {
        match self {
            Self::Realtime(session) => session.behind(),
            Self::OpenAi(session) => session.behind(),
            Self::Local(session, _) => session.behind(),
            Self::Whisper(session, _) => session.behind(),
        }
    }

    async fn finish_audio(&mut self) -> Result<(), RecognizerError> {
        match self {
            Self::OpenAi(session) => session.finish_audio().await,
            Self::Whisper(session, speech) => session
                .finish_audio()
                .await
                .map_err(|error| named(speech, error)),
            Self::Realtime(_) | Self::Local(..) => Ok(()),
        }
    }

    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        match self {
            Self::Realtime(session) => session.send_frame(frame).await,
            Self::OpenAi(session) => session.send_frame(frame).await,
            Self::Local(session, speech) => session
                .send_frame(frame)
                .await
                .map_err(|error| named(speech, error)),
            Self::Whisper(session, speech) => session
                .send_frame(frame)
                .await
                .map_err(|error| named(speech, error)),
        }
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        match self {
            Self::Realtime(session) => session.next_event().await,
            Self::OpenAi(session) => session.next_event().await,
            Self::Local(session, speech) => session
                .next_event()
                .await
                .map_err(|error| named(speech, error)),
            Self::Whisper(session, speech) => session
                .next_event()
                .await
                .map_err(|error| named(speech, error)),
        }
    }

    async fn cancel(self) {
        match self {
            Self::Realtime(session) => session.cancel().await,
            Self::OpenAi(session) => session.cancel().await,
            Self::Local(session, _) => session.cancel().await,
            Self::Whisper(session, _) => session.cancel().await,
        }
    }

    async fn close(self) {
        match self {
            Self::Realtime(session) => session.close().await,
            Self::OpenAi(session) => session.close().await,
            Self::Local(session, _) => session.close().await,
            Self::Whisper(session, _) => session.close().await,
        }
    }
}

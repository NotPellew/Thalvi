//! Owns the choice of recognizer that the active live entry makes.

use std::sync::Arc;

use crate::device::Adapters;
use crate::entries::{LiveEntry, LiveKind};
use crate::frame::Frame;
use crate::local::{LocalRecognizer, LocalSession};
use crate::metrics::Metrics;
use crate::models::ModelStore;
use crate::openai_live::{OpenAiRecognizer, OpenAiSession};
use crate::phrases::WordList;
use crate::realtime::{RealtimeRecognizer, RealtimeSession};
use crate::recognizer::{
    Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, SpeechSource,
};
use crate::transducer::TransducerSource;

/// The recognizer a config asks for.
pub enum Chosen {
    Realtime(RealtimeRecognizer),
    OpenAi(OpenAiRecognizer),
    Local {
        recognizer: Arc<LocalRecognizer<TransducerSource>>,
        speech: SpeechSource,
    },
}

/// An open recognition of any kind.
pub enum ChosenSession {
    Realtime(Box<RealtimeSession>),
    OpenAi(OpenAiSession),
    Local(LocalSession, SpeechSource),
}

impl Chosen {
    /// The recognizer for `entry`; nothing is fetched or loaded until [`Self::local`]'s `load`
    /// or the first dictation. `adapters` keeps what the load's check of the graphics adapter finds.
    #[must_use]
    pub fn new(
        entry: &LiveEntry,
        store: ModelStore,
        http: reqwest::Client,
        metrics: Metrics,
        phrases: &WordList,
        adapters: Adapters,
    ) -> Self {
        match &entry.kind {
            LiveKind::Realtime(realtime) => {
                Self::Realtime(RealtimeRecognizer::new(&entry.name, realtime, phrases))
            }
            LiveKind::OpenAi(settings) => Self::OpenAi(OpenAiRecognizer::new(
                &entry.name,
                settings.clone(),
                http,
                metrics.clone(),
            )),
            LiveKind::OnDevice(choice) => Self::Local {
                recognizer: Arc::new(LocalRecognizer::new(
                    TransducerSource::new(store, choice.clone(), phrases, adapters),
                    choice.label(),
                    metrics,
                )),
                speech: entry.source(),
            },
        }
    }

    /// The local recognizer, to load its model ahead of the first dictation.
    #[must_use]
    pub fn local(&self) -> Option<Arc<LocalRecognizer<TransducerSource>>> {
        match self {
            Self::Local { recognizer, .. } => Some(Arc::clone(recognizer)),
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
            Self::OpenAi(recognizer) => recognizer.connect().await.map(ChosenSession::OpenAi),
            Self::Local { recognizer, speech } => recognizer
                .connect()
                .await
                .map(|session| ChosenSession::Local(session, speech.clone()))
                .map_err(|error| named(speech, error)),
        }
    }
}

impl RecognizerSession for ChosenSession {
    fn buffers_audio(&self) -> bool {
        matches!(self, Self::OpenAi(_))
    }

    async fn finish_audio(&mut self) -> Result<(), RecognizerError> {
        match self {
            Self::OpenAi(session) => session.finish_audio().await,
            _ => Ok(()),
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
        }
    }

    async fn cancel(self) {
        match self {
            Self::Realtime(session) => session.cancel().await,
            Self::OpenAi(session) => session.cancel().await,
            Self::Local(session, _) => session.cancel().await,
        }
    }

    async fn close(self) {
        match self {
            Self::Realtime(session) => session.close().await,
            Self::OpenAi(session) => session.close().await,
            Self::Local(session, _) => session.close().await,
        }
    }
}

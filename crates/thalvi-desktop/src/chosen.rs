//! Owns the choice of recognizer that the active live entry makes.

use std::sync::Arc;

use crate::device::Adapters;
use crate::entries::{LiveEntry, LiveKind};
use crate::frame::Frame;
use crate::local::{LocalRecognizer, LocalSession};
use crate::metrics::Metrics;
use crate::models::ModelStore;
use crate::phrases::WordList;
use crate::realtime::{RealtimeRecognizer, RealtimeSession};
use crate::recognizer::{
    Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, ServerRecognizer,
    ServerSession, SpeechSource,
};
use crate::transducer::TransducerSource;

/// The recognizer a config asks for.
pub enum Chosen {
    Server(ServerRecognizer),
    Realtime(RealtimeRecognizer),
    Local {
        recognizer: Arc<LocalRecognizer<TransducerSource>>,
        speech: SpeechSource,
    },
}

/// An open recognition of any kind.
pub enum ChosenSession {
    Server(Box<ServerSession>),
    Realtime(Box<RealtimeSession>),
    Local(LocalSession, SpeechSource),
}

impl Chosen {
    /// The recognizer for `entry`; nothing is fetched or loaded until [`Self::local`]'s `load`
    /// or the first dictation. `adapters` keeps what the load's check of the graphics adapter finds.
    #[must_use]
    pub fn new(
        entry: &LiveEntry,
        store: ModelStore,
        metrics: Metrics,
        phrases: &WordList,
        adapters: Adapters,
    ) -> Self {
        match &entry.kind {
            LiveKind::ThalviServer(server) => {
                Self::Server(ServerRecognizer::new(&entry.name, server.clone()))
            }
            LiveKind::Realtime(realtime) => {
                Self::Realtime(RealtimeRecognizer::new(&entry.name, realtime, phrases))
            }
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
            Self::Server(_) | Self::Realtime(_) => None,
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
            Self::Server(server) => server
                .connect()
                .await
                .map(|session| ChosenSession::Server(Box::new(session))),
            Self::Realtime(realtime) => realtime
                .connect()
                .await
                .map(|session| ChosenSession::Realtime(Box::new(session))),
            Self::Local { recognizer, speech } => recognizer
                .connect()
                .await
                .map(|session| ChosenSession::Local(session, speech.clone()))
                .map_err(|error| named(speech, error)),
        }
    }
}

impl RecognizerSession for ChosenSession {
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        match self {
            Self::Server(session) => session.send_frame(frame).await,
            Self::Realtime(session) => session.send_frame(frame).await,
            Self::Local(session, speech) => session
                .send_frame(frame)
                .await
                .map_err(|error| named(speech, error)),
        }
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        match self {
            Self::Server(session) => session.next_event().await,
            Self::Realtime(session) => session.next_event().await,
            Self::Local(session, speech) => session
                .next_event()
                .await
                .map_err(|error| named(speech, error)),
        }
    }

    async fn cancel(self) {
        match self {
            Self::Server(session) => session.cancel().await,
            Self::Realtime(session) => session.cancel().await,
            Self::Local(session, _) => session.cancel().await,
        }
    }

    async fn close(self) {
        match self {
            Self::Server(session) => session.close().await,
            Self::Realtime(session) => session.close().await,
            Self::Local(session, _) => session.close().await,
        }
    }
}

//! Owns the choice between the server and the local recognizer that `stt.engine` makes.

use std::sync::Arc;

use crate::config::SttEngine;
use crate::frame::Frame;
use crate::local::{LocalRecognizer, LocalSession};
use crate::metrics::Metrics;
use crate::models::ModelStore;
use crate::phrases::WordList;
use crate::recognizer::{
    Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, ServerRecognizer,
    ServerSession, ServerUrl,
};
use crate::transducer::TransducerSource;

/// The recognizer a config asks for.
pub enum Chosen {
    Server(ServerRecognizer),
    Local(Arc<LocalRecognizer<TransducerSource>>),
}

/// An open recognition of either kind.
pub enum ChosenSession {
    Server(Box<ServerSession>),
    Local(LocalSession),
}

impl Chosen {
    /// The recognizer for `engine`; nothing is fetched or loaded until [`Self::local`]'s `load`
    /// or the first dictation.
    #[must_use]
    pub fn new(
        engine: &SttEngine,
        server: &ServerUrl,
        store: ModelStore,
        metrics: Metrics,
        phrases: &WordList,
    ) -> Self {
        match engine {
            SttEngine::Server => Self::Server(ServerRecognizer::new(server.clone())),
            SttEngine::Local(choice) => Self::Local(Arc::new(LocalRecognizer::new(
                TransducerSource::new(store, choice.clone(), phrases),
                choice.label(),
                metrics,
            ))),
        }
    }

    /// The local recognizer, to load its model ahead of the first dictation.
    #[must_use]
    pub fn local(&self) -> Option<Arc<LocalRecognizer<TransducerSource>>> {
        match self {
            Self::Local(local) => Some(Arc::clone(local)),
            Self::Server(_) => None,
        }
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
            Self::Local(local) => local.connect().await.map(ChosenSession::Local),
        }
    }
}

impl RecognizerSession for ChosenSession {
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        match self {
            Self::Server(session) => session.send_frame(frame).await,
            Self::Local(session) => session.send_frame(frame).await,
        }
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        match self {
            Self::Server(session) => session.next_event().await,
            Self::Local(session) => session.next_event().await,
        }
    }

    async fn cancel(self) {
        match self {
            Self::Server(session) => session.cancel().await,
            Self::Local(session) => session.cancel().await,
        }
    }

    async fn close(self) {
        match self {
            Self::Server(session) => session.close().await,
            Self::Local(session) => session.close().await,
        }
    }
}

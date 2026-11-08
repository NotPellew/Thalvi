//! Owns the speech recognizer trait and the server's Meta realtime ASR factory.

use std::sync::{Arc, Mutex, PoisonError};

use thalvi_asr::{Connection, Handshake};

use crate::phrases::PhraseStore;

pub use thalvi_asr::Event as RecognizerEvent;

/// The largest audio frame a client may send: one second of 24 kHz mono s16le.
pub const MAX_FRAME_BYTES: usize = 48_000;

/// Accepts a client audio frame only when it is whole s16le samples, at most one second.
///
/// # Errors
/// Returns a plain-words reason for an empty, oversized or odd-length frame.
pub fn validate_frame(audio: &[u8]) -> Result<(), String> {
    if audio.is_empty() || audio.len() > MAX_FRAME_BYTES || !audio.len().is_multiple_of(2) {
        return Err("Send at most one second of 24 kHz mono s16le audio at a time.".to_string());
    }
    Ok(())
}

/// A recognizer connection failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognizerError {
    /// The connection must not be retried (close code 1008).
    Fatal(String),
    /// The connection may be re-established.
    Retryable(String),
}

impl std::fmt::Display for RecognizerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fatal(message) | Self::Retryable(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RecognizerError {}

impl RecognizerError {
    /// Whether reconnecting is allowed after this error.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }
}

impl From<thalvi_asr::Error> for RecognizerError {
    fn from(error: thalvi_asr::Error) -> Self {
        match error {
            thalvi_asr::Error::Fatal(message) => Self::Fatal(message),
            error => Self::Retryable(error.to_string()),
        }
    }
}

/// A live recognizer connection.
pub trait Recognizer: Send {
    /// Forwards one chunk of microphone PCM audio.
    fn send_audio(
        &mut self,
        audio: &[u8],
    ) -> impl std::future::Future<Output = Result<(), RecognizerError>> + Send;

    /// Waits for the next recognizer event; `None` means the connection ended.
    fn next_event(
        &mut self,
    ) -> impl std::future::Future<Output = Option<Result<RecognizerEvent, RecognizerError>>> + Send;

    /// Ends the recognizer session and closes the connection.
    fn close(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

/// Creates recognizer connections.
pub trait RecognizerFactory: Send + Sync {
    /// The recognizer produced by this factory.
    type Rec: Recognizer;

    /// Opens a new recognizer connection and completes its handshake.
    fn connect(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::Rec, RecognizerError>> + Send;
}

/// Recognizer factory for the Meta realtime ASR protocol, spoken by Meta and by `speech/asr`.
#[derive(Clone)]
pub struct MetaRecognizerFactory {
    url: String,
    model: String,
    api_key: String,
    phrases: Arc<PhraseStore>,
    /// What the last handshake said about phrase boosting; `None` before one.
    boosting: Arc<Mutex<Option<bool>>>,
}

impl MetaRecognizerFactory {
    /// Creates a factory from validated configuration values; recognizers that announce phrase
    /// boosting get `phrases` before any audio.
    #[must_use]
    pub fn new(url: String, model: String, api_key: String, phrases: Arc<PhraseStore>) -> Self {
        Self {
            url,
            model,
            api_key,
            phrases,
            boosting: Arc::default(),
        }
    }

    /// Whether the recognizer boosts a word list, as its last handshake said; `None` before one.
    #[must_use]
    pub fn boosts_phrases(&self) -> Option<bool> {
        *self.boosting.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A live Meta realtime ASR connection.
pub struct MetaRecognizer {
    connection: Connection,
}

impl RecognizerFactory for MetaRecognizerFactory {
    type Rec = MetaRecognizer;

    async fn connect(&self) -> Result<Self::Rec, RecognizerError> {
        let phrases = self.phrases.list();
        let connection = Connection::open(Handshake {
            url: &self.url,
            model: &self.model,
            api_key: &self.api_key,
            phrases: &phrases,
        })
        .await?;
        *self.boosting.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(connection.boosts_phrases());
        Ok(MetaRecognizer { connection })
    }
}

impl Recognizer for MetaRecognizer {
    async fn send_audio(&mut self, audio: &[u8]) -> Result<(), RecognizerError> {
        Ok(self.connection.send_audio(audio.to_vec()).await?)
    }

    async fn next_event(&mut self) -> Option<Result<RecognizerEvent, RecognizerError>> {
        self.connection
            .next_event()
            .await
            .map(|event| event.map_err(Into::into))
    }

    async fn close(&mut self) {
        self.connection.close().await;
    }
}

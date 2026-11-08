//! Owns the desktop's direct client of a realtime ASR recognizer, behind the recognizer seam.

use std::time::Duration;

use thalvi_asr::{Connection, Error, Event, Handshake};

use crate::entries::Realtime;
use crate::frame::Frame;
use crate::phrases::WordList;
use crate::recognizer::{
    ConnectFailure, Detail, Recognizer, RecognizerError, RecognizerEvent, RecognizerSession,
    SpeechSource, Upsampler,
};

const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Connects to a recognizer that speaks Meta's realtime ASR protocol.
#[derive(Clone, Debug)]
pub struct RealtimeRecognizer {
    realtime: Realtime,
    speech: SpeechSource,
    phrases: WordList,
}

impl RealtimeRecognizer {
    /// The recognizer of the entry called `name`; it gets `phrases` if it announces boosting.
    #[must_use]
    pub fn new(name: &str, realtime: &Realtime, phrases: &WordList) -> Self {
        Self {
            speech: SpeechSource::direct(name, realtime.address().to_owned()),
            realtime: realtime.clone(),
            phrases: phrases.clone(),
        }
    }
}

/// An open realtime ASR stream.
pub struct RealtimeSession {
    connection: Connection,
    upsampler: Upsampler,
    speech: SpeechSource,
}

fn failure(speech: &SpeechSource, error: Error) -> RecognizerError {
    let speech = speech.clone();
    match error {
        Error::Connect(error) => RecognizerError::Unreachable {
            speech,
            reason: ConnectFailure::of(&error),
            detail: Detail(error.to_string()),
        },
        Error::Fatal(message) | Error::Refused(message) => {
            RecognizerError::Reported { speech, message }
        }
        Error::Protocol(_) => RecognizerError::NotUnderstood { speech },
        Error::Retryable(_) => RecognizerError::Dropped { speech },
    }
}

impl Recognizer for RealtimeRecognizer {
    type Session = RealtimeSession;

    async fn connect(&self) -> Result<RealtimeSession, RecognizerError> {
        let connection = Connection::open(Handshake {
            url: self.realtime.address(),
            model: self.realtime.model(),
            api_key: self.realtime.api_key(),
            phrases: self.phrases.phrases(),
        })
        .await
        .map_err(|error| failure(&self.speech, error))?;
        Ok(RealtimeSession {
            connection,
            upsampler: Upsampler::default(),
            speech: self.speech.clone(),
        })
    }
}

impl RecognizerSession for RealtimeSession {
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        let bytes: Vec<u8> = self
            .upsampler
            .push(frame)
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        self.connection
            .send_audio(bytes)
            .await
            .map_err(|error| failure(&self.speech, error))
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        loop {
            match self.connection.next_event().await {
                Some(Ok(Event::Partial { text })) => return Ok(RecognizerEvent::Partial(text)),
                Some(Ok(Event::SpeechComplete { text, .. })) => {
                    return Ok(RecognizerEvent::Final(text));
                }
                Some(Ok(Event::SpeechStart { .. } | Event::SpeechEnd { .. })) => {}
                Some(Ok(Event::Error { message })) => {
                    return Err(RecognizerError::Reported {
                        speech: self.speech.clone(),
                        message,
                    });
                }
                Some(Err(error)) => return Err(failure(&self.speech, error)),
                None => {
                    return Err(RecognizerError::Dropped {
                        speech: self.speech.clone(),
                    });
                }
            }
        }
    }

    async fn cancel(self) {
        self.close().await;
    }

    async fn close(mut self) {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.connection.close()).await;
    }
}

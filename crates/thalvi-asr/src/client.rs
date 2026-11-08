//! Owns the realtime ASR client: the handshake, audio, and the events the recognizer sends back.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// One event from the recognizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The user started speaking.
    SpeechStart {
        /// Recognizer turn identifier.
        turn_id: String,
        /// Milliseconds of audio the recognizer had processed.
        audio_processed_ms: Option<u64>,
    },
    /// The user stopped speaking.
    SpeechEnd {
        /// Recognizer turn identifier.
        turn_id: String,
        /// Milliseconds of audio the recognizer had processed.
        audio_processed_ms: Option<u64>,
    },
    /// A cumulative partial transcript; replaces the previous partial.
    Partial {
        /// The partial transcript text.
        text: String,
    },
    /// A completed user turn.
    SpeechComplete {
        /// Recognizer turn identifier.
        turn_id: String,
        /// The completed transcript text.
        text: String,
    },
    /// A recognizer-side failure; the connection stays usable.
    Error {
        /// Plain-words failure detail from the recognizer.
        message: String,
    },
}

/// A recognizer connection failure.
#[derive(Debug)]
pub enum Error {
    /// The recognizer could not be reached.
    Connect(tungstenite::Error),
    /// The connection must not be retried (close code 1008).
    Fatal(String),
    /// The connection may be re-established.
    Retryable(String),
    /// The recognizer answered the handshake with an error of its own, in its words.
    Refused(String),
    /// The recognizer sent something that is not the protocol.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(error) => write!(f, "recognizer connect failed: {error}"),
            Self::Fatal(message)
            | Self::Retryable(message)
            | Self::Refused(message)
            | Self::Protocol(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Whether reconnecting is allowed after this error.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self::Retryable(message.into())
    }
}

/// What a recognizer is asked for, and what it was told to favour.
#[derive(Debug, Clone, Copy)]
pub struct Handshake<'a> {
    pub url: &'a str,
    pub model: &'a str,
    pub api_key: &'a str,
    /// Sent to recognizers that announce phrase boosting; others never see it.
    pub phrases: &'a [String],
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// An open realtime ASR connection, past its handshake.
pub struct Connection {
    socket: Socket,
    boosts_phrases: bool,
}

impl Connection {
    /// Connects, sends the session request and waits for the recognizer to accept it.
    ///
    /// # Errors
    /// Fails when the recognizer cannot be reached, refuses the session, or answers the
    /// handshake with anything else.
    pub async fn open(handshake: Handshake<'_>) -> Result<Self, Error> {
        let (mut socket, _response) = tokio_tungstenite::connect_async(handshake.url)
            .await
            .map_err(Error::Connect)?;
        let init = json!({
            "authorization": { "accessToken": format!("Bearer {}", handshake.api_key) },
            "audioEncoding": "PCM_24KHZ",
            "model": handshake.model,
            "mode": "ENDPOINTING",
        });
        socket
            .send(Message::Text(init.to_string().into()))
            .await
            .map_err(|error| Error::retryable(format!("recognizer handshake failed: {error}")))?;
        let ack = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.next())
            .await
            .map_err(|_| Error::retryable("recognizer handshake timed out"))?;
        match ack {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).map_err(|error| {
                    Error::Protocol(format!("recognizer handshake was not JSON: {error}"))
                })?;
                if value.get("sessionId").is_some() {
                    let boosts_phrases =
                        value.get("phraseBoosting").and_then(Value::as_bool) == Some(true);
                    // Meta's recognizer doesn't announce boosting and never sees this message.
                    if boosts_phrases && !handshake.phrases.is_empty() {
                        let message = json!({"type": "phrases", "phrases": handshake.phrases});
                        socket
                            .send(Message::Text(message.to_string().into()))
                            .await
                            .map_err(|error| {
                                Error::retryable(format!("recognizer handshake failed: {error}"))
                            })?;
                    }
                    Ok(Self {
                        socket,
                        boosts_phrases,
                    })
                } else if value.get("type").and_then(Value::as_str) == Some("error") {
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("recognizer rejected the session");
                    Err(Error::Refused(message.to_string()))
                } else {
                    Err(Error::Protocol(
                        "recognizer handshake did not contain a sessionId".to_string(),
                    ))
                }
            }
            Some(Ok(Message::Close(frame))) => Err(close_error(frame.as_ref())),
            Some(Ok(_)) => Err(Error::Protocol(
                "recognizer handshake had an unexpected frame".to_string(),
            )),
            Some(Err(error)) => Err(Error::retryable(format!(
                "recognizer handshake failed: {error}"
            ))),
            None => Err(Error::retryable("recognizer closed during handshake")),
        }
    }

    /// Whether the recognizer announced that it boosts a word list.
    #[must_use]
    pub fn boosts_phrases(&self) -> bool {
        self.boosts_phrases
    }

    /// Forwards one chunk of 24 kHz little-endian PCM.
    ///
    /// # Errors
    /// Fails when the connection can no longer be written to.
    pub async fn send_audio(&mut self, audio: Vec<u8>) -> Result<(), Error> {
        self.socket
            .send(Message::Binary(audio.into()))
            .await
            .map_err(|error| Error::retryable(format!("recognizer send failed: {error}")))
    }

    /// Waits for the next event; `None` never happens while the connection lives, because an
    /// ending connection is reported as an error.
    pub async fn next_event(&mut self) -> Option<Result<Event, Error>> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = parse_event(&text) {
                        return Some(event);
                    }
                }
                Some(Ok(Message::Close(frame))) => return Some(Err(close_error(frame.as_ref()))),
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Some(Err(Error::retryable(format!(
                        "recognizer connection failed: {error}"
                    ))));
                }
                None => return Some(Err(Error::retryable("recognizer connection closed"))),
            }
        }
    }

    /// Ends the session and closes the connection; best effort.
    pub async fn close(&mut self) {
        let _ = self
            .socket
            .send(Message::Text(r#"{"type":"endStream"}"#.into()))
            .await;
        let _ = self.socket.close(None).await;
    }
}

fn close_error(frame: Option<&CloseFrame>) -> Error {
    let (code, reason) = frame.map_or((0_u16, String::new()), |frame| {
        (u16::from(frame.code), frame.reason.to_string())
    });
    let detail = if reason.is_empty() {
        format!("recognizer closed the connection (code {code})")
    } else {
        format!("recognizer closed the connection (code {code}): {reason}")
    };
    if code == u16::from(CloseCode::Policy) {
        Error::Fatal(detail)
    } else {
        Error::Retryable(detail)
    }
}

/// Parses one recognizer text frame; ignored frames return `None`.
#[must_use]
pub fn parse_event(text: &str) -> Option<Result<Event, Error>> {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return Some(Err(Error::Protocol(format!(
                "recognizer sent invalid JSON: {error}"
            ))));
        }
    };
    let turn_id = value
        .get("turnId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let audio_processed_ms = value.get("audioProcessedMs").and_then(Value::as_u64);
    let transcript = || {
        value
            .get("transcript")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match value.get("type").and_then(Value::as_str) {
        Some("speechStart") => Some(Ok(Event::SpeechStart {
            turn_id,
            audio_processed_ms,
        })),
        Some("speechEnd") => Some(Ok(Event::SpeechEnd {
            turn_id,
            audio_processed_ms,
        })),
        Some("transcript") => Some(Ok(Event::Partial { text: transcript() })),
        Some("speechComplete") => Some(Ok(Event::SpeechComplete {
            turn_id,
            text: transcript(),
        })),
        Some("error") => Some(Ok(Event::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("speech recognition failed")
                .to_string(),
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, parse_event};

    #[test]
    fn parses_events_and_ignores_progress_frames() {
        let cases: Vec<(&str, Option<Event>)> = vec![
            (
                r#"{"type":"speechStart","turnId":"1","audioProcessedMs":120}"#,
                Some(Event::SpeechStart {
                    turn_id: "1".to_string(),
                    audio_processed_ms: Some(120),
                }),
            ),
            (
                r#"{"type":"speechEnd","turnId":"1","audioProcessedMs":640}"#,
                Some(Event::SpeechEnd {
                    turn_id: "1".to_string(),
                    audio_processed_ms: Some(640),
                }),
            ),
            (
                r#"{"type":"transcript","transcript":"hel","final":false}"#,
                Some(Event::Partial {
                    text: "hel".to_string(),
                }),
            ),
            (
                r#"{"type":"speechComplete","turnId":"1","transcript":"hello"}"#,
                Some(Event::SpeechComplete {
                    turn_id: "1".to_string(),
                    text: "hello".to_string(),
                }),
            ),
            (
                r#"{"type":"error","message":"boom"}"#,
                Some(Event::Error {
                    message: "boom".to_string(),
                }),
            ),
            (r#"{"type":"audioProgress"}"#, None),
        ];
        for (frame, expected) in cases {
            let parsed = parse_event(frame).map(|result| result.unwrap());
            assert_eq!(parsed, expected);
        }
    }
}

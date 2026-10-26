//! Owns the speech recognizer trait and the Meta realtime ASR client.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// One event from the recognizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognizerEvent {
    /// The user started speaking.
    SpeechStart,
    /// A cumulative partial transcript; replaces the previous partial.
    Partial {
        /// The partial transcript text.
        text: String,
    },
    /// A completed user turn.
    SpeechComplete {
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

    fn retryable(message: impl Into<String>) -> Self {
        Self::Retryable(message.into())
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

/// Recognizer factory for the Meta realtime ASR service.
#[derive(Debug, Clone)]
pub struct MetaRecognizerFactory {
    url: String,
    model: String,
    api_key: String,
}

impl MetaRecognizerFactory {
    /// Creates a factory from validated configuration values.
    #[must_use]
    pub fn new(url: String, model: String, api_key: String) -> Self {
        Self {
            url,
            model,
            api_key,
        }
    }
}

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A live Meta realtime ASR connection.
pub struct MetaRecognizer {
    socket: ClientSocket,
}

impl RecognizerFactory for MetaRecognizerFactory {
    type Rec = MetaRecognizer;

    async fn connect(&self) -> Result<Self::Rec, RecognizerError> {
        let (mut socket, _response) =
            tokio_tungstenite::connect_async(&self.url)
                .await
                .map_err(|error| {
                    RecognizerError::retryable(format!("recognizer connect failed: {error}"))
                })?;
        let init = json!({
            "authorization": { "accessToken": format!("Bearer {}", self.api_key) },
            "audioEncoding": "PCM_24KHZ",
            "model": self.model,
            "mode": "ENDPOINTING",
        });
        socket
            .send(WsMessage::Text(init.to_string().into()))
            .await
            .map_err(|error| {
                RecognizerError::retryable(format!("recognizer handshake failed: {error}"))
            })?;
        let ack = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .map_err(|_| RecognizerError::retryable("recognizer handshake timed out"))?;
        match ack {
            Some(Ok(WsMessage::Text(text))) => {
                let value: Value = serde_json::from_str(&text).map_err(|error| {
                    RecognizerError::retryable(format!(
                        "recognizer handshake was not JSON: {error}"
                    ))
                })?;
                if value.get("sessionId").is_some() {
                    Ok(Self::Rec { socket })
                } else if value.get("type").and_then(Value::as_str) == Some("error") {
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("recognizer rejected the session");
                    Err(RecognizerError::retryable(message.to_string()))
                } else {
                    Err(RecognizerError::retryable(
                        "recognizer handshake did not contain a sessionId",
                    ))
                }
            }
            Some(Ok(WsMessage::Close(frame))) => Err(close_error(frame.as_ref())),
            Some(Ok(_)) => Err(RecognizerError::retryable(
                "recognizer handshake had an unexpected frame",
            )),
            Some(Err(error)) => Err(RecognizerError::retryable(format!(
                "recognizer handshake failed: {error}"
            ))),
            None => Err(RecognizerError::retryable(
                "recognizer closed during handshake",
            )),
        }
    }
}

impl Recognizer for MetaRecognizer {
    async fn send_audio(&mut self, audio: &[u8]) -> Result<(), RecognizerError> {
        self.socket
            .send(WsMessage::Binary(audio.to_vec().into()))
            .await
            .map_err(|error| RecognizerError::retryable(format!("recognizer send failed: {error}")))
    }

    async fn next_event(&mut self) -> Option<Result<RecognizerEvent, RecognizerError>> {
        loop {
            match self.socket.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Some(event) = parse_recognizer_text(&text) {
                        return Some(event);
                    }
                }
                Some(Ok(WsMessage::Close(frame))) => {
                    return Some(Err(close_error(frame.as_ref())));
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Some(Err(RecognizerError::retryable(format!(
                        "recognizer connection failed: {error}"
                    ))));
                }
                None => {
                    return Some(Err(RecognizerError::retryable(
                        "recognizer connection closed",
                    )));
                }
            }
        }
    }

    async fn close(&mut self) {
        let _ = self
            .socket
            .send(WsMessage::Text(r#"{"type":"endStream"}"#.into()))
            .await;
        let _ = self.socket.close(None).await;
    }
}

fn close_error(frame: Option<&CloseFrame>) -> RecognizerError {
    let (code, reason) = frame.map_or((0_u16, String::new()), |frame| {
        (u16::from(frame.code), frame.reason.to_string())
    });
    let detail = if reason.is_empty() {
        format!("recognizer closed the connection (code {code})")
    } else {
        format!("recognizer closed the connection (code {code}): {reason}")
    };
    if code == u16::from(CloseCode::Policy) {
        RecognizerError::Fatal(detail)
    } else {
        RecognizerError::Retryable(detail)
    }
}

/// Classifies a close code: only 1008 forbids reconnecting.
#[must_use]
pub fn close_code_is_fatal(code: u16) -> bool {
    code == 1008
}

/// Parses one recognizer text frame; ignored frames return `None`.
#[must_use]
pub fn parse_recognizer_text(text: &str) -> Option<Result<RecognizerEvent, RecognizerError>> {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return Some(Err(RecognizerError::retryable(format!(
                "recognizer sent invalid JSON: {error}"
            ))));
        }
    };
    match value.get("type").and_then(Value::as_str) {
        Some("speechStart") => Some(Ok(RecognizerEvent::SpeechStart)),
        Some("transcript") => Some(Ok(RecognizerEvent::Partial {
            text: value
                .get("transcript")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })),
        Some("speechComplete") => Some(Ok(RecognizerEvent::SpeechComplete {
            text: value
                .get("transcript")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })),
        Some("error") => Some(Ok(RecognizerEvent::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("speech recognition failed")
                .to_string(),
        })),
        _ => None,
    }
}

/// Builds the recognizer handshake frame for a model and API key.
#[must_use]
pub fn handshake_frame(model: &str, api_key: &str) -> String {
    json!({
        "authorization": { "accessToken": format!("Bearer {api_key}") },
        "audioEncoding": "PCM_24KHZ",
        "model": model,
        "mode": "ENDPOINTING",
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{RecognizerEvent, close_code_is_fatal, handshake_frame, parse_recognizer_text};

    #[test]
    fn handshake_shape_matches_contract() {
        let frame = handshake_frame("muse-voice", "secret");
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["authorization"]["accessToken"], "Bearer secret");
        assert_eq!(value["audioEncoding"], "PCM_24KHZ");
        assert_eq!(value["model"], "muse-voice");
        assert_eq!(value["mode"], "ENDPOINTING");
    }

    #[test]
    fn parses_events_and_ignores_progress_frames() {
        let cases: Vec<(&str, Option<RecognizerEvent>)> = vec![
            (
                r#"{"type":"speechStart","turnId":"1"}"#,
                Some(RecognizerEvent::SpeechStart),
            ),
            (
                r#"{"type":"transcript","transcript":"hel","final":false}"#,
                Some(RecognizerEvent::Partial {
                    text: "hel".to_string(),
                }),
            ),
            (
                r#"{"type":"speechComplete","turnId":"1","transcript":"hello"}"#,
                Some(RecognizerEvent::SpeechComplete {
                    text: "hello".to_string(),
                }),
            ),
            (
                r#"{"type":"error","message":"boom"}"#,
                Some(RecognizerEvent::Error {
                    message: "boom".to_string(),
                }),
            ),
            (r#"{"type":"speechEnd","turnId":"1"}"#, None),
            (r#"{"type":"audioProgress"}"#, None),
        ];
        for (frame, expected) in cases {
            let parsed = parse_recognizer_text(frame).map(|result| result.unwrap());
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn only_policy_close_is_fatal() {
        assert!(close_code_is_fatal(1008));
        for code in [1000_u16, 1011, 1013] {
            assert!(!close_code_is_fatal(code));
        }
    }
}

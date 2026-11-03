//! Owns the streaming speech recognizer seam, the server `/transcription` client and the 16 to 24 kHz upsampler it needs.

use std::{
    future::Future,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{self, Message, protocol::WebSocketConfig},
};

use crate::frame::{FRAME_SAMPLES, Frame};
use crate::report::UserError;

/// Samples in one frame after upsampling to 24 kHz.
pub const UPSAMPLED_SAMPLES: usize = FRAME_SAMPLES / 2 * 3;

const MAX_MESSAGE_BYTES: usize = 100_000;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Text the recognizer produced; only a `Final` can arm an ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecognizerEvent {
    Partial(String),
    Final(String),
}

/// What the user does when the speech server cannot be used.
const START_SERVER: &str = "Start the Thalvi server, or fix Settings → Speech → Server address.";

/// Where dictation is recognized, as the settings name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeechSource {
    Server(ServerUrl),
    /// The model on this computer; `model` is its name or folder.
    Local {
        model: String,
    },
}

impl SpeechSource {
    /// Names what is not working, for the start of a sentence.
    fn subject(&self) -> String {
        match self {
            Self::Server(server) => format!("The speech server at {server}"),
            Self::Local { model } => format!("The speech model {model}"),
        }
    }

    /// What to do about it, naming the setting.
    pub(crate) fn fix(&self) -> &'static str {
        match self {
            Self::Server(_) => START_SERVER,
            Self::Local { .. } => {
                "Wait for it to finish loading, or pick another under Settings → Speech → Speech model."
            }
        }
    }
}

impl std::fmt::Display for SpeechSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.subject())
    }
}

/// Why connecting to the server failed, as plain words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConnectFailure {
    #[error("connection refused")]
    Refused,
    #[error("timed out")]
    TimedOut,
    #[error("network or host unreachable")]
    Unreachable,
    #[error("it answered HTTP {0} instead of a speech connection")]
    Rejected(u16),
    #[error("the secure connection failed")]
    Tls,
    #[error("network error")]
    Other,
}

impl ConnectFailure {
    fn of(error: &tungstenite::Error) -> Self {
        use std::io::ErrorKind;
        match error {
            tungstenite::Error::Io(io) => match io.kind() {
                ErrorKind::ConnectionRefused => Self::Refused,
                ErrorKind::TimedOut => Self::TimedOut,
                ErrorKind::NetworkUnreachable | ErrorKind::HostUnreachable => Self::Unreachable,
                _ => Self::Other,
            },
            tungstenite::Error::Http(response) => Self::Rejected(response.status().as_u16()),
            tungstenite::Error::Tls(_) => Self::Tls,
            _ => Self::Other,
        }
    }
}

/// The technical text behind a failure, kept for the error report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Detail(pub String);

/// A failure the user can be told about in plain words; `Display` says what failed, why and
/// what to do.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecognizerError {
    #[error("Thalvi could not reach the speech server at {server} ({reason}). {START_SERVER}")]
    Unreachable {
        server: ServerUrl,
        reason: ConnectFailure,
        #[source]
        detail: Detail,
    },
    #[error(
        "The speech server at {server} closed the connection. Nothing was delivered. Check that the Thalvi server is still running."
    )]
    Dropped { server: ServerUrl },
    #[error(
        "The speech server at {server} sent something Thalvi did not understand. Check that Settings → Speech → Server address points at a Thalvi server."
    )]
    NotUnderstood { server: ServerUrl },
    /// Text the recognizer wrote for the user itself.
    #[error("{0}")]
    Said(String),
}

impl UserError for RecognizerError {
    fn headline(&self) -> &'static str {
        match self {
            Self::Unreachable { .. } => "Can't reach speech server — Settings",
            Self::Dropped { .. } => "Speech server hung up — Settings",
            Self::NotUnderstood { .. } => "Bad speech server reply — Settings",
            Self::Said(_) => "Speech recognition failed — Settings",
        }
    }
}

impl RecognizerError {
    #[must_use]
    pub fn said(message: impl Into<String>) -> Self {
        Self::Said(message.into())
    }
}

/// One open recognition stream; it exists only after the recognizer said it is ready.
pub trait RecognizerSession: Send {
    /// Sends one 16 kHz frame.
    fn send_frame(
        &mut self,
        frame: &Frame,
    ) -> impl Future<Output = Result<(), RecognizerError>> + Send;

    /// Waits for the next text event; safe to drop and call again.
    fn next_event(
        &mut self,
    ) -> impl Future<Output = Result<RecognizerEvent, RecognizerError>> + Send;

    /// Abandons the capture and releases the stream; best effort.
    fn cancel(self) -> impl Future<Output = ()> + Send;

    /// Ends the stream normally and releases it; best effort.
    fn close(self) -> impl Future<Output = ()> + Send;
}

pub trait Recognizer: Send + Sync {
    type Session: RecognizerSession;

    /// Resolves once the recognizer is ready for audio.
    fn connect(&self) -> impl Future<Output = Result<Self::Session, RecognizerError>> + Send;
}

/// Why a server URL was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServerUrlError {
    #[error("server must be a ws or wss URL with a host and no credentials.")]
    Invalid,
    #[error("server must be the server root, without a path, query, or fragment.")]
    NotRoot,
}

/// The validated root of the Thalvi server, as `ws://host:port` or `wss://host:port`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub struct ServerUrl(String);

impl Default for ServerUrl {
    /// The local server, `ws://127.0.0.1:8080`.
    fn default() -> Self {
        Self("ws://127.0.0.1:8080".to_string())
    }
}

impl ServerUrl {
    /// # Errors
    /// Fails when `value` is not a `ws` or `wss` server root without credentials.
    pub fn parse(value: &str) -> Result<Self, ServerUrlError> {
        let url = reqwest::Url::parse(value).map_err(|_| ServerUrlError::Invalid)?;
        if !matches!(url.scheme(), "ws" | "wss")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ServerUrlError::Invalid);
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(ServerUrlError::NotRoot);
        }
        Ok(Self(url.as_str().trim_end_matches('/').to_string()))
    }

    fn transcription(&self) -> String {
        format!("{}/transcription", self.0)
    }
}

impl std::fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ServerUrl {
    type Error = ServerUrlError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

/// Converts a 16 kHz stream to 24 kHz by 2:3 linear interpolation, carrying one sample across frames.
#[derive(Clone, Debug, Default)]
pub struct Upsampler {
    previous: Option<i16>,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "a weighted average of two i16 values fits in i16"
)]
fn mix(a: i16, b: i16) -> i16 {
    (i32::from(a) + 2 * i32::from(b) + 1).div_euclid(3) as i16
}

impl Upsampler {
    /// Upsamples the next frame; the output is delayed by one input sample so every frame yields exactly [`UPSAMPLED_SAMPLES`].
    pub fn push(&mut self, frame: &Frame) -> [i16; UPSAMPLED_SAMPLES] {
        let mut previous = self.previous.unwrap_or(frame[0]);
        let mut out = [0; UPSAMPLED_SAMPLES];
        for ([b, c], out) in frame
            .as_chunks::<2>()
            .0
            .iter()
            .zip(out.as_chunks_mut::<3>().0)
        {
            let (b, c) = (*b, *c);
            out[0] = previous;
            out[1] = mix(previous, b);
            out[2] = mix(c, b);
            previous = c;
        }
        self.previous = Some(previous);
        out
    }
}

/// Connects to the Thalvi server's `/transcription` route.
#[derive(Clone, Debug)]
pub struct ServerRecognizer {
    url: ServerUrl,
}

impl ServerRecognizer {
    #[must_use]
    pub fn new(url: ServerUrl) -> Self {
        Self { url }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// An open `/transcription` stream.
pub struct ServerSession {
    socket: Socket,
    upsampler: Upsampler,
    server: ServerUrl,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ready,
    Partial { text: String },
    Final { text: String },
    SpeechStart,
    SpeechEnd,
    Error { message: Option<String> },
}

fn parse(text: &str, server: &ServerUrl) -> Result<ServerMessage, RecognizerError> {
    serde_json::from_str(text).map_err(|_| RecognizerError::NotUnderstood {
        server: server.clone(),
    })
}

fn dropped(server: &ServerUrl) -> RecognizerError {
    RecognizerError::Dropped {
        server: server.clone(),
    }
}

impl Recognizer for ServerRecognizer {
    type Session = ServerSession;

    async fn connect(&self) -> Result<ServerSession, RecognizerError> {
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES));
        let server = &self.url;
        let (mut socket, _) = connect_async_with_config(server.transcription(), Some(config), true)
            .await
            .map_err(|error| RecognizerError::Unreachable {
                server: server.clone(),
                reason: ConnectFailure::of(&error),
                detail: Detail(error.to_string()),
            })?;
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => match parse(&text, server) {
                    Ok(ServerMessage::Ready) => break,
                    Ok(ServerMessage::Error {
                        message: Some(message),
                    }) => return Err(RecognizerError::said(message)),
                    _ => {
                        return Err(RecognizerError::NotUnderstood {
                            server: server.clone(),
                        });
                    }
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(dropped(server)),
                Some(Ok(_)) => {
                    return Err(RecognizerError::NotUnderstood {
                        server: server.clone(),
                    });
                }
            }
        }
        Ok(ServerSession {
            socket,
            upsampler: Upsampler::default(),
            server: server.clone(),
        })
    }
}

impl RecognizerSession for ServerSession {
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        let bytes: Vec<u8> = self
            .upsampler
            .push(frame)
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        self.socket
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|_| dropped(&self.server))
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => match parse(&text, &self.server)? {
                    ServerMessage::Partial { text } => return Ok(RecognizerEvent::Partial(text)),
                    ServerMessage::Final { text } => return Ok(RecognizerEvent::Final(text)),
                    ServerMessage::Error { message } => {
                        return Err(RecognizerError::said(
                            message.unwrap_or_else(|| "Speech recognition failed.".to_string()),
                        ));
                    }
                    ServerMessage::SpeechStart | ServerMessage::SpeechEnd => {}
                    ServerMessage::Ready => {
                        return Err(RecognizerError::NotUnderstood {
                            server: self.server.clone(),
                        });
                    }
                },
                Some(Ok(Message::Binary(_))) => {
                    return Err(RecognizerError::NotUnderstood {
                        server: self.server.clone(),
                    });
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(dropped(&self.server)),
            }
        }
    }

    async fn cancel(mut self) {
        let _ = tokio::time::timeout(
            CLOSE_TIMEOUT,
            self.socket
                .send(Message::Text(r#"{"type":"cancel"}"#.into())),
        )
        .await;
        self.close().await;
    }

    async fn close(mut self) {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.socket.close(None)).await;
    }
}

/// What a [`FakeRecognizer`] was asked to do, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeCall {
    Frame(Box<Frame>),
    Cancel,
    Close,
}

/// A scripted recognizer for tests; [`FakeControl`] drives it.
pub struct FakeRecognizer {
    ready: Mutex<Option<oneshot::Receiver<Result<(), RecognizerError>>>>,
    events: Mutex<Option<mpsc::UnboundedReceiver<Result<RecognizerEvent, RecognizerError>>>>,
    calls: Arc<Mutex<Vec<FakeCall>>>,
    stalled: Arc<AtomicBool>,
}

/// Test-side handle of a [`FakeRecognizer`].
pub struct FakeControl {
    ready: Option<oneshot::Sender<Result<(), RecognizerError>>>,
    events: mpsc::UnboundedSender<Result<RecognizerEvent, RecognizerError>>,
    calls: Arc<Mutex<Vec<FakeCall>>>,
    stalled: Arc<AtomicBool>,
}

/// The stream a [`FakeRecognizer`] hands out once ready.
pub struct FakeSession {
    events: mpsc::UnboundedReceiver<Result<RecognizerEvent, RecognizerError>>,
    calls: Arc<Mutex<Vec<FakeCall>>>,
    stalled: Arc<AtomicBool>,
}

impl FakeRecognizer {
    /// `connect` stays pending until [`FakeControl::ready`] or [`FakeControl::fail`] is called.
    #[must_use]
    pub fn new() -> (Self, FakeControl) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stalled = Arc::new(AtomicBool::new(false));
        (
            Self {
                ready: Mutex::new(Some(ready_rx)),
                events: Mutex::new(Some(events_rx)),
                calls: Arc::clone(&calls),
                stalled: Arc::clone(&stalled),
            },
            FakeControl {
                ready: Some(ready_tx),
                events: events_tx,
                calls,
                stalled,
            },
        )
    }
}

fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

fn fake_dropped() -> RecognizerError {
    RecognizerError::said("Speech recognition disconnected. Nothing was delivered.")
}

impl Recognizer for FakeRecognizer {
    type Session = FakeSession;

    async fn connect(&self) -> Result<FakeSession, RecognizerError> {
        let (Some(ready), Some(events)) = (take(&self.ready), take(&self.events)) else {
            return Err(RecognizerError::said("The fake recognizer connects once."));
        };
        ready.await.map_err(|_| fake_dropped())??;
        Ok(FakeSession {
            events,
            calls: Arc::clone(&self.calls),
            stalled: Arc::clone(&self.stalled),
        })
    }
}

impl FakeControl {
    pub fn ready(&mut self) {
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Ok(()));
        }
    }

    pub fn fail(&mut self, message: &str) {
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Err(RecognizerError::said(message)));
        }
    }

    pub fn emit(&self, event: Result<RecognizerEvent, RecognizerError>) {
        let _ = self.events.send(event);
    }

    /// From now on frames are accepted but never finish sending, as when the server stops reading.
    pub fn stall_sends(&self) {
        self.stalled.store(true, Ordering::Relaxed);
    }

    /// Everything the session was asked to do so far.
    pub fn calls(&self) -> Vec<FakeCall> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl FakeSession {
    fn record(&self, call: FakeCall) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }
}

impl RecognizerSession for FakeSession {
    fn send_frame(
        &mut self,
        frame: &Frame,
    ) -> impl Future<Output = Result<(), RecognizerError>> + Send {
        self.record(FakeCall::Frame(Box::new(*frame)));
        let stalled = self.stalled.load(Ordering::Relaxed);
        async move {
            if stalled {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        self.events
            .recv()
            .await
            .unwrap_or_else(|| Err(fake_dropped()))
    }

    fn cancel(self) -> impl Future<Output = ()> + Send {
        self.record(FakeCall::Cancel);
        std::future::ready(())
    }

    fn close(self) -> impl Future<Output = ()> + Send {
        self.record(FakeCall::Close);
        std::future::ready(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error as IoError, ErrorKind};

    use super::*;

    fn http(status: u16) -> tungstenite::Error {
        let response = tungstenite::http::Response::builder()
            .status(status)
            .body(None)
            .unwrap();
        tungstenite::Error::Http(Box::new(response))
    }

    #[test]
    fn a_failed_connection_is_told_apart_and_says_what_to_fix() {
        let server = ServerUrl::parse("ws://127.0.0.1:9").unwrap();
        let cases = [
            (
                tungstenite::Error::Io(IoError::from(ErrorKind::ConnectionRefused)),
                ConnectFailure::Refused,
                "connection refused",
            ),
            (
                tungstenite::Error::Io(IoError::from(ErrorKind::TimedOut)),
                ConnectFailure::TimedOut,
                "timed out",
            ),
            (
                tungstenite::Error::Io(IoError::from(ErrorKind::HostUnreachable)),
                ConnectFailure::Unreachable,
                "network or host unreachable",
            ),
            (
                tungstenite::Error::Io(IoError::from(ErrorKind::PermissionDenied)),
                ConnectFailure::Other,
                "network error",
            ),
            (
                http(404),
                ConnectFailure::Rejected(404),
                "it answered HTTP 404 instead of a speech connection",
            ),
        ];
        for (error, reason, words) in cases {
            assert_eq!(ConnectFailure::of(&error), reason, "{error}");
            let failure = RecognizerError::Unreachable {
                server: server.clone(),
                reason,
                detail: Detail(error.to_string()),
            };
            assert_eq!(
                failure.to_string(),
                format!(
                    "Thalvi could not reach the speech server at ws://127.0.0.1:9 ({words}). Start the Thalvi server, or fix Settings → Speech → Server address."
                )
            );
            assert_eq!(
                std::error::Error::source(&failure).map(ToString::to_string),
                Some(error.to_string())
            );
        }
    }
}

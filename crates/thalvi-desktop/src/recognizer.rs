//! Owns the streaming speech recognizer seam, the server `/transcription` client and the 16 to 24 kHz upsampler it needs.

use std::{
    future::Future,
    sync::{
        Arc, LazyLock, Mutex, PoisonError,
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
use crate::models::ModelError;
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

/// Where an entry recognizes speech, as errors and notices name it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    /// A recognizer that speaks the realtime ASR protocol, reached directly.
    Direct(String),
    ThalviServer(ServerUrl),
    OnDevice(String),
}

/// The live entry that recognizes dictation, named for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechSource {
    name: String,
    place: Place,
}

impl SpeechSource {
    /// A recognizer reached directly at `address`.
    #[must_use]
    pub fn direct(name: &str, address: String) -> Self {
        Self {
            name: name.to_owned(),
            place: Place::Direct(address),
        }
    }

    #[must_use]
    pub fn thalvi_server(name: &str, server: &ServerUrl) -> Self {
        Self {
            name: name.to_owned(),
            place: Place::ThalviServer(server.clone()),
        }
    }

    /// The model on this computer; `model` is its name or folder.
    #[must_use]
    pub fn on_device(name: &str, model: String) -> Self {
        Self {
            name: name.to_owned(),
            place: Place::OnDevice(model),
        }
    }

    /// The entry's name in Settings.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the entry runs on this computer.
    #[must_use]
    pub fn is_on_device(&self) -> bool {
        matches!(self.place, Place::OnDevice(_))
    }

    /// Names what is not working, for the start of a sentence.
    pub(crate) fn subject(&self) -> String {
        let named = self.to_string();
        let mut letters = named.chars();
        letters
            .next()
            .map(|first| first.to_uppercase().chain(letters).collect())
            .unwrap_or(named)
    }

    /// What to do about it, naming the setting.
    pub(crate) fn fix(&self) -> &'static str {
        match self.place {
            Place::Direct(_) => {
                "Check that it is running, or fix its address under Settings → Recognition."
            }
            Place::ThalviServer(_) => {
                "Start the Thalvi server, or fix its address under Settings → Recognition."
            }
            Place::OnDevice(_) => {
                "Wait for it to finish loading, or pick another under Settings → Recognition."
            }
        }
    }

    /// What the entry should be when it answers something else than a speech connection.
    fn expected(&self) -> &'static str {
        match self.place {
            Place::Direct(_) => "a realtime speech recognizer",
            Place::ThalviServer(_) => "a Thalvi server",
            Place::OnDevice(_) => "a speech model",
        }
    }
}

impl std::fmt::Display for SpeechSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = &self.name;
        match &self.place {
            Place::Direct(address) => write!(f, "the speech server \"{name}\" at {address}"),
            Place::ThalviServer(server) => write!(f, "the Thalvi server \"{name}\" at {server}"),
            Place::OnDevice(model) => write!(f, "the speech model \"{name}\" ({model})"),
        }
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
    pub(crate) fn of(error: &tungstenite::Error) -> Self {
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
/// what to do, naming the entry.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecognizerError {
    #[error("Thalvi could not reach {speech} ({reason}). {}", speech.fix())]
    Unreachable {
        speech: SpeechSource,
        reason: ConnectFailure,
        #[source]
        detail: Detail,
    },
    #[error(
        "{} closed the connection. Nothing was delivered. Check that it is still running.",
        speech.subject()
    )]
    Dropped { speech: SpeechSource },
    #[error(
        "{} sent something Thalvi did not understand. Check that its address under Settings → Recognition is {}.",
        speech.subject(),
        speech.expected()
    )]
    NotUnderstood { speech: SpeechSource },
    /// The entry refused the session or reported a failure, in its own words.
    #[error("{} reported a problem: {message}", speech.subject())]
    Reported {
        speech: SpeechSource,
        message: String,
    },
    /// A speech model that could not be provided; the headline is the model error's own.
    #[error("{message}")]
    Model {
        headline: &'static str,
        message: String,
    },
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
            Self::Model { headline, .. } => headline,
            Self::Reported { .. } | Self::Said(_) => "Speech recognition failed — Settings",
        }
    }
}

impl From<ModelError> for RecognizerError {
    fn from(error: ModelError) -> Self {
        Self::Model {
            headline: error.headline(),
            message: error.to_string(),
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

    /// The server's HTTP route at `path`, which starts with `/`.
    #[must_use]
    pub fn http(&self, path: &str) -> String {
        let root = self
            .0
            .strip_prefix("ws")
            .map_or_else(|| self.0.clone(), |rest| format!("http{rest}"));
        format!("{root}{path}")
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

/// Taps of the 16 to 24 kHz interpolation filter, at the 48 kHz rate between up by 3 and down by 2.
const TAPS: usize = 145;
/// Taps that meet an input sample for any one output sample.
const PHASE_TAPS: usize = TAPS.div_ceil(3);
/// Input samples an output still needs from earlier frames.
const HISTORY: usize = PHASE_TAPS - 1;

static PHASES: LazyLock<[[f32; PHASE_TAPS]; 3]> = LazyLock::new(phases);

/// A Blackman-windowed sinc cutting at 8 kHz, split by phase: flat to 7 kHz, images 75 dB down from 9 kHz.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "tap indices are tiny, and f32 holds the taps to well below a 16-bit step"
)]
fn phases() -> [[f32; PHASE_TAPS]; 3] {
    let centre = (TAPS - 1) as f64 / 2.0;
    let kernel: Vec<f64> = (0..TAPS)
        .map(|k| {
            let x = std::f64::consts::PI * (k as f64 - centre) / 3.0;
            let sinc = if 2 * k == TAPS - 1 { 1.0 } else { x.sin() / x };
            let angle = std::f64::consts::TAU * k as f64 / (TAPS - 1) as f64;
            sinc * (0.42 - 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos())
        })
        .collect();
    let gain = 3.0 / kernel.iter().sum::<f64>();
    let mut phases = [[0.0; PHASE_TAPS]; 3];
    for (k, tap) in kernel.iter().enumerate() {
        phases[k % 3][k / 3] = (tap * gain) as f32;
    }
    phases
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "float to integer casts saturate, so loud input clips"
)]
fn to_i16(sample: f32) -> i16 {
    sample.round() as i16
}

/// Converts a 16 kHz stream to 24 kHz: up by 3, a low-pass filter, down by 2, carrying the filter's input across frames.
pub struct Upsampler {
    // Boxed so the sessions and futures that hold one stay small.
    samples: Box<[i16; HISTORY + FRAME_SAMPLES]>,
}

impl Default for Upsampler {
    fn default() -> Self {
        Self {
            samples: Box::new([0; HISTORY + FRAME_SAMPLES]),
        }
    }
}

impl Upsampler {
    /// Upsamples the next frame; the output lags the input by the filter's 1.5 ms so every frame yields exactly [`UPSAMPLED_SAMPLES`].
    pub fn push(&mut self, frame: &Frame) -> [i16; UPSAMPLED_SAMPLES] {
        self.samples.copy_within(FRAME_SAMPLES.., 0);
        self.samples[HISTORY..].copy_from_slice(frame);
        let mut out = [0; UPSAMPLED_SAMPLES];
        for (m, out) in out.iter_mut().enumerate() {
            // Output m sits at 48 kHz sample 2m, which meets input samples on one phase of the filter.
            let phase = 2 * m % 3;
            let newest = HISTORY + (2 * m - phase) / 3;
            let sum: f32 = PHASES[phase]
                .iter()
                .zip(self.samples[newest - HISTORY..=newest].iter().rev())
                .map(|(tap, &sample)| tap * f32::from(sample))
                .sum();
            *out = to_i16(sum);
        }
        out
    }
}

/// Connects to the Thalvi server's `/transcription` route.
#[derive(Clone, Debug)]
pub struct ServerRecognizer {
    url: ServerUrl,
    speech: SpeechSource,
}

impl ServerRecognizer {
    /// The recognizer of the Thalvi server entry called `name`.
    #[must_use]
    pub fn new(name: &str, url: ServerUrl) -> Self {
        let speech = SpeechSource::thalvi_server(name, &url);
        Self { url, speech }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// An open `/transcription` stream.
pub struct ServerSession {
    socket: Socket,
    upsampler: Upsampler,
    speech: SpeechSource,
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

fn parse(text: &str, speech: &SpeechSource) -> Result<ServerMessage, RecognizerError> {
    serde_json::from_str(text).map_err(|_| RecognizerError::NotUnderstood {
        speech: speech.clone(),
    })
}

fn dropped(speech: &SpeechSource) -> RecognizerError {
    RecognizerError::Dropped {
        speech: speech.clone(),
    }
}

impl Recognizer for ServerRecognizer {
    type Session = ServerSession;

    async fn connect(&self) -> Result<ServerSession, RecognizerError> {
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES));
        let speech = &self.speech;
        let (mut socket, _) =
            connect_async_with_config(self.url.transcription(), Some(config), true)
                .await
                .map_err(|error| RecognizerError::Unreachable {
                    speech: speech.clone(),
                    reason: ConnectFailure::of(&error),
                    detail: Detail(error.to_string()),
                })?;
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => match parse(&text, speech) {
                    Ok(ServerMessage::Ready) => break,
                    Ok(ServerMessage::Error {
                        message: Some(message),
                    }) => {
                        return Err(RecognizerError::Reported {
                            speech: speech.clone(),
                            message,
                        });
                    }
                    _ => {
                        return Err(RecognizerError::NotUnderstood {
                            speech: speech.clone(),
                        });
                    }
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(dropped(speech)),
                Some(Ok(_)) => {
                    return Err(RecognizerError::NotUnderstood {
                        speech: speech.clone(),
                    });
                }
            }
        }
        Ok(ServerSession {
            socket,
            upsampler: Upsampler::default(),
            speech: speech.clone(),
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
            .map_err(|_| dropped(&self.speech))
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => match parse(&text, &self.speech)? {
                    ServerMessage::Partial { text } => return Ok(RecognizerEvent::Partial(text)),
                    ServerMessage::Final { text } => return Ok(RecognizerEvent::Final(text)),
                    ServerMessage::Error { message } => {
                        return Err(RecognizerError::Reported {
                            speech: self.speech.clone(),
                            message: message
                                .unwrap_or_else(|| "Speech recognition failed.".to_string()),
                        });
                    }
                    ServerMessage::SpeechStart | ServerMessage::SpeechEnd => {}
                    ServerMessage::Ready => {
                        return Err(RecognizerError::NotUnderstood {
                            speech: self.speech.clone(),
                        });
                    }
                },
                Some(Ok(Message::Binary(_))) => {
                    return Err(RecognizerError::NotUnderstood {
                        speech: self.speech.clone(),
                    });
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    return Err(dropped(&self.speech));
                }
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

type ReadySlot = Arc<Mutex<Option<oneshot::Receiver<Result<(), RecognizerError>>>>>;
type EventsSlot =
    Arc<Mutex<Option<mpsc::UnboundedReceiver<Result<RecognizerEvent, RecognizerError>>>>>;

/// A scripted recognizer for tests; [`FakeControl`] drives it.
pub struct FakeRecognizer {
    ready: ReadySlot,
    events: EventsSlot,
    calls: Arc<Mutex<Vec<FakeCall>>>,
    stalled: Arc<AtomicBool>,
}

/// Test-side handle of a [`FakeRecognizer`].
pub struct FakeControl {
    ready: Option<oneshot::Sender<Result<(), RecognizerError>>>,
    events: mpsc::UnboundedSender<Result<RecognizerEvent, RecognizerError>>,
    slots: (ReadySlot, EventsSlot),
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
        let ready = Arc::new(Mutex::new(Some(ready_rx)));
        let events = Arc::new(Mutex::new(Some(events_rx)));
        (
            Self {
                ready: Arc::clone(&ready),
                events: Arc::clone(&events),
                calls: Arc::clone(&calls),
                stalled: Arc::clone(&stalled),
            },
            FakeControl {
                ready: Some(ready_tx),
                events: events_tx,
                slots: (ready, events),
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
    /// Lets the recognizer connect once more, to a fresh session this control then drives; the
    /// calls log carries over.
    pub fn connect_again(&mut self) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        *self.slots.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(ready_rx);
        *self.slots.1.lock().unwrap_or_else(PoisonError::into_inner) = Some(events_rx);
        self.ready = Some(ready_tx);
        self.events = events_tx;
    }

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
        let speech =
            SpeechSource::thalvi_server("Office", &ServerUrl::parse("ws://127.0.0.1:9").unwrap());
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
                speech: speech.clone(),
                reason,
                detail: Detail(error.to_string()),
            };
            assert_eq!(
                failure.to_string(),
                format!(
                    "Thalvi could not reach the Thalvi server \"Office\" at ws://127.0.0.1:9 ({words}). Start the Thalvi server, or fix its address under Settings → Recognition."
                )
            );
            assert_eq!(
                std::error::Error::source(&failure).map(ToString::to_string),
                Some(error.to_string())
            );
        }
    }
}

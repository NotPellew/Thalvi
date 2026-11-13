//! Behavior tests for one voice session over a real WebSocket.
#![expect(clippy::unwrap_used, reason = "test setup failures panic immediately")]
#![expect(clippy::expect_used, reason = "test setup failures panic immediately")]
#![expect(
    clippy::unused_async_trait_impl,
    reason = "fake trait implementations need no await"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::routing::get;
use bytes::Bytes;
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde_json::{Value, json};
use thalvi_server::api;
use thalvi_server::briefing::{Briefing, BriefingContext};
use thalvi_server::config::BriefingConfig;
use thalvi_server::model::{
    ChatEvent, ChatMessage, ChatModel, DeltaStream, EventStream, ModelError, ToolCall,
    ToolDefinition, ToolResult,
};
use thalvi_server::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerFactory};
use thalvi_server::recorder::{RecordKinds, RecordingConfig};
use thalvi_server::session::{
    PRIVATE_ID_PREFIX, Retention, VoiceChoices, handle_session, refuse_private_resume,
};
use thalvi_server::store::SessionStore;
use thalvi_server::title::Titler;
use thalvi_server::tts::{AudioStream, Tts, TtsError};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

// ---------- fake recognizer ----------

type RecSender = tokio::sync::mpsc::UnboundedSender<Result<RecognizerEvent, RecognizerError>>;

#[derive(Clone, Default)]
struct FakeFactory {
    inner: Arc<FakeFactoryInner>,
}

#[derive(Default)]
struct FakeFactoryInner {
    connections: Mutex<Vec<RecSender>>,
    attempts: AtomicUsize,
    audio: Arc<Mutex<Vec<Vec<u8>>>>,
    connect_gate: Mutex<Option<Arc<Notify>>>,
    connect_errors: Mutex<VecDeque<RecognizerError>>,
}

impl FakeFactory {
    fn attempts(&self) -> usize {
        self.inner.attempts.load(Ordering::SeqCst)
    }

    fn audio_received(&self) -> Vec<Vec<u8>> {
        self.inner.audio.lock().unwrap().clone()
    }

    fn hold_connect(&self, gate: Arc<Notify>) {
        *self.inner.connect_gate.lock().unwrap() = Some(gate);
    }

    fn fail_connects(&self, count: usize, message: &str) {
        self.inner
            .connect_errors
            .lock()
            .unwrap()
            .extend((0..count).map(|_| RecognizerError::Retryable(message.to_string())));
    }

    fn push(&self, connection: usize, item: Result<RecognizerEvent, RecognizerError>) {
        let sender = self.inner.connections.lock().unwrap()[connection].clone();
        let _ = sender.send(item);
    }

    /// Whether the session let go of this connection's recognizer.
    fn released(&self, connection: usize) -> bool {
        self.inner.connections.lock().unwrap()[connection].is_closed()
    }

    fn latest_connection(&self) -> usize {
        self.inner.connections.lock().unwrap().len() - 1
    }

    fn complete_turn(&self, connection: usize, text: &str) {
        self.push(
            connection,
            Ok(RecognizerEvent::SpeechComplete {
                turn_id: "turn-1".to_string(),
                text: text.to_string(),
            }),
        );
    }
}

struct FakeRecognizer {
    receiver: tokio::sync::mpsc::UnboundedReceiver<Result<RecognizerEvent, RecognizerError>>,
    audio: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RecognizerFactory for FakeFactory {
    type Rec = FakeRecognizer;

    async fn connect(&self) -> Result<Self::Rec, RecognizerError> {
        self.inner.attempts.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.inner.connect_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let gate = self.inner.connect_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.inner.connections.lock().unwrap().push(sender);
        Ok(FakeRecognizer {
            receiver,
            audio: Arc::clone(&self.inner.audio),
        })
    }
}

impl Recognizer for FakeRecognizer {
    async fn send_audio(&mut self, audio: &[u8]) -> Result<(), RecognizerError> {
        self.audio.lock().unwrap().push(audio.to_vec());
        Ok(())
    }

    async fn next_event(&mut self) -> Option<Result<RecognizerEvent, RecognizerError>> {
        match self.receiver.recv().await {
            Some(item) => Some(item),
            None => Some(Err(RecognizerError::Retryable(
                "fake recognizer closed".to_string(),
            ))),
        }
    }

    async fn close(&mut self) {}
}

async fn recv_with_paused_clock(client: &mut ClientSocket) -> Received {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(message) = client.next().now_or_never() {
            let message = message.expect("client socket closed").unwrap();
            return match message {
                ClientMessage::Text(text) => Received::Json(serde_json::from_str(&text).unwrap()),
                ClientMessage::Binary(bytes) => Received::Audio(bytes),
                other => panic!("unexpected client message: {other:?}"),
            };
        }
        assert!(
            std::time::Instant::now() < deadline,
            "client message timed out"
        );
        tokio::task::yield_now().await;
    }
}

async fn collect_with_paused_clock(
    client: &mut ClientSocket,
    stop: impl Fn(&Received) -> bool,
) -> Vec<Received> {
    let mut received = Vec::new();
    loop {
        let item = recv_with_paused_clock(client).await;
        let done = stop(&item);
        received.push(item);
        if done {
            return received;
        }
    }
}

async fn voice_on_with_paused_clock(harness: &mut Harness) {
    send_voice(harness, true).await;
    let received = collect_with_paused_clock(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert!(has_state(&received, "starting"));
}

async fn fatal_recognizer_error_keeps_text_fallback() {
    let mut harness = start().await;
    tokio::time::pause();
    voice_on_with_paused_clock(&mut harness).await;
    harness.factory.push(
        0,
        Err(RecognizerError::Fatal(
            "invalid request (code 1008)".to_string(),
        )),
    );
    let received = collect_with_paused_clock(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert!(received.iter().any(|item| matches!(item,
        Received::Json(v) if v["type"] == "error"
            && v["message"].as_str().is_some_and(|message| message.contains("won't retry"))
    )));
    assert!(received.iter().any(|item| matches!(item,
        Received::Json(v) if v["type"] == "input" && v["source"] == "text" && v["ready"] == true
    )));
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    expect_quiet_until_marker(&mut harness).await;
    assert_eq!(harness.factory.attempts(), 1);
    harness.server.abort();
}

/// Reads up to the loss's mic-not-ready message and checks the reply's state was left alone.
async fn keep_reply_state_through_loss(client: &mut ClientSocket, received: &mut Vec<Received>) {
    received.extend(
        collect_with_paused_clock(client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "input" && v["source"] == "mic" && v["ready"] == false)
        })
        .await,
    );
    assert!(
        !received
            .iter()
            .any(|item| matches!(item, Received::Json(v) if v["type"] == "state")),
        "a recognizer loss must not replace the reply state: {received:?}"
    );
}

fn assert_retrying_follows_reply_end(received: &[Received]) {
    let position = |wanted: &dyn Fn(&Value) -> bool| {
        received
            .iter()
            .position(|item| matches!(item, Received::Json(v) if wanted(v)))
    };
    let reply_end = position(&|v| v["type"] == "reply_end");
    let retrying = position(&|v| v["type"] == "state" && v["state"] == "recognizer_retrying");
    assert!(
        matches!((reply_end, retrying), (Some(end), Some(state)) if end < state),
        "the retrying state waits for the reply to end: {received:?}"
    );
}

#[tokio::test]
async fn recognizer_outages_report_one_plain_notice_and_retry_until_ready() {
    const RETRY_NOTICE: &str = "Can't reach speech recognition. Trying again…";
    const RAW_ERROR: &str =
        "recognizer connect failed: IO error: ws://127.0.0.1:9 Connection refused (os error 111)";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Outage {
        FirstConnect,
        Ready,
        Reply,
    }

    fatal_recognizer_error_keeps_text_fallback().await;
    for outage in [Outage::FirstConnect, Outage::Ready, Outage::Reply] {
        let mut harness = start().await;
        tokio::time::pause();
        if outage == Outage::FirstConnect {
            harness.factory.fail_connects(2, RAW_ERROR);
            send_voice(&mut harness, true).await;
        } else {
            voice_on_with_paused_clock(&mut harness).await;
            harness.factory.fail_connects(2, RAW_ERROR);
        }

        let reply_gate = Arc::new(Notify::new());
        if outage == Outage::Reply {
            harness
                .model
                .add_script(vec![ModelChunk::Wait(Arc::clone(&reply_gate))]);
            harness.factory.complete_turn(0, "question");
            let _ = collect_with_paused_clock(&mut harness.client, |item| {
                matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "thinking")
            })
            .await;
        }

        if outage != Outage::FirstConnect {
            harness
                .factory
                .push(0, Err(RecognizerError::Retryable(RAW_ERROR.to_string())));
        }

        let mut received = collect_with_paused_clock(
            &mut harness.client,
            |item| matches!(item, Received::Json(v) if v["type"] == "error"),
        )
        .await;
        let last_error = received.iter().rev().find_map(|item| match item {
            Received::Json(v) if v["type"] == "error" => v["message"].as_str(),
            Received::Json(_) | Received::Audio(_) => None,
        });
        assert_eq!(last_error, Some(RETRY_NOTICE), "{outage:?}: {received:?}");
        if outage == Outage::Reply {
            keep_reply_state_through_loss(&mut harness.client, &mut received).await;
            reply_gate.notify_one();
        }
        received.extend(
            collect_with_paused_clock(&mut harness.client, |item| {
                matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "recognizer_retrying")
            })
            .await,
        );
        assert!(has_state(&received, "recognizer_retrying"), "{outage:?}");
        if outage == Outage::Reply {
            assert_retrying_follows_reply_end(&received);
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while harness.factory.attempts() < 3 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
            assert!(
                std::time::Instant::now() < deadline,
                "{outage:?}: reconnect timed out"
            );
        }
        tokio::time::resume();
        received.extend(
            collect_until(&mut harness.client, |item| {
                matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
            })
            .await,
        );
        let notices: Vec<&str> = received
            .iter()
            .filter_map(|item| match item {
                Received::Json(v) if v["type"] == "error" => v["message"].as_str(),
                Received::Json(_) | Received::Audio(_) => None,
            })
            .collect();
        assert_eq!(notices, vec![RETRY_NOTICE], "{outage:?}: {received:?}");
        for item in &received {
            let Received::Json(value) = item else {
                continue;
            };
            assert!(
                !value.to_string().contains(RAW_ERROR),
                "{outage:?}: {value}"
            );
        }
        expect_quiet_until_marker(&mut harness).await;
        harness.server.abort();
    }
}

// ---------- fake model ----------

enum ModelChunk {
    Delta(String),
    Heard(String),
    Tool(ToolCall),
    Wait(Arc<Notify>),
    Fail,
}

struct DropGuard(Arc<AtomicUsize>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// What the model was offered and shown on one call.
#[derive(Clone)]
struct ToolRound {
    offered: Vec<String>,
    results: Vec<ToolResult>,
}

#[derive(Clone, Default)]
struct FakeModel {
    calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    clips: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
    scripts: Arc<Mutex<VecDeque<Vec<ModelChunk>>>>,
    dropped: Arc<AtomicUsize>,
    /// The tool names offered and the tool results shown on each call.
    tool_rounds: Arc<Mutex<Vec<ToolRound>>>,
    /// What each reply call was told about the briefing.
    briefings: Arc<Mutex<Vec<BriefingContext>>>,
}

impl FakeModel {
    fn add_script(&self, chunks: Vec<ModelChunk>) {
        self.scripts.lock().unwrap().push_back(chunks);
    }

    fn calls(&self) -> Vec<Vec<ChatMessage>> {
        self.calls.lock().unwrap().clone()
    }

    /// The audio each call was given for the last user message.
    fn clips(&self) -> Vec<Option<Vec<u8>>> {
        self.clips.lock().unwrap().clone()
    }

    fn tool_rounds(&self) -> Vec<ToolRound> {
        self.tool_rounds.lock().unwrap().clone()
    }

    /// Each reply call's briefing, as described by [`describe_briefing`].
    fn briefings(&self) -> Vec<String> {
        self.briefings
            .lock()
            .unwrap()
            .iter()
            .map(describe_briefing)
            .collect()
    }

    fn next_events(&self, history: &[ChatMessage], audio: Option<&[u8]>) -> EventStream {
        self.calls.lock().unwrap().push(history.to_vec());
        self.clips.lock().unwrap().push(audio.map(<[u8]>::to_vec));
        let chunks = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        let guard = DropGuard(Arc::clone(&self.dropped));
        let stream = futures_util::stream::unfold(
            (VecDeque::from(chunks), Some(guard)),
            |(mut chunks, guard)| async move {
                match chunks.pop_front() {
                    Some(ModelChunk::Delta(text)) => {
                        Some((Ok(ChatEvent::Text(text)), (chunks, guard)))
                    }
                    Some(ModelChunk::Heard(text)) => {
                        Some((Ok(ChatEvent::Heard(text)), (chunks, guard)))
                    }
                    Some(ModelChunk::Tool(call)) => {
                        Some((Ok(ChatEvent::ToolCall(call)), (chunks, guard)))
                    }
                    Some(ModelChunk::Wait(notify)) => {
                        notify.notified().await;
                        Some((Ok(ChatEvent::Text(String::new())), (chunks, guard)))
                    }
                    Some(ModelChunk::Fail) => Some((
                        Err(ModelError("model blew up".to_string())),
                        (chunks, guard),
                    )),
                    None => None,
                }
            },
        )
        .filter(|item| {
            std::future::ready(!matches!(item, Ok(ChatEvent::Text(text)) if text.is_empty()))
        });
        Box::pin(stream)
    }
}

impl ChatModel for FakeModel {
    async fn stream_reply(
        &self,
        history: &[ChatMessage],
        audio: Option<&[u8]>,
    ) -> Result<DeltaStream, ModelError> {
        let events = self.next_events(history, audio);
        Ok(Box::pin(events.filter_map(|event| {
            std::future::ready(match event {
                Ok(ChatEvent::Text(text)) => Some(Ok(text)),
                Ok(ChatEvent::Heard(_) | ChatEvent::ToolCall(_)) => None,
                Err(error) => Some(Err(error)),
            })
        })))
    }

    async fn stream_reply_with_tools(
        &self,
        history: &[ChatMessage],
        audio: Option<&[u8]>,
        tools: &[ToolDefinition],
        tool_results: &[ToolResult],
        briefing: &BriefingContext,
    ) -> Result<EventStream, ModelError> {
        self.briefings.lock().unwrap().push(briefing.clone());
        self.tool_rounds.lock().unwrap().push(ToolRound {
            offered: tools.iter().map(|tool| tool.name.clone()).collect(),
            results: tool_results.to_vec(),
        });
        Ok(self.next_events(history, audio))
    }
}

// ---------- fake TTS ----------

enum TtsScript {
    Audio,
    Fail,
    Hang,
    /// Audio that only arrives once the gate opens.
    Gated(Arc<Notify>),
}

#[derive(Clone, Default)]
struct FakeTts {
    inputs: Arc<Mutex<Vec<String>>>,
    voices: Arc<Mutex<Vec<String>>>,
    scripts: Arc<Mutex<VecDeque<TtsScript>>>,
    dropped: Arc<AtomicUsize>,
}

impl FakeTts {
    fn add_script(&self, script: TtsScript) {
        self.scripts.lock().unwrap().push_back(script);
    }

    fn inputs(&self) -> Vec<String> {
        self.inputs.lock().unwrap().clone()
    }

    /// The voice of each synthesis request, in order.
    fn voices(&self) -> Vec<String> {
        self.voices.lock().unwrap().clone()
    }
}

impl Tts for FakeTts {
    async fn synthesize_with_voice(
        &self,
        text: &str,
        voice: &str,
    ) -> Result<AudioStream, TtsError> {
        self.voices.lock().unwrap().push(voice.to_string());
        self.synthesize(text).await
    }

    async fn synthesize(&self, text: &str) -> Result<AudioStream, TtsError> {
        self.inputs.lock().unwrap().push(text.to_string());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(TtsScript::Audio);
        match script {
            TtsScript::Audio => {
                let bytes = Bytes::from(format!("AUDIO:{text}"));
                Ok(Box::pin(futures_util::stream::once(
                    async move { Ok(bytes) },
                )))
            }
            TtsScript::Fail => Err(TtsError("tts blew up".to_string())),
            TtsScript::Gated(gate) => {
                let guard = DropGuard(Arc::clone(&self.dropped));
                let bytes = Bytes::from(format!("AUDIO:{text}"));
                Ok(Box::pin(futures_util::stream::once(async move {
                    let _guard = guard;
                    gate.notified().await;
                    Ok(bytes)
                })))
            }
            TtsScript::Hang => {
                let guard = DropGuard(Arc::clone(&self.dropped));
                let stream = futures_util::stream::unfold(Some(guard), |_guard| async move {
                    std::future::pending::<Option<(Result<Bytes, TtsError>, Option<DropGuard>)>>()
                        .await
                });
                Ok(Box::pin(stream))
            }
        }
    }
}

// ---------- harness ----------

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    client: ClientSocket,
    session_id: String,
    factory: FakeFactory,
    model: FakeModel,
    tts: FakeTts,
    store: Arc<SessionStore>,
    /// Sessions that have run to the end, saved and released.
    closed: Arc<AtomicUsize>,
    /// Holds the saved conversations; removed on drop.
    dir: ScratchDir,
    addr: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn reconnect(&mut self, resume: Option<&str>) -> Value {
        let url = resume.map_or_else(
            || format!("ws://{}/session", self.addr),
            |id| format!("ws://{}/session?resume={id}", self.addr),
        );
        let (mut client, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let (session_id, turns) = read_session_opening(&mut client).await;
        self.client = client;
        self.session_id = session_id;
        turns
    }
}

async fn read_session_opening(client: &mut ClientSocket) -> (String, Value) {
    let session = read_opening_message(client).await;
    let id = session["id"].as_str().unwrap().to_string();
    let turns = session["turns"].clone();
    (id, turns)
}

/// The `session` message that opens a connection; the idle state that follows is consumed.
async fn read_opening_message(client: &mut ClientSocket) -> Value {
    let first = recv(client).await;
    let Received::Json(session) = &first else {
        panic!("first message was not the session message: {first:?}");
    };
    assert_eq!(session["type"], "session");
    let second = recv(client).await;
    assert!(
        matches!(&second, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
        "second message was not idle state: {second:?}"
    );
    session.clone()
}

async fn start() -> Harness {
    start_with_recording(RecordingConfig::disabled()).await
}

async fn start_with_recording(recording: RecordingConfig) -> Harness {
    start_with(recording, None).await
}

/// A background title model the test can script, alongside the title config.
fn title_model() -> (FakeModel, Arc<Titler<FakeModel>>) {
    let model = FakeModel::default();
    let titler = Arc::new(Titler {
        model: model.clone(),
        name: "fake-title-model".to_string(),
    });
    (model, titler)
}

async fn start_with(recording: RecordingConfig, titler: Option<Arc<Titler<FakeModel>>>) -> Harness {
    start_full(recording, titler, false, false, None).await
}

async fn start_with_ack(
    recording: RecordingConfig,
    titler: Option<Arc<Titler<FakeModel>>>,
    ack_enabled: bool,
) -> Harness {
    start_full(recording, titler, ack_enabled, false, None).await
}

#[expect(
    clippy::too_many_lines,
    reason = "the harness wires every fake into one server"
)]
async fn start_full(
    recording: RecordingConfig,
    titler: Option<Arc<Titler<FakeModel>>>,
    ack_enabled: bool,
    audio_input: bool,
    briefing: Option<Arc<Briefing>>,
) -> Harness {
    let factory = FakeFactory::default();
    let model = FakeModel::default();
    let tts = FakeTts::default();
    let dir = ScratchDir::new("sessions");
    let store = Arc::new(SessionStore::open(&dir.0.join("sessions")).unwrap());
    let closed = Arc::new(AtomicUsize::new(0));
    let (factory2, model2, tts2, store2, closed2) = (
        factory.clone(),
        model.clone(),
        tts.clone(),
        Arc::clone(&store),
        Arc::clone(&closed),
    );
    let recording2 = recording.clone();
    let app = Router::new()
        .route(
            "/session",
            get(
                move |upgrade: WebSocketUpgrade,
                      axum::extract::Query(query): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| {
                    let (factory, model, tts, store, closed, titler, briefing) = (
                        factory2.clone(),
                        model2.clone(),
                        tts2.clone(),
                        Arc::clone(&store2),
                        Arc::clone(&closed2),
                        titler.clone(),
                        briefing.clone(),
                    );
                    let recording = recording2.clone();
                    async move {
                        let private = query.get("private").is_some_and(|value| value == "1");
                        let resume = query.get("resume").cloned();
                        let retention = Retention::from_request(
                            private,
                            resume,
                            store,
                            titler,
                            briefing,
                            || {
                                static NEXT_PRIVATE: AtomicUsize = AtomicUsize::new(1);
                                format!(
                                    "{PRIVATE_ID_PREFIX}{}",
                                    NEXT_PRIVATE.fetch_add(1, Ordering::SeqCst)
                                )
                            },
                        );
                        upgrade.on_upgrade(move |socket: WebSocket| async move {
                            let Some(retention) = retention else {
                                refuse_private_resume(socket).await;
                                closed.fetch_add(1, Ordering::SeqCst);
                                return;
                            };
                            handle_session(
                                socket,
                                factory,
                                Arc::new(model),
                                Arc::new(tts),
                                retention,
                                recording,
                                ack_enabled,
                                audio_input,
                                VoiceChoices {
                                    default: "af_heart".to_string(),
                                    offered: ["af_heart".to_string(), "sohee".to_string()].into(),
                                },
                            )
                            .await;
                            closed.fetch_add(1, Ordering::SeqCst);
                        })
                    }
                },
            ),
        )
        .merge(api::routes().with_state(Arc::clone(&store)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/session"))
        .await
        .unwrap();
    let (session_id, turns) = read_session_opening(&mut client).await;
    assert_eq!(turns, json!([]));
    Harness {
        client,
        session_id,
        factory,
        model,
        tts,
        store,
        closed,
        dir,
        addr,
        server,
    }
}

/// What the web client's one-click Voice control sends: both choices together.
async fn send_voice(harness: &mut Harness, on: bool) {
    let source = if on { "mic" } else { "text" };
    send_json(harness, &json!({"type": "input", "source": source})).await;
    send_json(harness, &json!({"type": "reply_audio", "on": on})).await;
}

async fn voice_on(harness: &mut Harness) {
    send_voice(harness, true).await;
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert!(
        has_state(&received, "starting"),
        "no starting state before listening: {received:?}"
    );
}

async fn send_json(harness: &mut Harness, value: &Value) {
    harness
        .client
        .send(ClientMessage::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn wait_until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition not reached within 5s");
}

/// Waits until `count` sessions have fully ended, final save and recording flush included.
async fn wait_closed(harness: &Harness, count: usize) {
    wait_until(|| harness.closed.load(Ordering::SeqCst) >= count).await;
}

#[derive(Debug, PartialEq)]
enum Received {
    Json(Value),
    Audio(Bytes),
}

async fn recv(client: &mut ClientSocket) -> Received {
    let message = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("timed out waiting for a client message")
        .expect("client socket closed")
        .unwrap();
    match message {
        ClientMessage::Text(text) => Received::Json(serde_json::from_str(&text).unwrap()),
        ClientMessage::Binary(bytes) => Received::Audio(bytes),
        other => panic!("unexpected client message: {other:?}"),
    }
}

async fn collect_until(
    client: &mut ClientSocket,
    stop: impl Fn(&Received) -> bool,
) -> Vec<Received> {
    let mut received = Vec::new();
    loop {
        let item = recv(client).await;
        let done = stop(&item);
        received.push(item);
        if done {
            return received;
        }
    }
}

async fn next_resting_state(client: &mut ClientSocket) -> String {
    loop {
        if let Received::Json(v) = recv(client).await
            && v["type"] == "state"
            && let Some(state @ ("idle" | "starting" | "listening")) = v["state"].as_str()
        {
            return state.to_string();
        }
    }
}

/// An empty text message is answered with an error, which proves everything
/// the server sent before it has already arrived.
async fn expect_quiet_until_marker(harness: &mut Harness) {
    send_json(harness, &json!({"type": "text", "text": ""})).await;
    let marker = recv(&mut harness.client).await;
    assert!(
        matches!(&marker, Received::Json(v) if v["type"] == "error"),
        "expected only the marker error, got {marker:?}"
    );
}

fn has_state(received: &[Received], state: &str) -> bool {
    received.iter().any(
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == state),
    )
}

fn has_type(received: &[Received], kind: &str) -> bool {
    received
        .iter()
        .any(|item| matches!(item, Received::Json(v) if v["type"] == kind))
}

fn reply_text(received: &[Received]) -> String {
    received
        .iter()
        .filter_map(|item| match item {
            Received::Json(v) if v["type"] == "reply" => v["text"].as_str().map(str::to_string),
            Received::Json(_) | Received::Audio(_) => None,
        })
        .collect()
}

fn audios(received: &[Received]) -> Vec<Bytes> {
    received
        .iter()
        .filter_map(|item| match item {
            Received::Audio(bytes) => Some(bytes.clone()),
            Received::Json(_) => None,
        })
        .collect()
}

// ---------- tests ----------

#[tokio::test]
async fn completed_turn_streams_first_sentence_audio_while_model_is_still_open() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    let gate = Arc::new(Notify::new());
    harness.model.add_script(vec![
        ModelChunk::Delta("Hello there. ".to_string()),
        ModelChunk::Wait(Arc::clone(&gate)),
        ModelChunk::Delta("How are you?".to_string()),
    ]);
    harness.factory.push(
        0,
        Ok(RecognizerEvent::Partial {
            text: "hel".to_string(),
        }),
    );
    harness.factory.complete_turn(0, "hello thalvi");

    let mut received = vec![recv(&mut harness.client).await];
    received.extend(
        collect_until(&mut harness.client, |item| {
            matches!(item, Received::Audio(_))
        })
        .await,
    );
    assert!(received.iter().any(
        |item| matches!(item, Received::Json(v) if v["type"] == "partial" && v["text"] == "hel")
    ));
    assert!(received
        .iter()
        .any(|item| matches!(item, Received::Json(v) if v["type"] == "user" && v["text"] == "hello thalvi")));
    assert!(has_state(&received, "thinking"));
    assert_eq!(reply_text(&received), "Hello there. ");
    assert_eq!(audios(&received), vec![Bytes::from("AUDIO:Hello there.")]);
    assert!(
        !has_state(&received, "listening"),
        "reply finished before the gate opened: {received:?}"
    );

    gate.notify_one();
    let rest = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert_eq!(reply_text(&rest), "How are you?");
    assert_eq!(audios(&rest), vec![Bytes::from("AUDIO:How are you?")]);
    assert!(
        has_type(&rest, "reply_end"),
        "finished reply must end with reply_end: {rest:?}"
    );
    harness.server.abort();
}

#[tokio::test]
async fn speech_start_and_cancel_each_interrupt_the_reply() {
    for use_cancel in [false, true] {
        let mut harness = start().await;
        voice_on(&mut harness).await;
        harness.model.add_script(vec![ModelChunk::Delta(
            "First sentence. Second sentence.".to_string(),
        )]);
        harness.tts.add_script(TtsScript::Audio);
        harness.tts.add_script(TtsScript::Hang);
        harness.factory.complete_turn(0, "question");
        let received = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Audio(_))
        })
        .await;
        assert_eq!(
            audios(&received),
            vec![Bytes::from("AUDIO:First sentence.")]
        );
        wait_until(|| harness.tts.inputs().len() == 2).await;

        if use_cancel {
            harness
                .client
                .send(ClientMessage::Text(r#"{"type":"cancel"}"#.into()))
                .await
                .unwrap();
        } else {
            harness.factory.push(
                0,
                Ok(RecognizerEvent::SpeechStart {
                    turn_id: "turn-1".to_string(),
                    audio_processed_ms: None,
                }),
            );
        }
        let received = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
        })
        .await;
        assert!(
            has_type(&received, "interrupted"),
            "use_cancel={use_cancel}: {received:?}"
        );
        assert_eq!(audios(&received), Vec::<Bytes>::new());
        wait_until(|| harness.tts.dropped.load(Ordering::SeqCst) >= 1).await;

        expect_quiet_until_marker(&mut harness).await;
        harness.server.abort();
    }
}

#[tokio::test]
async fn model_and_tts_failures_report_an_error_and_the_next_turn_works() {
    for fail_tts in [false, true] {
        let mut harness = start().await;
        voice_on(&mut harness).await;
        if fail_tts {
            harness
                .model
                .add_script(vec![ModelChunk::Delta("Hello there.".to_string())]);
            harness.tts.add_script(TtsScript::Fail);
        } else {
            harness.model.add_script(vec![ModelChunk::Fail]);
        }
        harness.factory.complete_turn(0, "first");
        let received = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
        })
        .await;
        assert!(
            has_type(&received, "error"),
            "fail_tts={fail_tts}: {received:?}"
        );

        harness
            .model
            .add_script(vec![ModelChunk::Delta("Recovered nicely.".to_string())]);
        harness.factory.complete_turn(0, "second");
        let received = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
        })
        .await;
        assert_eq!(reply_text(&received), "Recovered nicely.");
        assert_eq!(
            audios(&received),
            vec![Bytes::from("AUDIO:Recovered nicely.")]
        );
        harness.server.abort();
    }
}

#[tokio::test]
async fn recognizer_loss_reports_reconnects_and_later_turns_work() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    harness.factory.push(
        0,
        Err(RecognizerError::Retryable("connection dropped".to_string())),
    );
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "error"),
    )
    .await;
    assert!(has_type(&received, "error"));
    wait_until(|| harness.factory.attempts() >= 2).await;
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert!(has_state(&received, "recognizer_retrying"));

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Back again.".to_string())]);
    harness.factory.complete_turn(1, "after reconnect");
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert_eq!(audios(&received), vec![Bytes::from("AUDIO:Back again.")]);
    harness.server.abort();
}

#[tokio::test]
async fn fatal_recognizer_error_is_not_retried() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    harness.factory.push(
        0,
        Err(RecognizerError::Fatal(
            "invalid request (code 1008)".to_string(),
        )),
    );
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "error"),
    )
    .await;
    assert!(has_type(&received, "error"));
    let input_text = recv(&mut harness.client).await;
    assert!(
        matches!(&input_text, Received::Json(v) if v["type"] == "input" && v["source"] == "text" && v["ready"] == true),
        "a fatal recognizer error tells the client input is back to text, got {input_text:?}"
    );
    let after_error = recv(&mut harness.client).await;
    assert!(
        matches!(&after_error, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
        "a fatal recognizer error turns voice off and reports idle, got {after_error:?}"
    );

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    expect_quiet_until_marker(&mut harness).await;
    assert_eq!(
        harness.factory.attempts(),
        1,
        "fatal recognizer error must not reconnect"
    );

    voice_on(&mut harness).await;
    assert_eq!(harness.factory.attempts(), 2);
    harness.server.abort();
}

#[tokio::test]
async fn provider_errors_reach_the_user_but_not_the_log() {
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    for (failing, provider_text, logged) in [
        (
            "recognizer",
            "PROVIDER_ECHO_of_what_the_user_said",
            "recognizer disconnected",
        ),
        ("model", "model blew up", "reply failed"),
        ("speech", "tts blew up", "reply failed"),
    ] {
        let log = CapturedLog::default();
        let writer = log.clone();
        let _logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let mut harness = start().await;
        voice_on(&mut harness).await;
        match failing {
            "recognizer" => harness
                .factory
                .push(0, Err(RecognizerError::Fatal(provider_text.to_string()))),
            "model" => {
                harness.model.add_script(vec![ModelChunk::Fail]);
                harness.factory.complete_turn(0, "a question");
            }
            _ => {
                harness
                    .model
                    .add_script(vec![ModelChunk::Delta("Hello there.".to_string())]);
                harness.tts.add_script(TtsScript::Fail);
                harness.factory.complete_turn(0, "a question");
            }
        }
        let received = collect_until(
            &mut harness.client,
            |item| matches!(item, Received::Json(v) if v["type"] == "error"),
        )
        .await;
        assert!(
            received.iter().any(|item| matches!(item, Received::Json(v) if v["message"].as_str().is_some_and(|m| m.contains(provider_text)))),
            "{failing}: {received:?}"
        );
        harness.server.abort();
        let log = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains(logged), "{failing}: {log}");
        assert!(!log.contains(provider_text), "{failing}: {log}");
    }
}

#[tokio::test]
async fn next_turn_history_includes_the_cut_off_reply_text() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    harness.model.add_script(vec![ModelChunk::Delta(
        "Answer one. Answer two is longer. Unfinished tail".to_string(),
    )]);
    harness.tts.add_script(TtsScript::Audio);
    harness.tts.add_script(TtsScript::Hang);
    harness.factory.complete_turn(0, "first question");
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    wait_until(|| harness.tts.inputs().len() == 2).await;
    harness
        .client
        .send(ClientMessage::Text(r#"{"type":"cancel"}"#.into()))
        .await
        .unwrap();
    let _ = collect_until(&mut harness.client, |item| {
        has_type(std::slice::from_ref(item), "interrupted")
    })
    .await;

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Follow up.".to_string())]);
    harness.factory.complete_turn(0, "second question");
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;

    let calls = harness.model.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1],
        vec![
            ChatMessage::user("first question".to_string()),
            ChatMessage::assistant("Answer one. Answer two is longer.".to_string()),
            ChatMessage::user("second question".to_string()),
        ]
    );
    harness.server.abort();
}

#[tokio::test]
async fn audio_before_recognizer_ready_is_discarded_and_state_goes_starting_then_listening() {
    let mut harness = start().await;
    let gate = Arc::new(Notify::new());
    harness.factory.hold_connect(Arc::clone(&gate));
    send_voice(&mut harness, true).await;
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "starting")
    })
    .await;
    assert!(has_state(&received, "starting"));
    assert!(!has_state(&received, "listening"));
    let _ = collect_until(&mut harness.client, |item| {
        has_type(std::slice::from_ref(item), "reply_audio")
    })
    .await;

    for _ in 0..3 {
        harness
            .client
            .send(ClientMessage::Binary(vec![7_u8; 3840].into()))
            .await
            .unwrap();
    }
    // The error reply proves the server has processed the frames before it
    // on the same socket, so they were received while not ready.
    send_json(&mut harness, &json!({"type": "text", "text": ""})).await;
    let marker = recv(&mut harness.client).await;
    assert!(
        matches!(&marker, Received::Json(v) if v["type"] == "error"),
        "expected the marker error, got {marker:?}"
    );
    gate.notify_one();
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert!(has_state(&received, "listening"));
    assert_eq!(
        harness.factory.audio_received(),
        Vec::<Vec<u8>>::new(),
        "audio sent before the recognizer was ready must never reach it"
    );

    harness
        .client
        .send(ClientMessage::Binary(vec![9_u8; 3840].into()))
        .await
        .unwrap();
    wait_until(|| harness.factory.audio_received().len() == 1).await;
    assert_eq!(harness.factory.audio_received()[0], vec![9_u8; 3840]);
    harness.server.abort();
}

#[tokio::test]
async fn mic_frames_that_are_not_bounded_pcm_never_reach_the_recognizer() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    for frame in [vec![], vec![1_u8; 3], vec![1_u8; 48_002], vec![2_u8; 3840]] {
        harness
            .client
            .send(ClientMessage::Binary(frame.into()))
            .await
            .unwrap();
    }
    wait_until(|| !harness.factory.audio_received().is_empty()).await;
    assert_eq!(harness.factory.audio_received(), vec![vec![2_u8; 3840]]);
    harness.server.abort();
}

#[tokio::test]
async fn text_turn_is_answered_and_interrupts_a_reply_without_voice() {
    let mut harness = start().await;
    let gate = Arc::new(Notify::new());
    harness.model.add_script(vec![
        ModelChunk::Delta("First reply. ".to_string()),
        ModelChunk::Wait(Arc::clone(&gate)),
    ]);
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Second answer.".to_string())]);

    send_json(
        &mut harness,
        &json!({"type": "text", "text": "  first question  "}),
    )
    .await;
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply"),
    )
    .await;
    assert!(received
        .iter()
        .any(|item| matches!(item, Received::Json(v) if v["type"] == "user" && v["text"] == "first question")));
    assert!(has_state(&received, "thinking"));

    send_json(
        &mut harness,
        &json!({"type": "text", "text": "second question"}),
    )
    .await;
    let interrupted = recv(&mut harness.client).await;
    assert!(
        matches!(&interrupted, Received::Json(v) if v["type"] == "interrupted"),
        "expected interrupted first, got {interrupted:?}"
    );
    let after = recv(&mut harness.client).await;
    assert!(
        matches!(&after, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
        "interrupted must be followed by a state message, got {after:?}"
    );
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert!(received
        .iter()
        .any(|item| matches!(item, Received::Json(v) if v["type"] == "user" && v["text"] == "second question")));
    assert_eq!(reply_text(&received), "Second answer.");
    assert!(has_type(&received, "reply_end"));
    assert!(!has_state(&received, "listening"));

    assert_eq!(
        harness.factory.attempts(),
        0,
        "voice off must not connect the recognizer"
    );
    assert_eq!(
        harness.tts.inputs(),
        Vec::<String>::new(),
        "voice off must not synthesize"
    );
    let calls = harness.model.calls();
    assert_eq!(
        calls[1],
        vec![
            ChatMessage::user("first question".to_string()),
            ChatMessage::assistant("First reply.".to_string()),
            ChatMessage::user("second question".to_string()),
        ]
    );
    harness.server.abort();
}

#[tokio::test]
async fn turning_voice_off_mid_reply_stops_synthesis_while_text_continues() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    let gate = Arc::new(Notify::new());
    harness.model.add_script(vec![
        ModelChunk::Delta("One. ".to_string()),
        ModelChunk::Wait(Arc::clone(&gate)),
        ModelChunk::Delta("Two. Three.".to_string()),
    ]);
    harness.factory.complete_turn(0, "question");
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    assert_eq!(audios(&received), vec![Bytes::from("AUDIO:One.")]);

    send_voice(&mut harness, false).await;
    let muted = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_audio"),
    )
    .await;
    assert!(
        matches!(muted.last(), Some(Received::Json(v)) if v["on"] == false),
        "expected the reply audio off event, got {muted:?}"
    );
    gate.notify_one();
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert_eq!(reply_text(&received), "Two. Three.");
    assert_eq!(audios(&received), Vec::<Bytes>::new());
    assert_eq!(harness.tts.inputs(), vec!["One.".to_string()]);

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Fine.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "next"})).await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert_eq!(
        harness.model.calls()[1],
        vec![
            ChatMessage::user("question".to_string()),
            ChatMessage::assistant("One. Two. Three.".to_string()),
            ChatMessage::user("next".to_string()),
        ],
        "a finished reply is remembered in full even though only its first sentence was spoken"
    );
    harness.server.abort();
}

#[tokio::test]
async fn rejoining_restores_history_and_unknown_ids_start_fresh() {
    let mut harness = start().await;
    let first_id = harness.session_id.clone();
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Noted.".to_string())]);
    send_json(
        &mut harness,
        &json!({"type": "text", "text": "remember this"}),
    )
    .await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;

    let turns = harness.reconnect(Some(&first_id)).await;
    assert_eq!(harness.session_id, first_id);
    assert_eq!(
        turns,
        json!([
            {"role": "user", "text": "remember this"},
            {"role": "assistant", "text": "Noted."},
        ])
    );

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Continuing.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "go on"})).await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    let calls = harness.model.calls();
    assert_eq!(
        calls[1],
        vec![
            ChatMessage::user("remember this".to_string()),
            ChatMessage::assistant("Noted.".to_string()),
            ChatMessage::user("go on".to_string()),
        ]
    );

    let turns = harness.reconnect(Some("no-such-session")).await;
    assert_eq!(turns, json!([]));
    assert_ne!(harness.session_id, "no-such-session");
    harness.server.abort();
}

#[tokio::test]
async fn empty_or_oversized_text_is_an_error_and_is_ignored() {
    let mut harness = start().await;
    for text in [String::new(), "   ".to_string(), "x".repeat(20_001)] {
        send_json(&mut harness, &json!({"type": "text", "text": text})).await;
        let received = recv(&mut harness.client).await;
        assert!(
            matches!(&received, Received::Json(v) if v["type"] == "error"),
            "text of {} chars should be an error, got {received:?}",
            text.chars().count()
        );
    }
    assert_eq!(harness.model.calls().len(), 0);

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Ok.".to_string())]);
    send_json(
        &mut harness,
        &json!({"type": "text", "text": "a".repeat(20_000)}),
    )
    .await;
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert_eq!(reply_text(&received), "Ok.");
    harness.server.abort();
}

/// A unique scratch directory that is removed when dropped.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let process = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self(std::env::temp_dir().join(format!("thalvi-record-test-{tag}-{process}-{nanos}-{id}")))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scratch_directories_with_the_same_tag_are_isolated() {
    let first = ScratchDir::new("isolation");
    let second = ScratchDir::new("isolation");
    std::fs::create_dir_all(&first.0).unwrap();
    std::fs::create_dir_all(&second.0).unwrap();
    std::fs::write(first.0.join("marker"), b"first").unwrap();
    std::fs::write(second.0.join("marker"), b"second").unwrap();
    drop(first);
    assert_eq!(std::fs::read(second.0.join("marker")).unwrap(), b"second");
}

fn recording_config(kinds: RecordKinds, data_dir: &ScratchDir) -> RecordingConfig {
    RecordingConfig {
        kinds,
        data_dir: data_dir.0.clone(),
        stt_model: "fake-stt".to_string(),
        llm_model: "fake-llm".to_string(),
        reasoning_effort: Some("minimal".to_string()),
        tts_model: "fake-tts".to_string(),
        tts_voice: "fake-voice".to_string(),
    }
}

fn find_session_files(data_dir: &std::path::Path, session: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let recordings = data_dir.join("recordings");
    let Ok(dates) = std::fs::read_dir(&recordings) else {
        return found;
    };
    for date in dates.flatten() {
        let jsonl = date.path().join(format!("{session}.jsonl"));
        if jsonl.exists() {
            found.push(jsonl);
        }
        let session_dir = date.path().join(session);
        if let Ok(entries) = std::fs::read_dir(&session_dir) {
            for entry in entries.flatten() {
                found.push(entry.path());
            }
        }
    }
    found
}

async fn run_voice_turn(harness: &mut Harness) {
    voice_on(harness).await;
    let already = harness.factory.audio_received().len();
    for _ in 0..10 {
        harness
            .client
            .send(ClientMessage::Binary(vec![3_u8; 3840].into()))
            .await
            .unwrap();
    }
    wait_until(|| harness.factory.audio_received().len() == already + 10).await;
    let connection = harness.factory.latest_connection();
    harness.factory.push(
        connection,
        Ok(RecognizerEvent::SpeechStart {
            turn_id: "turn-1".to_string(),
            audio_processed_ms: Some(100),
        }),
    );
    harness.factory.push(
        connection,
        Ok(RecognizerEvent::SpeechEnd {
            turn_id: "turn-1".to_string(),
            audio_processed_ms: Some(500),
        }),
    );
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Hello there.".to_string())]);
    harness.factory.complete_turn(connection, "hello thalvi");
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
}

async fn recorded_session(kinds: RecordKinds, tag: &str) -> (ScratchDir, String) {
    let data_dir = ScratchDir::new(tag);
    let config = recording_config(kinds, &data_dir);
    let mut harness = start_with_recording(config).await;
    run_voice_turn(&mut harness).await;
    let session = harness.session_id.clone();
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;
    harness.server.abort();
    (data_dir, session)
}

#[tokio::test]
async fn recording_writes_nothing_metrics_text_or_all_kinds() {
    // Nothing enabled: no files at all.
    let (data_dir, session) = recorded_session(RecordKinds::default(), "none").await;
    assert!(
        find_session_files(&data_dir.0, &session).is_empty(),
        "no recording files should be written when THALVI_RECORD is empty"
    );

    // Metrics only: events present, no text fields, no wav.
    let kinds = RecordKinds {
        metrics: true,
        text: false,
        audio: false,
    };
    let (data_dir, session) = recorded_session(kinds, "metrics").await;
    wait_until(|| {
        std::fs::read_to_string(
            find_session_files(&data_dir.0, &session)
                .into_iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .unwrap_or_default(),
        )
        .is_ok_and(|content| content.contains("session_close"))
    })
    .await;
    let lines = read_recorded_lines_blocking(&data_dir.0, &session);
    assert_events(
        &lines,
        &[
            "session_open",
            "stt_connect",
            "turn",
            "reply",
            "tts_sentence",
            "session_close",
        ],
    );
    assert!(lines.iter().all(|line| line.get("text").is_none()));
    let turn = lines.iter().find(|line| line["event"] == "turn").unwrap();
    assert_eq!(turn["source"], "voice");
    assert_eq!(turn["speech_ms"], 400);
    let connect = lines
        .iter()
        .find(|line| line["event"] == "stt_connect")
        .unwrap();
    assert_eq!(connect["outcome"], "ok");
    assert_eq!(connect["model"], "fake-stt");
    assert!(
        find_session_files(&data_dir.0, &session)
            .iter()
            .all(|path| path.extension().is_none_or(|ext| ext != "wav")),
        "metrics-only recording must not write wav files"
    );

    // Text only: only turn and reply lines, carrying text.
    let kinds = RecordKinds {
        metrics: false,
        text: true,
        audio: false,
    };
    let (data_dir, session) = recorded_session(kinds, "text").await;
    wait_until(|| !read_recorded_lines_blocking(&data_dir.0, &session).is_empty()).await;
    let lines = read_recorded_lines_blocking(&data_dir.0, &session);
    assert_ne!(lines.len(), 0);
    assert!(
        lines
            .iter()
            .all(|line| matches!(line["event"].as_str(), Some("turn" | "reply"))),
        "text-only recording must write only turn and reply events: {lines:?}"
    );
    let turn = lines.iter().find(|line| line["event"] == "turn").unwrap();
    assert_eq!(turn["text"], "hello thalvi");
    let reply = lines.iter().find(|line| line["event"] == "reply").unwrap();
    assert_eq!(reply["text"], "Hello there.");

    // All three: the wav exists and its length matches the cut turn audio.
    let kinds = RecordKinds {
        metrics: true,
        text: true,
        audio: true,
    };
    let (data_dir, session) = recorded_session(kinds, "all").await;
    wait_until(|| {
        find_session_files(&data_dir.0, &session)
            .iter()
            .any(|path| path.extension().is_some_and(|ext| ext == "wav"))
    })
    .await;
    let wav = find_session_files(&data_dir.0, &session)
        .into_iter()
        .find(|path| path.extension().is_some_and(|ext| ext == "wav"))
        .unwrap();
    let wav_len = std::fs::metadata(&wav).unwrap().len();
    assert_eq!(
        wav_len,
        44 + 38_400,
        "100..500 ms speech plus 300 ms padding each side"
    );
    let lines = read_recorded_lines_blocking(&data_dir.0, &session);
    let turn = lines.iter().find(|line| line["event"] == "turn").unwrap();
    assert!(turn["endpoint_lag_ms"].is_number());
    assert_eq!(turn["audio"], "1.wav");
    assert_eq!(turn["text"], "hello thalvi");
}

fn assert_events(lines: &[Value], expected: &[&str]) {
    let events: Vec<&str> = lines
        .iter()
        .filter_map(|line| line["event"].as_str())
        .collect();
    for event in expected {
        assert!(events.contains(event), "missing {event} in {events:?}");
    }
}

fn read_recorded_lines_blocking(data_dir: &std::path::Path, session: &str) -> Vec<Value> {
    let jsonl = find_session_files(data_dir, session)
        .into_iter()
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    let Some(jsonl) = jsonl else {
        return Vec::new();
    };
    std::fs::read_to_string(&jsonl)
        .map(|content| {
            content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn endpoint_lag_is_measured_from_when_the_end_audio_was_forwarded() {
    let kinds = RecordKinds {
        metrics: true,
        text: false,
        audio: false,
    };
    let data_dir = ScratchDir::new("lag");
    let config = recording_config(kinds, &data_dir);
    let mut harness = start_with_recording(config).await;
    voice_on(&mut harness).await;
    for _ in 0..10 {
        harness
            .client
            .send(ClientMessage::Binary(vec![3_u8; 3840].into()))
            .await
            .unwrap();
    }
    wait_until(|| harness.factory.audio_received().len() == 10).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(300)).await;
    let connection = harness.factory.latest_connection();
    harness.factory.push(
        connection,
        Ok(RecognizerEvent::SpeechStart {
            turn_id: "turn-1".to_string(),
            audio_processed_ms: Some(100),
        }),
    );
    harness.factory.push(
        connection,
        Ok(RecognizerEvent::SpeechEnd {
            turn_id: "turn-1".to_string(),
            audio_processed_ms: Some(500),
        }),
    );
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Hello there.".to_string())]);
    harness.factory.complete_turn(connection, "hello thalvi");
    let session = harness.session_id.clone();
    wait_until(|| {
        read_recorded_lines_blocking(&data_dir.0, &session)
            .iter()
            .any(|line| line["event"] == "turn")
    })
    .await;
    tokio::time::resume();
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;
    harness.server.abort();
    let lines = read_recorded_lines_blocking(&data_dir.0, &session);
    let turn = lines.iter().find(|line| line["event"] == "turn").unwrap();
    let lag = turn["endpoint_lag_ms"].as_u64().unwrap();
    assert!(
        lag >= 250,
        "lag should reach back to when the speech-end audio was forwarded, got {lag} ms"
    );
}

#[tokio::test]
async fn resumed_session_continues_turn_numbering_and_keeps_both_wavs() {
    let kinds = RecordKinds {
        metrics: true,
        text: true,
        audio: true,
    };
    let data_dir = ScratchDir::new("resume");
    let config = recording_config(kinds, &data_dir);
    let mut harness = start_with_recording(config).await;
    run_voice_turn(&mut harness).await;
    let session = harness.session_id.clone();
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;

    let turns = harness.reconnect(Some(&session)).await;
    assert_eq!(turns.as_array().map(Vec::len), Some(2));
    run_voice_turn(&mut harness).await;
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 2).await;
    harness.server.abort();

    wait_until(|| {
        let files = find_session_files(&data_dir.0, &session);
        files.iter().any(|path| path.ends_with("1.wav"))
            && files.iter().any(|path| path.ends_with("2.wav"))
    })
    .await;
    let lines = read_recorded_lines_blocking(&data_dir.0, &session);
    let turn_numbers: Vec<u64> = lines
        .iter()
        .filter(|line| line["event"] == "turn")
        .filter_map(|line| line["turn"].as_u64())
        .collect();
    assert_eq!(turn_numbers, vec![1, 2]);
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn operator_log_carries_turn_source_and_length_but_never_the_text() {
    let log = LogBuffer::default();
    let writer = log.clone();
    // Scoped subscribers lose events to callsites other test threads cached as disabled.
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();
    let mut harness = start().await;
    voice_on(&mut harness).await;
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Ok.".to_string())]);
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Ok.".to_string())]);
    send_json(
        &mut harness,
        &json!({"type": "text", "text": "alpha typed secret"}),
    )
    .await;
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    harness.factory.complete_turn(0, "bravo spoken secret");
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    harness.server.abort();

    let logged = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains(r#"source="text" chars=18"#),
        "typed turn not logged by source and length: {logged}"
    );
    assert!(
        logged.contains(r#"source="voice" chars=19"#),
        "spoken turn not logged by source and length: {logged}"
    );
    assert!(
        !logged.contains("secret"),
        "turn text must stay out of the operator log: {logged}"
    );
}

#[tokio::test]
async fn typed_turn_speech_start_and_cancel_interrupt_audio_until_the_client_has_played_it_all() {
    // "AUDIO:One." and "AUDIO:Two." are 10 bytes each; a count of 10 is a report
    // that left the client before the second sentence arrived.
    for (played, interrupts) in [(None, true), (Some(10), true), (Some(20), false)] {
        for trigger in ["typed", "speech start", "cancel"] {
            let case = format!("{trigger} after played {played:?}");
            let mut harness = start().await;
            voice_on(&mut harness).await;
            let gap = Arc::new(Notify::new());
            harness.model.add_script(vec![
                ModelChunk::Delta("One. ".to_string()),
                ModelChunk::Wait(Arc::clone(&gap)),
                ModelChunk::Delta("Two.".to_string()),
            ]);
            harness.factory.complete_turn(0, "first");
            let _ = collect_until(&mut harness.client, |item| {
                matches!(item, Received::Audio(_))
            })
            .await;
            gap.notify_one();
            let received = collect_until(&mut harness.client, |item| {
                matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
            })
            .await;
            assert_eq!(audios(&received), vec![Bytes::from("AUDIO:Two.")]);
            if let Some(bytes) = played {
                send_json(&mut harness, &json!({"type": "played", "bytes": bytes})).await;
                expect_quiet_until_marker(&mut harness).await;
            }

            match trigger {
                "typed" => {
                    harness
                        .model
                        .add_script(vec![ModelChunk::Delta("Next.".to_string())]);
                    send_json(&mut harness, &json!({"type": "text", "text": "second"})).await;
                }
                "speech start" => harness.factory.push(
                    0,
                    Ok(RecognizerEvent::SpeechStart {
                        turn_id: "turn-2".to_string(),
                        audio_processed_ms: None,
                    }),
                ),
                _ => send_json(&mut harness, &json!({"type": "cancel"})).await,
            }

            if !interrupts {
                if trigger == "typed" {
                    let first = recv(&mut harness.client).await;
                    assert!(
                        matches!(&first, Received::Json(v) if v["type"] == "user"),
                        "{case}: a played reply must not be interrupted, got {first:?}"
                    );
                } else {
                    expect_quiet_until_marker(&mut harness).await;
                }
                harness.server.abort();
                continue;
            }
            let interrupted = recv(&mut harness.client).await;
            assert!(
                matches!(&interrupted, Received::Json(v) if v["type"] == "interrupted"),
                "{case}: expected interrupted first, got {interrupted:?}"
            );
            let resting = recv(&mut harness.client).await;
            assert!(
                matches!(&resting, Received::Json(v) if v["type"] == "state" && v["state"] == "listening"),
                "{case}: interrupted must be followed by the resting state, got {resting:?}"
            );

            if trigger == "cancel" {
                send_json(&mut harness, &json!({"type": "cancel"})).await;
                expect_quiet_until_marker(&mut harness).await;
            }
            harness.server.abort();
        }
    }
}

#[tokio::test]
async fn closing_the_socket_mid_reply_keeps_the_text_shown_for_the_rejoin() {
    let mut harness = start().await;
    let first_id = harness.session_id.clone();
    harness.model.add_script(vec![
        ModelChunk::Delta("Partial words. ".to_string()),
        ModelChunk::Wait(Arc::new(Notify::new())),
    ]);
    send_json(&mut harness, &json!({"type": "text", "text": "question"})).await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply"),
    )
    .await;
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;

    let turns = harness.reconnect(Some(&first_id)).await;
    assert_eq!(
        turns,
        json!([
            {"role": "user", "text": "question"},
            {"role": "assistant", "text": "Partial words."},
        ])
    );
    harness.server.abort();
}

#[tokio::test]
async fn cut_off_reply_after_voice_off_keeps_all_the_text_shown() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    let (first_gate, second_gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    harness.model.add_script(vec![
        ModelChunk::Delta("One. ".to_string()),
        ModelChunk::Wait(Arc::clone(&first_gate)),
        ModelChunk::Delta("Two. ".to_string()),
        ModelChunk::Wait(Arc::clone(&second_gate)),
    ]);
    harness.factory.complete_turn(0, "question");
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    send_voice(&mut harness, false).await;
    let muted = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_audio"),
    )
    .await;
    assert!(
        matches!(muted.last(), Some(Received::Json(v)) if v["on"] == false),
        "expected the reply audio off event, got {muted:?}"
    );
    first_gate.notify_one();
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply" && v["text"] == "Two. "),
    )
    .await;
    assert_eq!(audios(&received), Vec::<Bytes>::new());
    send_json(&mut harness, &json!({"type": "cancel"})).await;
    let _ = collect_until(&mut harness.client, |item| {
        has_type(std::slice::from_ref(item), "interrupted")
    })
    .await;

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Fine.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "next"})).await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert_eq!(
        harness.model.calls()[1][1],
        ChatMessage::assistant("One. Two.".to_string()),
        "voice was turned off, so the user read both sentences"
    );
    harness.server.abort();
}

#[tokio::test]
async fn resting_state_waits_for_the_recognizer_after_every_way_a_reply_ends() {
    let mut harness = start().await;
    let connect_gate = Arc::new(Notify::new());
    harness.factory.hold_connect(Arc::clone(&connect_gate));
    send_voice(&mut harness, true).await;
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "starting")
    })
    .await;

    for ending in ["finished", "cancelled", "failed"] {
        harness.model.add_script(match ending {
            "finished" => vec![ModelChunk::Delta("Hi there.".to_string())],
            "cancelled" => vec![ModelChunk::Wait(Arc::new(Notify::new()))],
            _ => vec![ModelChunk::Fail],
        });
        send_json(&mut harness, &json!({"type": "text", "text": ending})).await;
        if ending == "cancelled" {
            let _ = collect_until(&mut harness.client, |item| {
                matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "thinking")
            })
            .await;
            send_json(&mut harness, &json!({"type": "cancel"})).await;
        }
        assert_eq!(
            next_resting_state(&mut harness.client).await,
            "starting",
            "{ending}: the recognizer is still connecting, so the mic is not listening"
        );
    }

    connect_gate.notify_one();
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert!(has_state(&received, "listening"));
    harness.server.abort();
}

#[tokio::test]
async fn turning_voice_on_mid_reply_keeps_the_reply_state_until_it_ends() {
    let mut harness = start().await;
    let model_gate = Arc::new(Notify::new());
    harness
        .model
        .add_script(vec![ModelChunk::Wait(Arc::clone(&model_gate))]);
    send_json(&mut harness, &json!({"type": "text", "text": "question"})).await;
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "thinking")
    })
    .await;

    let connect_gate = Arc::new(Notify::new());
    harness.factory.hold_connect(Arc::clone(&connect_gate));
    send_voice(&mut harness, true).await;
    let echoed = collect_until(&mut harness.client, |item| {
        has_type(std::slice::from_ref(item), "reply_audio")
    })
    .await;
    assert!(
        !echoed
            .iter()
            .any(|item| matches!(item, Received::Json(v) if v["type"] == "state")),
        "a running reply keeps its state while the mic starts: {echoed:?}"
    );

    model_gate.notify_one();
    assert_eq!(
        next_resting_state(&mut harness.client).await,
        "starting",
        "the reply ended while the recognizer was still connecting"
    );
    connect_gate.notify_one();
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert!(has_state(&received, "listening"));
    harness.server.abort();
}

#[tokio::test]
async fn an_acknowledgement_and_real_audio_keep_the_underlying_thinking_state() {
    let mut harness = start_with_ack(RecordingConfig::disabled(), None, true).await;
    voice_on(&mut harness).await;
    let gate = Arc::new(Notify::new());
    harness.model.add_script(vec![
        ModelChunk::Wait(Arc::clone(&gate)),
        ModelChunk::Delta("Take the ring road.".to_string()),
    ]);
    harness.factory.complete_turn(0, "route please");
    wait_until(|| harness.model.calls().len() == 1).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(600)).await;
    tokio::time::resume();
    let before_answer = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    assert!(has_state(&before_answer, "thinking"));
    assert!(
        !has_state(&before_answer, "speaking"),
        "only the acknowledgement was sent: {before_answer:?}"
    );
    assert_eq!(
        audios(&before_answer).len(),
        1,
        "the acknowledgement: {before_answer:?}"
    );
    expect_quiet_until_marker(&mut harness).await;

    gate.notify_one();
    let answer = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    assert_eq!(
        audios(&answer),
        vec![Bytes::from("AUDIO:Take the ring road.")]
    );
    assert!(
        !has_state(&answer, "speaking"),
        "the client derives speaking from playback, not a stale server phase: {answer:?}"
    );
    harness.server.abort();
}

#[tokio::test]
async fn turning_voice_off_stops_the_sentence_being_synthesized_while_text_continues() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    let tts_gate = Arc::new(Notify::new());
    harness
        .tts
        .add_script(TtsScript::Gated(Arc::clone(&tts_gate)));
    harness.model.add_script(vec![
        ModelChunk::Delta("Hello there. ".to_string()),
        ModelChunk::Delta("Second.".to_string()),
    ]);
    harness.factory.complete_turn(0, "question");
    wait_until(|| harness.tts.inputs().len() == 1).await;

    send_voice(&mut harness, false).await;
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert_eq!(reply_text(&received), "Hello there. Second.");
    assert!(
        !has_state(&received, "speaking"),
        "no speaking state once voice is off: {received:?}"
    );
    wait_until(|| harness.tts.dropped.load(Ordering::SeqCst) == 1).await;

    tts_gate.notify_one();
    expect_quiet_until_marker(&mut harness).await;
    assert_eq!(harness.tts.inputs(), vec!["Hello there.".to_string()]);
    harness.server.abort();
}

// ---------- input and reply audio are independent ----------

async fn set_modes(harness: &mut Harness, source: &str, reply_audio: bool) {
    send_json(harness, &json!({"type": "input", "source": source})).await;
    send_json(harness, &json!({"type": "reply_audio", "on": reply_audio})).await;
    let (mut echoed, mut listening) = (false, source == "text");
    while !(echoed && listening) {
        if let Received::Json(v) = recv(&mut harness.client).await {
            if v["type"] == "reply_audio" {
                assert_eq!(v["on"], reply_audio);
                echoed = true;
            }
            listening |= v["type"] == "state" && v["state"] == "listening";
        }
    }
}

#[tokio::test]
async fn every_input_and_reply_audio_combination_opens_only_the_services_it_uses() {
    for (source, reply_audio) in [
        ("text", false),
        ("text", true),
        ("mic", false),
        ("mic", true),
    ] {
        let mut harness = start().await;
        set_modes(&mut harness, source, reply_audio).await;
        harness
            .model
            .add_script(vec![ModelChunk::Delta("Hi there.".to_string())]);
        if source == "mic" {
            harness.factory.complete_turn(0, "hello");
        } else {
            send_json(&mut harness, &json!({"type": "text", "text": "hello"})).await;
        }
        let received = collect_until(
            &mut harness.client,
            |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
        )
        .await;

        let case = format!("{source} input, reply audio {reply_audio}");
        assert_eq!(reply_text(&received), "Hi there.", "{case}");
        assert_eq!(
            audios(&received),
            if reply_audio {
                vec![Bytes::from("AUDIO:Hi there.")]
            } else {
                Vec::new()
            },
            "{case}"
        );
        assert_eq!(
            harness.factory.attempts(),
            usize::from(source == "mic"),
            "{case}: recognizer connections"
        );
        assert_eq!(
            harness.tts.inputs().len(),
            usize::from(reply_audio),
            "{case}: speech requests"
        );
        assert_eq!(
            harness.model.calls()[0],
            vec![ChatMessage::user("hello".to_string())],
            "{case}"
        );
        harness.server.abort();
    }
}

#[tokio::test]
async fn text_input_ignores_microphone_audio_and_stray_voice_controls() {
    let mut harness = start().await;
    send_json(&mut harness, &json!({"type": "reply_audio", "on": true})).await;
    for stray in [
        json!({"type": "voice", "on": true}),
        json!({"type": "input", "source": "satellite"}),
        json!({"type": "input"}),
        json!({"type": "reply_audio", "on": "yes"}),
    ] {
        send_json(&mut harness, &stray).await;
    }
    harness
        .client
        .send(ClientMessage::Binary(vec![5_u8; 3840].into()))
        .await
        .unwrap();
    let echoed = recv(&mut harness.client).await;
    assert!(
        matches!(&echoed, Received::Json(v) if v["type"] == "reply_audio" && v["on"] == true),
        "only the valid reply audio control is answered, got {echoed:?}"
    );
    expect_quiet_until_marker(&mut harness).await;
    assert_eq!(harness.factory.attempts(), 0);
    harness.server.abort();
}

#[tokio::test]
async fn stopping_input_releases_the_recognizer_and_keeps_the_spoken_reply_going() {
    let mut harness = start().await;
    set_modes(&mut harness, "mic", true).await;
    let gate = Arc::new(Notify::new());
    harness.model.add_script(vec![
        ModelChunk::Delta("One. ".to_string()),
        ModelChunk::Wait(Arc::clone(&gate)),
        ModelChunk::Delta("Two.".to_string()),
    ]);
    harness.factory.complete_turn(0, "question");
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    assert_eq!(audios(&received), vec![Bytes::from("AUDIO:One.")]);

    send_json(&mut harness, &json!({"type": "input", "source": "text"})).await;
    wait_until(|| harness.factory.released(0)).await;
    gate.notify_one();
    let rest = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert_eq!(reply_text(&rest), "Two.");
    assert_eq!(
        audios(&rest),
        vec![Bytes::from("AUDIO:Two.")],
        "reply audio stays on after input stops"
    );

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Typed.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "typed"})).await;
    let typed = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert_eq!(audios(&typed), vec![Bytes::from("AUDIO:Typed.")]);
    assert_eq!(harness.factory.attempts(), 1, "no new recognizer for text");
    harness.server.abort();
}

#[tokio::test]
async fn muting_reply_audio_drops_queued_audio_but_keeps_text_and_listening() {
    let mut harness = start().await;
    set_modes(&mut harness, "mic", true).await;
    let tts_gate = Arc::new(Notify::new());
    harness
        .tts
        .add_script(TtsScript::Gated(Arc::clone(&tts_gate)));
    harness.model.add_script(vec![
        ModelChunk::Delta("Hello there. ".to_string()),
        ModelChunk::Delta("Second.".to_string()),
    ]);
    harness.factory.complete_turn(0, "question");
    wait_until(|| harness.tts.inputs().len() == 1).await;

    send_json(&mut harness, &json!({"type": "reply_audio", "on": false})).await;
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening"),
    )
    .await;
    assert_eq!(reply_text(&received), "Hello there. Second.");
    assert_eq!(audios(&received), Vec::<Bytes>::new());
    wait_until(|| harness.tts.dropped.load(Ordering::SeqCst) == 1).await;
    tts_gate.notify_one();

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Quiet answer.".to_string())]);
    harness.factory.complete_turn(0, "still listening");
    let quiet = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert_eq!(reply_text(&quiet), "Quiet answer.");
    assert_eq!(audios(&quiet), Vec::<Bytes>::new());
    assert!(!harness.factory.released(0), "muting never stops the mic");

    send_json(&mut harness, &json!({"type": "reply_audio", "on": true})).await;
    let _ = collect_until(&mut harness.client, |item| {
        has_type(std::slice::from_ref(item), "reply_audio")
    })
    .await;
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Loud answer.".to_string())]);
    harness.factory.complete_turn(0, "again");
    let loud = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert_eq!(audios(&loud), vec![Bytes::from("AUDIO:Loud answer.")]);
    assert_eq!(
        harness.tts.inputs(),
        vec!["Hello there.".to_string(), "Loud answer.".to_string()]
    );
    harness.server.abort();
}

#[tokio::test]
async fn unmuting_mid_reply_speaks_only_the_sentences_that_follow() {
    let mut harness = start().await;
    set_modes(&mut harness, "text", false).await;
    let gate = Arc::new(Notify::new());
    harness.model.add_script(vec![
        ModelChunk::Delta("Early. ".to_string()),
        ModelChunk::Wait(Arc::clone(&gate)),
        ModelChunk::Delta("Later.".to_string()),
    ]);
    send_json(&mut harness, &json!({"type": "text", "text": "question"})).await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply" && v["text"] == "Early. "),
    )
    .await;

    send_json(&mut harness, &json!({"type": "reply_audio", "on": true})).await;
    let _ = recv(&mut harness.client).await;
    gate.notify_one();
    let rest = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert_eq!(audios(&rest), vec![Bytes::from("AUDIO:Later.")]);
    assert_eq!(harness.tts.inputs(), vec!["Later.".to_string()]);
    harness.server.abort();
}

#[tokio::test]
async fn cancel_and_a_new_turn_both_end_a_text_reply_that_is_being_spoken() {
    let mut harness = start().await;
    set_modes(&mut harness, "text", true).await;
    harness.model.add_script(vec![ModelChunk::Delta(
        "First sentence. Second sentence.".to_string(),
    )]);
    harness.tts.add_script(TtsScript::Audio);
    harness.tts.add_script(TtsScript::Hang);
    send_json(&mut harness, &json!({"type": "text", "text": "one"})).await;
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    wait_until(|| harness.tts.inputs().len() == 2).await;

    send_json(&mut harness, &json!({"type": "cancel"})).await;
    let cancelled = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert!(has_type(&cancelled, "interrupted"));
    wait_until(|| harness.tts.dropped.load(Ordering::SeqCst) == 1).await;

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Slow one. Slow two.".to_string())]);
    harness.tts.add_script(TtsScript::Audio);
    harness.tts.add_script(TtsScript::Hang);
    send_json(&mut harness, &json!({"type": "text", "text": "two"})).await;
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    wait_until(|| harness.tts.inputs().len() == 4).await;

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Fresh.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "three"})).await;
    let superseding = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert!(has_type(&superseding, "interrupted"));
    assert_eq!(audios(&superseding), vec![Bytes::from("AUDIO:Fresh.")]);
    wait_until(|| harness.tts.dropped.load(Ordering::SeqCst) == 2).await;
    assert_eq!(harness.factory.attempts(), 0);
    harness.server.abort();
}

#[tokio::test]
async fn a_fatal_recognizer_failure_falls_back_to_text_input_and_keeps_reply_audio() {
    let mut harness = start().await;
    set_modes(&mut harness, "mic", true).await;
    harness.factory.push(
        0,
        Err(RecognizerError::Fatal(
            "invalid request (code 1008)".to_string(),
        )),
    );
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert!(received.iter().any(|item| matches!(item, Received::Json(v)
        if v["type"] == "input" && v["source"] == "text" && v["ready"] == true)));
    assert!(
        !has_type(&received, "reply_audio"),
        "the failure does not touch reply audio: {received:?}"
    );
    wait_until(|| harness.factory.released(0)).await;

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Still talking.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "typed"})).await;
    let typed = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert_eq!(audios(&typed), vec![Bytes::from("AUDIO:Still talking.")]);
    assert_eq!(harness.factory.attempts(), 1);
    harness.server.abort();
}

#[tokio::test]
async fn a_reconnect_resumes_without_repeating_turns_and_releases_the_old_recognizer() {
    let mut harness = start().await;
    set_modes(&mut harness, "mic", true).await;
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Answer one.".to_string())]);
    harness.factory.complete_turn(0, "first question");
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    let id = harness.session_id.clone();
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;
    wait_until(|| harness.factory.released(0)).await;

    let turns = harness.reconnect(Some(&id)).await;
    assert_eq!(
        turns,
        json!([
            {"role": "user", "text": "first question"},
            {"role": "assistant", "text": "Answer one."},
        ])
    );
    send_voice(&mut harness, true).await;
    let _ = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Answer two.".to_string())]);
    harness.factory.complete_turn(1, "second question");
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    assert_eq!(audios(&received), vec![Bytes::from("AUDIO:Answer two.")]);
    assert_eq!(harness.factory.attempts(), 2);
    assert_eq!(
        harness.model.calls()[1],
        vec![
            ChatMessage::user("first question".to_string()),
            ChatMessage::assistant("Answer one.".to_string()),
            ChatMessage::user("second question".to_string()),
        ]
    );
    harness.server.abort();
}

// ---------- saved conversations ----------

fn saved_titles(harness: &Harness) -> Vec<String> {
    let reopened = SessionStore::open(&harness.dir.0.join("sessions")).unwrap();
    reopened.list().into_iter().map(|row| row.title).collect()
}

async fn open_client(
    addr: std::net::SocketAddr,
    resume: Option<&str>,
) -> (ClientSocket, String, Value) {
    let url = resume.map_or_else(
        || format!("ws://{addr}/session"),
        |id| format!("ws://{addr}/session?resume={id}"),
    );
    let (mut client, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let (id, turns) = read_session_opening(&mut client).await;
    (client, id, turns)
}

async fn expect_closed(client: &mut ClientSocket) {
    let next = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("socket stayed open");
    assert!(
        matches!(next, None | Some(Ok(ClientMessage::Close(_)) | Err(_))),
        "expected the socket to close, got {next:?}"
    );
}

async fn ask(harness: &mut Harness, question: &str, answer: &str) {
    harness
        .model
        .add_script(vec![ModelChunk::Delta(answer.to_string())]);
    send_json(harness, &json!({"type": "text", "text": question})).await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
}

fn url(harness: &Harness, path: &str) -> String {
    format!("http://{}{path}", harness.addr)
}

async fn listed(harness: &Harness) -> Vec<Value> {
    let body: Value = reqwest::get(url(harness, "/sessions"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["sessions"].as_array().unwrap().clone()
}

#[tokio::test]
async fn conversations_survive_a_restart_with_their_history_and_turn_numbers() {
    let mut harness = start().await;
    let id = harness.session_id.clone();
    ask(&mut harness, "what's twelve times seven?", "Eighty-four.").await;
    harness.store.flush().await;

    let reopened = SessionStore::open(&harness.dir.0.join("sessions")).unwrap();
    let summaries = reopened.list();
    assert_eq!(
        summaries
            .iter()
            .map(|s| (&s.id[..], &s.title[..]))
            .collect::<Vec<_>>(),
        vec![(id.as_str(), "what's twelve times seven?")]
    );
    let attachment = reopened.attach(Some(&id)).await;
    assert_eq!(
        (attachment.history, attachment.next_turn),
        (
            vec![
                ChatMessage::user("what's twelve times seven?".to_string()),
                ChatMessage::assistant("Eighty-four.".to_string()),
            ],
            1
        )
    );
    harness.server.abort();
}

#[tokio::test]
async fn the_list_shows_only_conversations_with_turns_and_supports_rename_and_delete() {
    let mut harness = start().await;
    assert_eq!(listed(&harness).await, Vec::<Value>::new());
    let id = harness.session_id.clone();
    ask(&mut harness, "first question", "First answer.").await;

    let rows = listed(&harness).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id.as_str());
    assert_eq!(rows[0]["title"], "first question");
    assert!(rows[0]["age_s"].is_u64(), "row: {}", rows[0]);

    let client = reqwest::Client::new();
    let too_long = format!("{{\"title\":\"{}\"}}", "x".repeat(81));
    let cases = [
        ("{\"title\":\"  \"}", id.as_str(), 400),
        (too_long.as_str(), id.as_str(), 400),
        ("not json", id.as_str(), 400),
        ("{\"name\":\"x\"}", id.as_str(), 400),
        ("{\"title\":\"Maths\"}", "no-such-session", 404),
        ("{\"title\":\"  Times   tables \"}", id.as_str(), 204),
    ];
    for (body, target, status) in cases {
        let response = client
            .patch(url(&harness, &format!("/sessions/{target}")))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status, "body: {body:.40}");
        if status != 204 {
            let error: Value = response.json().await.unwrap();
            assert!(
                error["error"].as_str().is_some_and(|m| !m.is_empty()),
                "{error}"
            );
        }
    }
    assert_eq!(listed(&harness).await[0]["title"], "Times tables");

    let delete = |target: &str| {
        client
            .delete(url(&harness, &format!("/sessions/{target}")))
            .send()
    };
    assert_eq!(
        delete("no-such-session").await.unwrap().status().as_u16(),
        404
    );
    assert_eq!(delete(&id).await.unwrap().status().as_u16(), 204);
    assert_eq!(listed(&harness).await, Vec::<Value>::new());
    harness.store.flush().await;
    assert_eq!(saved_titles(&harness), Vec::<String>::new());
    harness.server.abort();
}

#[tokio::test]
async fn opening_a_live_conversation_elsewhere_moves_it_and_stops_its_reply() {
    let mut harness = start().await;
    let id = harness.session_id.clone();
    ask(&mut harness, "first question", "First answer.").await;
    harness.model.add_script(vec![
        ModelChunk::Delta("Partial words. ".to_string()),
        ModelChunk::Wait(Arc::new(Notify::new())),
    ]);
    send_json(
        &mut harness,
        &json!({"type": "text", "text": "second question"}),
    )
    .await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply"),
    )
    .await;

    let (mut other, other_id, turns) = open_client(harness.addr, Some(&id)).await;
    assert_eq!(other_id, id);
    assert_eq!(
        turns,
        json!([
            {"role": "user", "text": "first question"},
            {"role": "assistant", "text": "First answer."},
            {"role": "user", "text": "second question"},
            {"role": "assistant", "text": "Partial words."},
        ])
    );

    let ended = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "ended"),
    )
    .await;
    assert_eq!(
        ended.last(),
        Some(&Received::Json(json!({
            "type": "ended",
            "reason": "moved",
            "message": "This conversation continued on another device.",
        })))
    );
    expect_closed(&mut harness.client).await;
    assert_eq!(harness.model.dropped.load(Ordering::SeqCst), 2);

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Carrying on.".to_string())]);
    harness.client = {
        send_json_to(
            &mut other,
            &json!({"type": "text", "text": "third question"}),
        )
        .await;
        other
    };
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
    )
    .await;
    assert_eq!(harness.model.calls().last().map(Vec::len), Some(5));
    assert_eq!(listed(&harness).await.len(), 1);
    harness.server.abort();
}

async fn send_json_to(client: &mut ClientSocket, value: &Value) {
    client
        .send(ClientMessage::Text(value.to_string().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn deleting_a_live_conversation_tells_its_device_and_it_is_not_saved_again() {
    let mut harness = start().await;
    let id = harness.session_id.clone();
    ask(&mut harness, "to be deleted", "Gone soon.").await;
    harness.model.add_script(vec![
        ModelChunk::Delta("Still talking. ".to_string()),
        ModelChunk::Wait(Arc::new(Notify::new())),
    ]);
    send_json(&mut harness, &json!({"type": "text", "text": "and again"})).await;
    let _ = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply"),
    )
    .await;

    let response = reqwest::Client::new()
        .delete(url(&harness, &format!("/sessions/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 204);
    let ended = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "ended"),
    )
    .await;
    assert_eq!(
        ended.last(),
        Some(&Received::Json(json!({
            "type": "ended",
            "reason": "deleted",
            "message": "This conversation was deleted.",
        })))
    );
    expect_closed(&mut harness.client).await;
    wait_closed(&harness, 1).await;
    harness.store.flush().await;
    assert_eq!(saved_titles(&harness), Vec::<String>::new());

    let turns = harness.reconnect(Some(&id)).await;
    assert_eq!(turns, json!([]));
    assert_ne!(harness.session_id, id);
    harness.server.abort();
}

async fn wait_for_title_event(data_dir: &ScratchDir, session: &str) -> Value {
    wait_until(|| {
        read_recorded_lines_blocking(&data_dir.0, session)
            .iter()
            .any(|line| line["event"] == "title")
    })
    .await;
    read_recorded_lines_blocking(&data_dir.0, session)
        .into_iter()
        .find(|line| line["event"] == "title")
        .unwrap()
}

fn metrics_only() -> RecordKinds {
    RecordKinds {
        metrics: true,
        text: false,
        audio: false,
    }
}

#[tokio::test]
async fn the_background_model_titles_the_first_exchange_and_the_call_is_measured() {
    let data_dir = ScratchDir::new("title-ok");
    let (title_fake, titler) = title_model();
    title_fake.add_script(vec![
        ModelChunk::Delta("\"Twelve times seven".to_string()),
        ModelChunk::Delta(" explained.\"".to_string()),
    ]);
    let mut harness = start_with(recording_config(metrics_only(), &data_dir), Some(titler)).await;
    let id = harness.session_id.clone();
    let long_question = "what's twelve times seven, and can you show me how to work it out";
    ask(&mut harness, long_question, "Eighty-four.").await;

    let event = wait_for_title_event(&data_dir, &id).await;
    assert_eq!(event["model"], "fake-title-model");
    assert_eq!(event["outcome"], "ok");
    assert!(event["latency_ms"].is_u64(), "{event}");
    assert_eq!(
        listed(&harness).await[0]["title"],
        "Twelve times seven explained"
    );
    let prompt = &title_fake.calls()[0];
    assert!(
        prompt[0].content.contains(long_question) && prompt[0].content.contains("Eighty-four."),
        "title prompt should carry the exchange: {prompt:?}"
    );

    harness.store.flush().await;
    assert_eq!(saved_titles(&harness), vec!["Twelve times seven explained"]);
    harness.server.abort();
}

#[tokio::test]
async fn a_failed_title_call_keeps_the_first_line_title_and_is_measured() {
    let data_dir = ScratchDir::new("title-fail");
    let (title_fake, titler) = title_model();
    title_fake.add_script(vec![ModelChunk::Fail]);
    let mut harness = start_with(recording_config(metrics_only(), &data_dir), Some(titler)).await;
    let id = harness.session_id.clone();
    ask(
        &mut harness,
        "what's twelve times seven, and can you show me how to work it out",
        "Eighty-four.",
    )
    .await;

    let event = wait_for_title_event(&data_dir, &id).await;
    assert_eq!(event["outcome"], "failed");
    assert!(
        event["error"]
            .as_str()
            .is_some_and(|e| e.contains("model blew up")),
        "{event}"
    );
    assert_eq!(
        listed(&harness).await[0]["title"],
        "what's twelve times seven, and can you s…"
    );
    harness.server.abort();
}

#[tokio::test]
async fn a_title_the_user_typed_is_not_overwritten_by_the_model() {
    let data_dir = ScratchDir::new("title-renamed");
    let (title_fake, titler) = title_model();
    let gate = Arc::new(Notify::new());
    title_fake.add_script(vec![
        ModelChunk::Wait(Arc::clone(&gate)),
        ModelChunk::Delta("Model title".to_string()),
    ]);
    let mut harness = start_with(recording_config(metrics_only(), &data_dir), Some(titler)).await;
    let id = harness.session_id.clone();
    ask(&mut harness, "hello", "Hi.").await;
    wait_until(|| !title_fake.calls().is_empty()).await;

    let response = reqwest::Client::new()
        .patch(url(&harness, &format!("/sessions/{id}")))
        .json(&json!({"title": "Mine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 204);
    gate.notify_one();
    let event = wait_for_title_event(&data_dir, &id).await;
    assert_eq!(event["outcome"], "ok");
    assert_eq!(listed(&harness).await[0]["title"], "Mine");
    harness.server.abort();
}

#[tokio::test]
async fn only_a_voice_turn_carries_its_padded_clip_and_only_with_audio_input_on() {
    // Recording the mic fills the buffer too, but must not send the clip to the model.
    for (audio_input, record_audio) in [(true, false), (false, false), (false, true)] {
        let data_dir = ScratchDir::new("audio-input");
        let kinds = RecordKinds {
            audio: record_audio,
            ..metrics_only()
        };
        let config = recording_config(kinds, &data_dir);
        let mut harness = start_full(config, None, false, audio_input, None).await;
        voice_on(&mut harness).await;
        let mut mic = Vec::new();
        for frame in 0..30_u8 {
            let bytes = vec![frame; 3840];
            mic.extend_from_slice(&bytes);
            harness
                .client
                .send(ClientMessage::Binary(bytes.into()))
                .await
                .unwrap();
        }
        wait_until(|| harness.factory.audio_received().len() == 30).await;
        let connection = harness.factory.latest_connection();
        for (event, at) in [("start", 1000), ("end", 1500)] {
            let turn_id = "turn-1".to_string();
            let audio_processed_ms = Some(at);
            harness.factory.push(
                connection,
                Ok(if event == "start" {
                    RecognizerEvent::SpeechStart {
                        turn_id,
                        audio_processed_ms,
                    }
                } else {
                    RecognizerEvent::SpeechEnd {
                        turn_id,
                        audio_processed_ms,
                    }
                }),
            );
        }
        for answer in ["Hello there.", "Typed answer."] {
            harness
                .model
                .add_script(vec![ModelChunk::Delta(answer.to_string())]);
        }
        harness.factory.complete_turn(connection, "hello thalvi");
        let listening = |item: &Received| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening");
        let _ = collect_until(&mut harness.client, listening).await;
        send_json(&mut harness, &json!({"type": "text", "text": "and typed"})).await;
        let _ = collect_until(&mut harness.client, listening).await;

        // 1000 ms and 1500 ms, each padded by 300 ms: mic bytes 700..1800 ms.
        let expected = mic[700 * 48..1800 * 48].to_vec();
        assert_eq!(
            harness.model.clips(),
            vec![audio_input.then_some(expected), None],
            "audio_input={audio_input} record_audio={record_audio}"
        );
        let texts: Vec<String> = harness.model.calls()[1]
            .iter()
            .map(|message| message.content.clone())
            .collect();
        assert_eq!(texts, ["hello thalvi", "Hello there.", "and typed"]);

        let session = harness.session_id.clone();
        harness.client.close(None).await.unwrap();
        wait_closed(&harness, 1).await;
        harness.server.abort();
        let lines = read_recorded_lines_blocking(&data_dir.0, &session);
        let input_ms: Vec<Value> = lines
            .iter()
            .filter(|line| line["event"] == "reply")
            .map(|line| line["input_audio_ms"].clone())
            .collect();
        let first = if audio_input {
            json!(1100)
        } else {
            Value::Null
        };
        assert_eq!(
            input_ms,
            vec![first, Value::Null],
            "audio_input={audio_input} record_audio={record_audio}"
        );
    }
}

#[tokio::test]
async fn a_voice_turn_whose_echo_disagrees_gets_the_fallback_and_leaves_no_history() {
    let cases = [
        (
            Some("The user said: \"turn on the lights\""),
            "Lights are on.",
            "Lights are on.",
        ),
        (
            Some("The user said: \"play some jazz\""),
            "Playing jazz.",
            "Sorry, I didn't catch that.",
        ),
        (None, "Lights are on.", "Lights are on."),
    ];
    for (echo, answer, spoken) in cases {
        let mut harness = start_full(RecordingConfig::disabled(), None, false, true, None).await;
        harness.model.add_script(
            echo.map(|echo| ModelChunk::Heard(echo.to_string()))
                .into_iter()
                .chain([ModelChunk::Delta(answer.to_string())])
                .collect(),
        );
        voice_on(&mut harness).await;
        for _ in 0..10 {
            harness
                .client
                .send(ClientMessage::Binary(vec![3_u8; 3840].into()))
                .await
                .unwrap();
        }
        wait_until(|| harness.factory.audio_received().len() == 10).await;
        let connection = harness.factory.latest_connection();
        harness
            .factory
            .complete_turn(connection, "turn on the lights");
        let received = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
        })
        .await;
        let replies: String = received
            .iter()
            .filter_map(|item| match item {
                Received::Json(v) if v["type"] == "reply" => v["text"].as_str(),
                _ => None,
            })
            .collect();
        assert_eq!(replies, spoken, "echo {echo:?}");
        assert_eq!(harness.tts.inputs(), [spoken], "echo {echo:?}");

        harness
            .model
            .add_script(vec![ModelChunk::Delta("Fine.".to_string())]);
        send_json(&mut harness, &json!({"type": "text", "text": "and typed"})).await;
        let _ = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
        })
        .await;
        let texts: Vec<String> = harness.model.calls()[1]
            .iter()
            .map(|message| message.content.clone())
            .collect();
        let expected: &[&str] = if spoken == answer {
            &["turn on the lights", answer, "and typed"]
        } else {
            &["and typed"]
        };
        assert_eq!(texts, expected, "echo {echo:?}");
        harness.server.abort();
    }
}
#[tokio::test]
async fn voice_echo_admits_matching_short_reply_and_rejects_tool_on_mismatch() {
    for (echo, accepted) in [("yes", true), ("no", false)] {
        let mut harness = start_full(RecordingConfig::disabled(), None, false, true, None).await;
        harness.model.add_script(vec![
            ModelChunk::Heard(echo.to_string()),
            search_call("call-1", r#"{"query":"anything"}"#),
        ]);
        if accepted {
            harness.model.add_script(vec![
                ModelChunk::Heard("yes".to_string()),
                ModelChunk::Delta("Done.".to_string()),
            ]);
        }
        voice_on(&mut harness).await;
        for _ in 0..10 {
            harness
                .client
                .send(ClientMessage::Binary(vec![3_u8; 3840].into()))
                .await
                .unwrap();
        }
        wait_until(|| harness.factory.audio_received().len() == 10).await;
        let connection = harness.factory.latest_connection();
        harness.factory.complete_turn(connection, "yes");
        let received = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
        }).await;
        assert_eq!(!tasks(&received).is_empty(), accepted);
        assert_eq!(
            reply_text(&received),
            if accepted {
                "Done."
            } else {
                "Sorry, I didn't catch that."
            }
        );
        harness
            .model
            .add_script(vec![ModelChunk::Delta("Fine.".to_string())]);
        send_json(&mut harness, &json!({"type": "text", "text": "next"})).await;
        finish_reply(&mut harness).await;
        let calls = harness.model.calls();
        let texts: Vec<&str> = calls
            .last()
            .unwrap()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(
            texts,
            if accepted {
                vec!["yes", "Done.", "next"]
            } else {
                vec!["next"]
            }
        );
        harness.server.abort();
    }
}

// ---------- history search tool, selected voice, save notice ----------

fn search_call(id: &str, arguments: &str) -> ModelChunk {
    ModelChunk::Tool(ToolCall {
        id: id.to_string(),
        name: "search_history".to_string(),
        arguments: arguments.to_string(),
    })
}

fn tasks(received: &[Received]) -> Vec<Value> {
    received
        .iter()
        .filter_map(|item| match item {
            Received::Json(v) if v["type"] == "task" => Some(v["step"].clone()),
            Received::Json(_) | Received::Audio(_) => None,
        })
        .collect()
}

/// The states and task statuses in the order the client saw them.
fn progress(received: &[Received]) -> Vec<String> {
    received
        .iter()
        .filter_map(|item| match item {
            Received::Json(v) if v["type"] == "state" => Some(format!("state:{}", v["state"])),
            Received::Json(v) if v["type"] == "task" => {
                Some(format!("task:{}", v["step"]["status"]))
            }
            Received::Json(_) | Received::Audio(_) => None,
        })
        .collect()
}

async fn finish_reply(harness: &mut Harness) -> Vec<Received> {
    collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await
}

async fn save_past_conversation(harness: &Harness, text: &str) -> String {
    let past = harness.store.attach(None).await;
    harness
        .store
        .save(&past, &[ChatMessage::user(text.to_string())], 1);
    let id = past.id.clone();
    harness.store.release(past);
    harness.store.flush().await;
    id
}

#[tokio::test]
async fn a_history_search_feeds_the_follow_up_call_and_shows_task_steps() {
    let mut harness = start().await;
    let past_id =
        save_past_conversation(&harness, "We settled the road briefing route earlier").await;
    harness
        .model
        .add_script(vec![search_call("call-1", r#"{"query":"road briefing"}"#)]);
    harness.model.add_script(vec![ModelChunk::Delta(
        "You chose the north route.".to_string(),
    )]);
    send_json(
        &mut harness,
        &json!({"type": "text", "text": "what did we decide about the road briefing?"}),
    )
    .await;
    let received = finish_reply(&mut harness).await;

    assert_eq!(
        tasks(&received),
        vec![
            json!({"id": "call-1", "label": "Searching conversations", "status": "running"}),
            json!({"id": "call-1", "label": "Searching conversations", "status": "done", "detail": "2 matches"}),
        ]
    );
    assert_eq!(
        progress(&received),
        [
            "state:\"thinking\"",
            "task:\"running\"",
            "state:\"working\"",
            "task:\"done\"",
            "state:\"thinking\"",
        ]
    );
    assert_eq!(reply_text(&received), "You chose the north route.");

    let rounds = harness.model.tool_rounds();
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0].offered, ["search_history"]);
    assert_eq!(rounds[0].results, []);
    assert_eq!(
        rounds[1].offered,
        ["search_history"],
        "the follow-up may search again"
    );
    assert_eq!(rounds[1].results.len(), 1);
    assert_eq!(rounds[1].results[0].call.id, "call-1");
    let result: Value = serde_json::from_str(&rounds[1].results[0].content).unwrap();
    assert_eq!(result["count"], 2);
    let sessions: Vec<&str> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|found| found["session_id"].as_str())
        .collect();
    assert!(
        sessions.contains(&past_id.as_str()) && sessions.contains(&harness.session_id.as_str()),
        "the search covers a saved conversation and the live one: {sessions:?}"
    );

    let calls = harness.model.calls();
    assert_eq!(
        calls[0], calls[1],
        "tool output is not added to the history"
    );
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;
    let turns = harness.reconnect(Some(&harness.session_id.clone())).await;
    let texts: Vec<&str> = turns
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|turn| turn["text"].as_str())
        .collect();
    assert_eq!(
        texts,
        [
            "what did we decide about the road briefing?",
            "You chose the north route."
        ]
    );
    harness.server.abort();
}

#[tokio::test]
async fn finishing_a_search_returns_to_thinking_after_preamble_audio() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    harness.model.add_script(vec![
        ModelChunk::Delta("One moment.".to_string()),
        search_call("call-1", r#"{"query":"anything"}"#),
    ]);
    harness
        .model
        .add_script(vec![ModelChunk::Delta("The answer.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "search"})).await;
    let received = finish_reply(&mut harness).await;
    let text: String = received
        .iter()
        .filter_map(|message| match message {
            Received::Json(value) if value["type"] == "reply" => value["text"].as_str(),
            _ => None,
        })
        .collect();
    assert_eq!(text, "One moment. The answer.");
    let progress = progress(&received);
    let done = progress
        .iter()
        .position(|event| event == "task:\"done\"")
        .expect("search completion");
    assert_eq!(
        progress.get(done + 1).map(String::as_str),
        Some("state:\"thinking\"")
    );
    harness.server.abort();
}

#[tokio::test]
async fn a_current_scope_search_skips_saved_conversations() {
    let mut harness = start().await;
    save_past_conversation(&harness, "The cabin code is saved in another chat").await;
    harness.model.add_script(vec![search_call(
        "call-1",
        r#"{"query":"cabin","scope":"current"}"#,
    )]);
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Only this chat.".to_string())]);
    send_json(
        &mut harness,
        &json!({"type": "text", "text": "cabin code?"}),
    )
    .await;
    let received = finish_reply(&mut harness).await;
    assert_eq!(tasks(&received)[1]["detail"], "1 match");
}

#[tokio::test]
async fn an_invalid_or_unknown_tool_call_fails_its_step_and_the_reply_still_finishes() {
    for (name, arguments, detail) in [
        (
            "search_history",
            r#"{"query":""}"#,
            "The history search query can't be empty.",
        ),
        (
            "search_history",
            r#"{"query":"x","semantic":true}"#,
            "Send query text and optionally scope current or all.",
        ),
        ("delete_everything", "{}", "That tool doesn't exist."),
    ] {
        let mut harness = start().await;
        harness.model.add_script(vec![ModelChunk::Tool(ToolCall {
            id: "bad-1".to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        })]);
        harness.model.add_script(vec![ModelChunk::Delta(
            "I could not look that up.".to_string(),
        )]);
        send_json(&mut harness, &json!({"type": "text", "text": "find it"})).await;
        let received = finish_reply(&mut harness).await;

        let steps = tasks(&received);
        assert_eq!(steps.len(), 2, "{name} {arguments}: {steps:?}");
        assert_eq!(steps[1]["status"], "failed", "{name} {arguments}");
        assert_eq!(steps[1]["detail"], detail, "{name} {arguments}");
        assert_eq!(steps[1]["id"], "bad-1");
        assert!(!has_type(&received, "error"), "{name}: {received:?}");
        assert_eq!(reply_text(&received), "I could not look that up.");
        let rounds = harness.model.tool_rounds();
        let result: Value = serde_json::from_str(&rounds[1].results[0].content).unwrap();
        assert_eq!(result["error"], detail, "{name} {arguments}");
        harness.server.abort();
    }
}

#[tokio::test]
async fn a_model_that_keeps_searching_is_stopped_after_a_few_rounds() {
    let mut harness = start().await;
    for round in 0..4 {
        harness.model.add_script(vec![search_call(
            &format!("loop-{round}"),
            r#"{"query":"x"}"#,
        )]);
    }
    send_json(&mut harness, &json!({"type": "text", "text": "hmm"})).await;
    let received = collect_until(
        &mut harness.client,
        |item| matches!(item, Received::Json(v) if v["type"] == "error"),
    )
    .await;
    assert_eq!(
        tasks(&received).len(),
        6,
        "three searches ran, then the model was cut off: {received:?}"
    );
    let offered: Vec<usize> = harness
        .model
        .tool_rounds()
        .iter()
        .map(|round| round.offered.len())
        .collect();
    assert_eq!(offered, [1, 1, 1, 0], "the last call must answer in text");
    assert!(
        matches!(received.last(), Some(Received::Json(v)) if v["message"] == "The language model kept searching instead of answering."),
        "{received:?}"
    );
    assert!(
        has_type(&received, "reply_end"),
        "the reply ends: {received:?}"
    );
    let after = recv(&mut harness.client).await;
    assert!(
        matches!(&after, Received::Json(v) if v["type"] == "reply_end")
            || has_type(&received, "reply_end"),
        "the reply ends: {after:?}"
    );
    harness.server.abort();
}

#[tokio::test]
async fn tool_calls_record_latency_outcome_and_match_count_in_metrics() {
    let kinds = RecordKinds {
        metrics: true,
        text: false,
        audio: false,
    };
    let data_dir = ScratchDir::new("tool-metrics");
    let mut harness = start_with_recording(recording_config(kinds, &data_dir)).await;
    harness
        .model
        .add_script(vec![search_call("c1", r#"{"query":"look"}"#)]);
    harness
        .model
        .add_script(vec![search_call("c2", r#"{"query":""}"#)]);
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Done.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "look"})).await;
    finish_reply(&mut harness).await;
    let session = harness.session_id.clone();
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;
    harness.server.abort();

    let events = |lines: &[Value]| -> Vec<Value> {
        lines
            .iter()
            .filter(|line| line["event"] == "tool")
            .cloned()
            .collect()
    };
    wait_until(|| events(&read_recorded_lines_blocking(&data_dir.0, &session)).len() == 2).await;
    let tools = events(&read_recorded_lines_blocking(&data_dir.0, &session));
    assert_eq!(tools[0]["tool"], "search_history");
    assert_eq!(tools[0]["outcome"], "ok");
    assert_eq!(
        tools[0]["matches"], 1,
        "the live user turn matches nothing else: {tools:?}"
    );
    assert!(tools[0]["total_ms"].is_u64());
    assert_eq!(tools[1]["outcome"], "failed");
    assert_eq!(
        tools[1]["error"],
        "The history search query can't be empty."
    );
    let lines = read_recorded_lines_blocking(&data_dir.0, &session);
    let calls: Vec<_> = lines
        .iter()
        .filter(|line| line["event"] == "model_call")
        .collect();
    assert_eq!(calls.len(), 3);
    for (round, call) in calls.iter().enumerate() {
        assert_eq!(call["round"], round);
        assert_eq!(call["outcome"], "ok");
        assert!(call["total_ms"].is_u64());
    }
    let reply = lines
        .iter()
        .find(|line| line["event"] == "reply")
        .expect("reply metric");
    assert!(reply["reply_loop_ms"].is_u64());
    assert!(reply.get("model_ms").is_none());
}

#[tokio::test]
async fn an_unknown_voice_name_is_refused_and_replies_keep_the_default() {
    let mut harness = start().await;
    send_json(
        &mut harness,
        &json!({"type": "voice_name", "voice": "nobody"}),
    )
    .await;
    let refused = recv(&mut harness.client).await;
    assert!(
        matches!(&refused, Received::Json(v) if v["type"] == "error" && v["message"].as_str().is_some_and(|m| m.contains("isn't offered"))),
        "{refused:?}"
    );
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Hi.".to_string())]);
    voice_on(&mut harness).await;
    harness.factory.complete_turn(0, "hello");
    collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert_eq!(harness.tts.voices(), ["af_heart"]);
    harness.server.abort();
}

#[tokio::test]
async fn a_chosen_voice_applies_from_the_next_reply_not_mid_reply() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    let gate = Arc::new(Notify::new());
    harness.model.add_script(vec![
        ModelChunk::Delta("First sentence. ".to_string()),
        ModelChunk::Wait(Arc::clone(&gate)),
        ModelChunk::Delta("Second sentence.".to_string()),
    ]);
    harness.factory.complete_turn(0, "one");
    collect_until(&mut harness.client, |item| {
        matches!(item, Received::Audio(_))
    })
    .await;
    send_json(
        &mut harness,
        &json!({"type": "voice_name", "voice": "sohee"}),
    )
    .await;
    expect_quiet_until_marker(&mut harness).await;
    gate.notify_one();
    collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert_eq!(harness.tts.voices(), ["af_heart", "af_heart"]);

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Next reply.".to_string())]);
    harness.factory.complete_turn(0, "two");
    collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    assert_eq!(harness.tts.voices(), ["af_heart", "af_heart", "sohee"]);
    harness.server.abort();
}

#[tokio::test]
async fn reply_metrics_name_the_voice_actually_used() {
    let kinds = RecordKinds {
        metrics: true,
        text: false,
        audio: false,
    };
    let data_dir = ScratchDir::new("voice-metrics");
    let mut harness = start_with_recording(recording_config(kinds, &data_dir)).await;
    send_json(
        &mut harness,
        &json!({"type": "voice_name", "voice": "sohee"}),
    )
    .await;
    voice_on(&mut harness).await;
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Hi.".to_string())]);
    harness.factory.complete_turn(0, "hello");
    collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
    })
    .await;
    let session = harness.session_id.clone();
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;
    harness.server.abort();
    wait_until(|| {
        read_recorded_lines_blocking(&data_dir.0, &session)
            .iter()
            .any(|line| line["event"] == "reply")
    })
    .await;
    let lines = read_recorded_lines_blocking(&data_dir.0, &session);
    let reply = lines.iter().find(|line| line["event"] == "reply").unwrap();
    assert_eq!(reply["voice"], "sohee");
    let sentence = lines
        .iter()
        .find(|line| line["event"] == "tts_sentence")
        .unwrap();
    assert_eq!(sentence["voice"], "sohee");
}

#[tokio::test]
async fn a_failed_live_save_sends_one_notice_while_the_reply_finishes() {
    let mut harness = start().await;
    harness
        .model
        .add_script(vec![ModelChunk::Delta("First.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "one"})).await;
    finish_reply(&mut harness).await;
    harness.store.flush().await;

    // Replacing the store directory with a file makes every later write fail.
    let sessions_dir = harness.dir.0.join("sessions");
    std::fs::remove_dir_all(&sessions_dir).unwrap();
    std::fs::write(&sessions_dir, b"not a directory").unwrap();

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Second.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "two"})).await;
    let mut received = Vec::new();
    while !(has_type(&received, "reply_end") && notices(&received) > 0) {
        received.push(recv(&mut harness.client).await);
    }
    assert_eq!(reply_text(&received), "Second.");

    harness
        .model
        .add_script(vec![ModelChunk::Delta("Third.".to_string())]);
    send_json(&mut harness, &json!({"type": "text", "text": "three"})).await;
    finish_reply(&mut harness).await;
    send_json(&mut harness, &json!({"type": "text", "text": ""})).await;
    let mut later = Vec::new();
    loop {
        let item = recv(&mut harness.client).await;
        let marker = matches!(&item, Received::Json(v) if v["type"] == "error" && v["message"].as_str().is_some_and(|m| m.contains("empty")));
        later.push(item);
        if marker {
            break;
        }
    }
    assert_eq!(notices(&received), 1);
    assert_eq!(notices(&later), 0, "the notice is sent once: {later:?}");
    harness.server.abort();
}

fn notices(received: &[Received]) -> usize {
    received
        .iter()
        .filter(|item| matches!(item, Received::Json(v) if v["type"] == "error" && v["message"] == "This conversation couldn't be saved"))
        .count()
}

// ---------- private conversations ----------

async fn connect_to(addr: std::net::SocketAddr, query: &str) -> ClientSocket {
    let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/session{query}"))
        .await
        .unwrap();
    client
}

/// Leaves the harness's first connection and joins a new private conversation.
async fn open_private(harness: &mut Harness) -> Value {
    let mut client = connect_to(harness.addr, "?private=1").await;
    let opening = read_opening_message(&mut client).await;
    harness.client = client;
    harness.session_id = opening["id"].as_str().unwrap().to_string();
    opening
}

fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// Asserts no file under `dirs` contains any of `secrets`.
fn assert_no_file_holds(dirs: &[&std::path::Path], secrets: &[&str]) {
    for path in dirs.iter().flat_map(|dir| files_under(dir)) {
        let content = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        for secret in secrets {
            assert!(
                !content.contains(secret),
                "{} holds {secret:?}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn a_private_conversation_leaves_no_content_on_disk_with_every_recording_on() {
    let data_dir = ScratchDir::new("private-disk");
    let everything = RecordKinds {
        metrics: true,
        text: true,
        audio: true,
    };
    let (title_fake, titler) = title_model();
    title_fake.add_script(vec![ModelChunk::Delta("Secret title".to_string())]);
    let mut harness = start_with(recording_config(everything, &data_dir), Some(titler)).await;
    let opening = open_private(&mut harness).await;
    assert_eq!(opening["private"], true);
    assert_eq!(opening["turns"], json!([]));
    ask(&mut harness, "typed secret question", "typed secret answer").await;
    run_voice_turn(&mut harness).await;
    let private_id = harness.session_id.clone();
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 2).await;

    assert_no_file_holds(
        &[&harness.dir.0, &data_dir.0],
        &[
            "typed secret",
            "hello thalvi",
            "Hello there.",
            "Secret title",
        ],
    );
    harness.store.flush().await;
    assert!(
        files_under(&harness.dir.0.join("sessions")).is_empty(),
        "no conversation file"
    );
    assert_eq!(saved_titles(&harness), Vec::<String>::new());
    assert!(title_fake.calls().is_empty(), "no title was asked for");
    assert!(
        files_under(&data_dir.0)
            .iter()
            .all(|path| path.extension().is_none_or(|ext| ext != "wav")),
        "no microphone clip"
    );

    let lines = read_recorded_lines_blocking(&data_dir.0, &private_id);
    let events: Vec<&str> = lines
        .iter()
        .filter_map(|line| line["event"].as_str())
        .collect();
    assert_eq!(events.iter().filter(|event| **event == "turn").count(), 2);
    assert_eq!(events.iter().filter(|event| **event == "reply").count(), 2);
    assert_events(&lines, &["session_open", "model_call", "session_close"]);
    for line in &lines {
        assert!(
            ["text", "tts_text", "audio"]
                .iter()
                .all(|field| line.get(*field).is_none()),
            "content in a private metrics line: {line}"
        );
    }
    harness.server.abort();
}

#[tokio::test]
async fn a_private_conversation_is_not_listed_opened_or_found_by_another_conversation() {
    let mut harness = start().await;
    open_private(&mut harness).await;
    let private_id = harness.session_id.clone();
    ask(&mut harness, "the cabin code is 4711", "Noted.").await;

    assert_eq!(listed(&harness).await, Vec::<Value>::new());
    let detail = reqwest::get(url(&harness, &format!("/sessions/{private_id}")))
        .await
        .unwrap();
    assert_eq!(detail.status(), reqwest::StatusCode::NOT_FOUND);

    let (mut saved, saved_id, _) = open_client(harness.addr, None).await;
    harness
        .model
        .add_script(vec![search_call("call-1", r#"{"query":"cabin"}"#)]);
    harness
        .model
        .add_script(vec![ModelChunk::Delta("Nothing found.".to_string())]);
    send_json_to(
        &mut saved,
        &json!({"type": "text", "text": "what was the cabin code?"}),
    )
    .await;
    let received = collect_until(
        &mut saved,
        |item| matches!(item, Received::Json(v) if v["type"] == "reply_end"),
    )
    .await;
    let found = tasks(&received)[1]["detail"].clone();
    let rounds = harness.model.tool_rounds();
    let result: Value = serde_json::from_str(&rounds.last().unwrap().results[0].content).unwrap();
    let sessions: Vec<&str> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|found| found["session_id"].as_str())
        .collect();
    assert_eq!(
        (found, sessions),
        (json!("1 match"), vec![saved_id.as_str()]),
        "only the saved conversation's own question matches"
    );
    harness.server.abort();
}

#[tokio::test]
async fn a_private_search_covers_only_its_own_turns_whatever_the_scope() {
    let mut harness = start().await;
    save_past_conversation(&harness, "The cabin code is in a saved chat").await;
    open_private(&mut harness).await;
    ask(&mut harness, "cabin plans", "Sounds good.").await;
    for (turn, arguments) in [
        (1, r#"{"query":"cabin","scope":"all"}"#),
        (2, r#"{"query":"cabin","scope":"current"}"#),
    ] {
        harness
            .model
            .add_script(vec![search_call("call-1", arguments)]);
        harness
            .model
            .add_script(vec![ModelChunk::Delta("Done.".to_string())]);
        send_json(&mut harness, &json!({"type": "text", "text": "cabin?"})).await;
        let _ = finish_reply(&mut harness).await;
        let ids: Vec<String> = {
            let rounds = harness.model.tool_rounds();
            let result: Value =
                serde_json::from_str(&rounds.last().unwrap().results[0].content).unwrap();
            result["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|found| found["session_id"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(
            !ids.is_empty() && ids.iter().all(|id| *id == harness.session_id),
            "turn {turn}: {ids:?}"
        );
    }
    harness.server.abort();
}

#[tokio::test]
async fn failure_cancel_and_disconnect_in_a_private_conversation_save_nothing_and_keep_its_context()
{
    for case in ["model failure", "cancel", "disconnect"] {
        let mut harness = start().await;
        open_private(&mut harness).await;
        let gate = Arc::new(Notify::new());
        match case {
            "model failure" => harness.model.add_script(vec![ModelChunk::Fail]),
            _ => harness.model.add_script(vec![
                ModelChunk::Delta("Part one. ".to_string()),
                ModelChunk::Wait(Arc::clone(&gate)),
            ]),
        }
        send_json(
            &mut harness,
            &json!({"type": "text", "text": "first private question"}),
        )
        .await;
        match case {
            "model failure" => {
                let received = collect_until(
                    &mut harness.client,
                    |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
                )
                .await;
                assert!(has_type(&received, "error"), "{case}");
            }
            "cancel" => {
                let _ = collect_until(
                    &mut harness.client,
                    |item| matches!(item, Received::Json(v) if v["type"] == "reply"),
                )
                .await;
                send_json(&mut harness, &json!({"type": "cancel"})).await;
                let _ = collect_until(
                    &mut harness.client,
                    |item| matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
                )
                .await;
            }
            _ => {
                let _ = collect_until(
                    &mut harness.client,
                    |item| matches!(item, Received::Json(v) if v["type"] == "reply"),
                )
                .await;
                harness.client.close(None).await.unwrap();
                wait_closed(&harness, 2).await;
            }
        }
        if case != "disconnect" {
            ask(&mut harness, "second private question", "Fine.").await;
            let calls = harness.model.calls();
            let texts: Vec<&str> = calls
                .last()
                .unwrap()
                .iter()
                .map(|message| message.content.as_str())
                .collect();
            assert!(
                texts.contains(&"first private question"),
                "{case}: the live context is kept: {texts:?}"
            );
            harness.client.close(None).await.unwrap();
            wait_closed(&harness, 2).await;
        }
        harness.store.flush().await;
        assert!(
            files_under(&harness.dir.0.join("sessions")).is_empty(),
            "{case}: a conversation file was written"
        );
        assert_eq!(saved_titles(&harness), Vec::<String>::new(), "{case}");
        harness.server.abort();
    }
}

#[tokio::test]
async fn a_private_conversation_cannot_be_resumed_and_the_refusal_starts_nothing() {
    let mut harness = start().await;
    open_private(&mut harness).await;
    let private_id = harness.session_id.clone();
    ask(&mut harness, "private question", "Private answer.").await;
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 2).await;
    let calls_before = harness.model.calls().len();

    for query in [
        format!("?resume={private_id}"),
        format!("?private=1&resume={private_id}"),
        "?private=1&resume=s1-1".to_string(),
    ] {
        let mut client = connect_to(harness.addr, &query).await;
        let Received::Json(refusal) = recv(&mut client).await else {
            panic!("{query}: expected a message");
        };
        assert_eq!(refusal["type"], "ended", "{query}");
        assert_eq!(refusal["reason"], "private", "{query}");
        assert!(
            refusal["message"]
                .as_str()
                .is_some_and(|message| message.contains("can't be resumed")),
            "{query}: {refusal}"
        );
        expect_closed(&mut client).await;
    }
    assert_eq!(harness.model.calls().len(), calls_before);
    harness.store.flush().await;
    assert_eq!(saved_titles(&harness), Vec::<String>::new());
    harness.server.abort();
}

#[tokio::test]
async fn switching_between_saved_and_private_never_carries_context_across() {
    let mut harness = start().await;
    let saved_id = harness.session_id.clone();
    ask(&mut harness, "saved question", "Saved answer.").await;

    let opening = open_private(&mut harness).await;
    assert_eq!(opening["private"], true);
    ask(&mut harness, "private question", "Private answer.").await;
    let history_of_last_call = |harness: &Harness| -> Vec<String> {
        harness
            .model
            .calls()
            .last()
            .unwrap()
            .iter()
            .map(|message| message.content.clone())
            .collect()
    };
    assert_eq!(history_of_last_call(&harness), ["private question"]);

    let turns = harness.reconnect(Some(&saved_id)).await;
    assert_eq!(
        turns,
        json!([
            {"role": "user", "text": "saved question"},
            {"role": "assistant", "text": "Saved answer."},
        ]),
        "the saved conversation holds none of the private turns"
    );
    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 3).await;

    let mut fresh = connect_to(harness.addr, "").await;
    let opening = read_opening_message(&mut fresh).await;
    assert_eq!(
        (opening.get("private"), &opening["turns"]),
        (None, &json!([])),
        "a saved conversation does not say private"
    );
    harness.store.flush().await;
    assert_eq!(saved_titles(&harness), ["saved question"]);
    harness.server.abort();
}

// ---------- briefing headlines ----------

/// `off`, `unavailable`, or the headlines joined by `|`.
fn describe_briefing(context: &BriefingContext) -> String {
    match context {
        BriefingContext::Off => "off".to_string(),
        BriefingContext::Unavailable => "unavailable".to_string(),
        BriefingContext::Headlines(snapshot) => snapshot
            .items
            .iter()
            .map(|item| item.headline.as_str())
            .collect::<Vec<_>>()
            .join("|"),
    }
}

/// A loopback briefing source that always serves one fresh headline; when `held`, each
/// request waits for [`BriefingSource::release`].
struct BriefingSource {
    config: BriefingConfig,
    hits: Arc<AtomicUsize>,
    headline: Arc<Mutex<String>>,
    gate: Arc<Notify>,
}

impl BriefingSource {
    async fn start(headline: &str, held: bool) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let headline = Arc::new(Mutex::new(headline.to_string()));
        let gate = Arc::new(Notify::new());
        let (hits2, headline2, gate2) =
            (Arc::clone(&hits), Arc::clone(&headline), Arc::clone(&gate));
        let app = Router::new().route(
            "/v1/briefings/latest",
            get(move || {
                let (hits, headline, gate) = (
                    Arc::clone(&hits2),
                    Arc::clone(&headline2),
                    Arc::clone(&gate2),
                );
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if held {
                        gate.notified().await;
                    }
                    let generated_at = (chrono::Utc::now() - chrono::TimeDelta::hours(1))
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let headline = headline.lock().unwrap().clone();
                    axum::Json(json!({
                        "generated_at": generated_at,
                        "items": [{"id": "topic-1", "headline": headline}],
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            config: BriefingConfig {
                base_url: format!("http://{addr}"),
                token: "test-token".to_string(),
            },
            hits,
            headline,
            gate,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    async fn requested(&self, count: usize) {
        wait_until(|| self.hits() >= count).await;
    }

    fn release(&self) {
        self.gate.notify_one();
    }

    fn serve_headline(&self, headline: &str) {
        *self.headline.lock().unwrap() = headline.to_string();
    }

    fn start_briefing(&self) -> Arc<Briefing> {
        let recorder = thalvi_server::recorder::start_recording(
            RecordingConfig::disabled(),
            "briefing".into(),
        )
        .recorder;
        Briefing::start(&self.config, recorder).unwrap()
    }
}

/// Opens conversations on `briefing` until one is told `expected`.
async fn cache_holds(briefing: &Arc<Briefing>, expected: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while describe_briefing(&briefing.open()) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the briefing never reached the cache");
}

#[tokio::test]
async fn a_new_conversation_is_told_the_cached_briefing_and_keeps_it_for_every_turn() {
    let source = BriefingSource::start("Markets rally", false).await;
    let briefing = source.start_briefing();
    cache_holds(&briefing, "Markets rally").await;
    let mut harness = start_full(
        RecordingConfig::disabled(),
        None,
        false,
        false,
        Some(Arc::clone(&briefing)),
    )
    .await;
    source.serve_headline("Storm warning");
    cache_holds(&briefing, "Storm warning").await;
    ask(&mut harness, "what is new?", "A rally.").await;
    ask(&mut harness, "and now?", "Still a rally.").await;
    assert_eq!(
        harness.model.briefings(),
        ["Markets rally", "Markets rally"],
        "later turns keep what the conversation opened with"
    );
}

#[tokio::test]
async fn a_cold_conversation_replies_before_the_fetch_finishes_and_a_resume_takes_the_new_snapshot()
{
    let source = BriefingSource::start("Markets rally", true).await;
    let briefing = source.start_briefing();
    source.requested(1).await;
    let mut harness = start_full(
        RecordingConfig::disabled(),
        None,
        false,
        false,
        Some(Arc::clone(&briefing)),
    )
    .await;
    let id = harness.session_id.clone();
    ask(&mut harness, "what is new?", "No briefing yet.").await;
    assert_eq!(source.hits(), 1, "opening joined the running fetch");
    assert_eq!(harness.model.briefings(), ["unavailable"]);

    source.release();
    cache_holds(&briefing, "Markets rally").await;
    ask(&mut harness, "and now?", "Still none.").await;
    assert_eq!(harness.model.briefings(), ["unavailable", "unavailable"]);

    harness.client.close(None).await.unwrap();
    wait_closed(&harness, 1).await;
    harness.reconnect(Some(&id)).await;
    ask(&mut harness, "and after resuming?", "A rally.").await;
    assert_eq!(harness.model.briefings()[2], "Markets rally");
}

#[tokio::test]
async fn a_private_conversation_gets_no_briefing_and_asks_the_source_for_nothing() {
    let source = BriefingSource::start("Markets rally", false).await;
    let briefing = source.start_briefing();
    cache_holds(&briefing, "Markets rally").await;
    let mut harness = start_full(
        RecordingConfig::disabled(),
        None,
        false,
        false,
        Some(Arc::clone(&briefing)),
    )
    .await;
    ask(&mut harness, "saved question", "Saved answer.").await;
    let before = source.hits();

    open_private(&mut harness).await;
    ask(&mut harness, "private question", "Private answer.").await;
    assert_eq!(harness.model.briefings(), ["Markets rally", "off"]);
    assert_eq!(
        source.hits(),
        before,
        "a private conversation fetched nothing"
    );

    open_client(harness.addr, None).await;
    source.requested(before + 1).await;
}

#[tokio::test]
async fn without_a_briefing_source_replies_carry_no_briefing() {
    let mut harness = start().await;
    ask(&mut harness, "hello", "Hi.").await;
    assert_eq!(harness.model.briefings(), ["off"]);
}

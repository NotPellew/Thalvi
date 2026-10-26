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
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thalvi_server::model::{ChatMessage, ChatModel, DeltaStream, ModelError};
use thalvi_server::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerFactory};
use thalvi_server::recorder::{RecordKinds, RecordingConfig};
use thalvi_server::session::handle_session;
use thalvi_server::store::SessionStore;
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

    fn push(&self, connection: usize, item: Result<RecognizerEvent, RecognizerError>) {
        let sender = self.inner.connections.lock().unwrap()[connection].clone();
        let _ = sender.send(item);
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

// ---------- fake model ----------

enum ModelChunk {
    Delta(String),
    Wait(Arc<Notify>),
    Fail,
}

struct DropGuard(Arc<AtomicUsize>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
struct FakeModel {
    calls: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    scripts: Arc<Mutex<VecDeque<Vec<ModelChunk>>>>,
    dropped: Arc<AtomicUsize>,
}

impl FakeModel {
    fn add_script(&self, chunks: Vec<ModelChunk>) {
        self.scripts.lock().unwrap().push_back(chunks);
    }

    fn calls(&self) -> Vec<Vec<ChatMessage>> {
        self.calls.lock().unwrap().clone()
    }
}

impl ChatModel for FakeModel {
    async fn stream_reply(&self, history: &[ChatMessage]) -> Result<DeltaStream, ModelError> {
        self.calls.lock().unwrap().push(history.to_vec());
        let chunks = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        let guard = DropGuard(Arc::clone(&self.dropped));
        let stream = futures_util::stream::unfold(
            (VecDeque::from(chunks), Some(guard)),
            |(mut chunks, guard)| async move {
                match chunks.pop_front() {
                    Some(ModelChunk::Delta(text)) => Some((Ok(text), (chunks, guard))),
                    Some(ModelChunk::Wait(notify)) => {
                        notify.notified().await;
                        Some((Ok(String::new()), (chunks, guard)))
                    }
                    Some(ModelChunk::Fail) => Some((
                        Err(ModelError("model blew up".to_string())),
                        (chunks, guard),
                    )),
                    None => None,
                }
            },
        )
        .filter(|item| std::future::ready(!matches!(item, Ok(text) if text.is_empty())));
        Ok(Box::pin(stream))
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
}

impl Tts for FakeTts {
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
    let first = recv(client).await;
    let Received::Json(session) = &first else {
        panic!("first message was not the session message: {first:?}");
    };
    assert_eq!(session["type"], "session");
    let id = session["id"].as_str().unwrap().to_string();
    let turns = session["turns"].clone();
    let second = recv(client).await;
    assert!(
        matches!(&second, Received::Json(v) if v["type"] == "state" && v["state"] == "idle"),
        "second message was not idle state: {second:?}"
    );
    (id, turns)
}

async fn start() -> Harness {
    start_with_recording(RecordingConfig::disabled()).await
}

async fn start_with_recording(recording: RecordingConfig) -> Harness {
    let factory = FakeFactory::default();
    let model = FakeModel::default();
    let tts = FakeTts::default();
    let store = Arc::new(SessionStore::new());
    let (factory2, model2, tts2, store2) = (
        factory.clone(),
        model.clone(),
        tts.clone(),
        Arc::clone(&store),
    );
    let recording2 = recording.clone();
    let app = Router::new().route(
        "/session",
        get(
            move |upgrade: WebSocketUpgrade,
                  axum::extract::Query(query): axum::extract::Query<
                std::collections::HashMap<String, String>,
            >| {
                let (factory, model, tts, store) = (
                    factory2.clone(),
                    model2.clone(),
                    tts2.clone(),
                    Arc::clone(&store2),
                );
                let recording = recording2.clone();
                async move {
                    let resume = query.get("resume").cloned();
                    upgrade.on_upgrade(move |socket: WebSocket| {
                        handle_session(
                            socket,
                            factory,
                            Arc::new(model),
                            Arc::new(tts),
                            store,
                            resume,
                            recording,
                        )
                    })
                }
            },
        ),
    );
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
        addr,
        server,
    }
}

async fn voice_on(harness: &mut Harness) {
    send_json(harness, &json!({"type": "voice", "on": true})).await;
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
    assert!(has_state(&received, "starting"));

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
    let voice_off = recv(&mut harness.client).await;
    assert!(
        matches!(&voice_off, Received::Json(v) if v["type"] == "voice" && v["on"] == false),
        "a fatal recognizer error tells the client voice is off, got {voice_off:?}"
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
    send_json(&mut harness, &json!({"type": "voice", "on": true})).await;
    let received = collect_until(&mut harness.client, |item| {
        matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "starting")
    })
    .await;
    assert!(has_state(&received, "starting"));
    assert!(!has_state(&received, "listening"));

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

    send_json(&mut harness, &json!({"type": "voice", "on": false})).await;
    let off_state = recv(&mut harness.client).await;
    assert!(
        matches!(&off_state, Received::Json(v) if v["type"] == "state" && v["state"] == "thinking"),
        "expected thinking after voice off mid-reply, got {off_state:?}"
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
    wait_until(|| harness.store.contains(&first_id)).await;

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
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self(std::env::temp_dir().join(format!("thalvi-record-test-{tag}-{nanos}")))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    wait_until(|| harness.store.contains(&session)).await;
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
    wait_until(|| harness.store.contains(&session)).await;
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
    wait_until(|| harness.store.contains(&session)).await;

    let turns = harness.reconnect(Some(&session)).await;
    assert_eq!(turns.as_array().map(Vec::len), Some(2));
    run_voice_turn(&mut harness).await;
    harness.client.close(None).await.unwrap();
    wait_until(|| harness.store.contains(&session)).await;
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
async fn typed_turn_speech_start_and_cancel_interrupt_audio_the_client_may_still_play() {
    for trigger in ["typed", "speech start", "cancel"] {
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
        // The reply is idle between sentences, then finishes: the server cannot
        // know when the client stops playing, so audio was sent and may still play.
        gap.notify_one();
        let received = collect_until(&mut harness.client, |item| {
            matches!(item, Received::Json(v) if v["type"] == "state" && v["state"] == "listening")
        })
        .await;
        assert_eq!(audios(&received), vec![Bytes::from("AUDIO:Two.")]);

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
        let interrupted = recv(&mut harness.client).await;
        assert!(
            matches!(&interrupted, Received::Json(v) if v["type"] == "interrupted"),
            "{trigger}: expected interrupted first, got {interrupted:?}"
        );
        let resting = recv(&mut harness.client).await;
        assert!(
            matches!(&resting, Received::Json(v) if v["type"] == "state" && v["state"] == "listening"),
            "{trigger}: interrupted must be followed by the resting state, got {resting:?}"
        );

        if trigger == "cancel" {
            send_json(&mut harness, &json!({"type": "cancel"})).await;
            expect_quiet_until_marker(&mut harness).await;
        }
        harness.server.abort();
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
    wait_until(|| harness.store.contains(&first_id)).await;

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
    send_json(&mut harness, &json!({"type": "voice", "on": false})).await;
    let off_state = recv(&mut harness.client).await;
    assert!(
        matches!(&off_state, Received::Json(v) if v["type"] == "state" && v["state"] == "thinking"),
        "expected thinking after voice off mid-reply, got {off_state:?}"
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
    send_json(&mut harness, &json!({"type": "voice", "on": true})).await;
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
    send_json(&mut harness, &json!({"type": "voice", "on": true})).await;
    expect_quiet_until_marker(&mut harness).await;

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

    send_json(&mut harness, &json!({"type": "voice", "on": false})).await;
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

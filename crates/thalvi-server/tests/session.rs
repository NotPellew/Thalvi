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

    fn complete_turn(&self, connection: usize, text: &str) {
        self.push(
            connection,
            Ok(RecognizerEvent::SpeechComplete {
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
            harness.factory.push(0, Ok(RecognizerEvent::SpeechStart));
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

        let extra = tokio::time::timeout(Duration::from_millis(300), harness.client.next()).await;
        assert!(
            matches!(extra, Err(_) | Ok(None | Some(Ok(ClientMessage::Ping(_))))),
            "unexpected message after interrupt: {extra:?}"
        );
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
    let retried = tokio::time::timeout(Duration::from_millis(400), async {
        while harness.factory.attempts() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        retried.is_err(),
        "fatal recognizer error must not reconnect"
    );
    assert_eq!(harness.factory.attempts(), 1);
    harness.server.abort();
}

#[tokio::test]
async fn next_turn_history_includes_the_cut_off_reply_text() {
    let mut harness = start().await;
    voice_on(&mut harness).await;
    harness.model.add_script(vec![ModelChunk::Delta(
        "Answer one. Answer two is longer.".to_string(),
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

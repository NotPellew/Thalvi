//! Real STT, model, and TTS clients against local fake servers.
#![expect(clippy::unwrap_used, reason = "test setup failures panic immediately")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thalvi_server::model::{ChatMessage, ChatModel, OpenAiChatModel};
use thalvi_server::recognizer::{
    MetaRecognizerFactory, Recognizer, RecognizerError, RecognizerEvent, RecognizerFactory,
};
use thalvi_server::tts::{OpenAiTts, Qwen3Tts, Tts};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

#[tokio::test]
async fn meta_recognizer_handshake_events_audio_and_policy_close() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let init = socket.next().await.unwrap().unwrap();
        let init_text = init.to_text().unwrap().to_string();
        let init_json: Value = serde_json::from_str(&init_text).unwrap();
        assert_eq!(init_json["authorization"]["accessToken"], "Bearer test-key");
        assert_eq!(init_json["audioEncoding"], "PCM_24KHZ");
        assert_eq!(init_json["model"], "muse-voice");
        assert_eq!(init_json["mode"], "ENDPOINTING");
        socket
            .send(WsMessage::Text(r#"{"sessionId":"session-1"}"#.into()))
            .await
            .unwrap();
        socket
            .send(WsMessage::Text(
                r#"{"type":"transcript","transcript":"hel","final":false}"#.into(),
            ))
            .await
            .unwrap();
        socket
            .send(WsMessage::Text(
                r#"{"type":"speechComplete","turnId":"1","transcript":"hello"}"#.into(),
            ))
            .await
            .unwrap();
        let audio = socket.next().await.unwrap().unwrap();
        assert_eq!(audio.into_data(), vec![1_u8, 2, 3, 4]);
        socket
            .send(WsMessage::Close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "bad request".into(),
            })))
            .await
            .unwrap();
    });

    let factory = MetaRecognizerFactory::new(
        format!("ws://{addr}"),
        "muse-voice".to_string(),
        "test-key".to_string(),
    );
    let mut recognizer = tokio::time::timeout(Duration::from_secs(5), factory.connect())
        .await
        .unwrap()
        .unwrap();
    let first = recognizer.next_event().await.unwrap().unwrap();
    assert_eq!(
        first,
        RecognizerEvent::Partial {
            text: "hel".to_string()
        }
    );
    let second = recognizer.next_event().await.unwrap().unwrap();
    assert_eq!(
        second,
        RecognizerEvent::SpeechComplete {
            turn_id: "1".to_string(),
            text: "hello".to_string()
        }
    );
    recognizer.send_audio(&[1, 2, 3, 4]).await.unwrap();
    let closed = recognizer.next_event().await.unwrap();
    assert!(
        matches!(closed, Err(RecognizerError::Fatal(_))),
        "1008 must be fatal, got {closed:?}"
    );
    server.await.unwrap();
}

#[derive(Clone, Default)]
struct Captured {
    bodies: Arc<Mutex<Vec<Value>>>,
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn chat_model_parses_sse_until_done_and_sends_stream_request() {
    let captured = Captured::default();
    let captured2 = captured.clone();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move |body: String| {
                let captured = captured2.clone();
                async move {
                    captured
                        .bodies
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&body).unwrap());
                    // The second event is split inside the two bytes of "é".
                    let sse = concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"café.\"}}]}\n\n",
                        "data: [DONE]\n\n",
                    )
                    .as_bytes();
                    let split = sse.iter().position(|&byte| byte == 0xC3).unwrap() + 1;
                    let chunks = vec![
                        Ok::<Vec<u8>, std::io::Error>(sse[..split].to_vec()),
                        Ok(sse[split..].to_vec()),
                    ];
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(futures_util::stream::iter(chunks)))
                        .unwrap()
                }
            }),
        )
        .with_state(());
    let base = serve(app).await;
    let model = OpenAiChatModel::new(
        base,
        "test-model".to_string(),
        String::new(),
        Some("minimal".to_string()),
    );
    let mut stream = model
        .stream_reply(&[ChatMessage::user("hi".to_string())])
        .await
        .unwrap();
    let mut deltas = Vec::new();
    while let Some(delta) = stream.next().await {
        deltas.push(delta.unwrap());
    }
    assert_eq!(deltas, vec!["Hello ".to_string(), "café.".to_string()]);
    let bodies = captured.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["stream"], true);
    assert_eq!(bodies[0]["model"], "test-model");
    assert_eq!(bodies[0]["reasoning_effort"], "minimal");
    assert_eq!(
        bodies[0]["messages"][1],
        json!({"role": "user", "content": "hi"})
    );
}

#[tokio::test]
async fn tts_client_sends_contract_body_and_streams_pcm() {
    let captured = Captured::default();
    let captured2 = captured.clone();
    let app = Router::new().route(
        "/v1/audio/speech",
        post(move |body: String| {
            let captured = captured2.clone();
            async move {
                captured
                    .bodies
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&body).unwrap());
                let chunks = vec![Ok::<Vec<u8>, std::io::Error>(vec![1, 2]), Ok(vec![3, 4, 5])];
                Response::builder()
                    .header("content-type", "audio/pcm")
                    .body(Body::from_stream(futures_util::stream::iter(chunks)))
                    .unwrap()
            }
        }),
    );
    let base = serve(app).await;
    let tts = OpenAiTts::new(
        base,
        "kokoro".to_string(),
        "af_heart".to_string(),
        String::new(),
    );
    let mut stream = tts.synthesize("Hello there.").await.unwrap();
    let mut audio = Vec::new();
    while let Some(chunk) = stream.next().await {
        audio.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(audio, vec![1_u8, 2, 3, 4, 5]);
    let bodies = captured.bodies.lock().unwrap();
    assert_eq!(
        bodies[0],
        json!({
            "model": "kokoro",
            "voice": "af_heart",
            "input": "Hello there.",
            "response_format": "pcm",
            "stream_format": "audio",
        })
    );
}

fn frame(pcm: &[u8]) -> Vec<u8> {
    let mut framed = u32::try_from(pcm.len()).unwrap().to_be_bytes().to_vec();
    framed.extend_from_slice(pcm);
    framed
}

#[tokio::test]
async fn qwen3_tts_client_decodes_framed_pcm_as_it_arrives() {
    let end = frame(&[]);
    let both = [frame(&[1, 2, 3, 4]), frame(&[5, 6])].concat();
    // Chunks the fake server sends, and the frames (Err = stream failure) the client yields.
    let cases = [
        (
            "frames split across chunks",
            StatusCode::OK,
            vec![
                both[..2].to_vec(),
                both[2..7].to_vec(),
                [&both[7..], &end[..1]].concat(),
                end[1..].to_vec(),
            ],
            Ok(vec![Ok(vec![1, 2, 3, 4]), Ok(vec![5, 6])]),
        ),
        (
            "several frames and the end frame in one chunk",
            StatusCode::OK,
            vec![[both.clone(), end.clone(), frame(&[9])].concat()],
            Ok(vec![Ok(vec![1, 2, 3, 4]), Ok(vec![5, 6])]),
        ),
        (
            "stream ends without the end frame",
            StatusCode::OK,
            vec![frame(&[1, 2]), frame(&[3, 4])[..3].to_vec()],
            Ok(vec![Ok(vec![1, 2]), Err(())]),
        ),
        (
            "error status",
            StatusCode::BAD_REQUEST,
            vec![br#"{"detail":"Unknown voice"}"#.to_vec()],
            Err(()),
        ),
    ];
    for (name, status, chunks, expected) in cases {
        let captured = Captured::default();
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/audio/speech/pcm-stream",
            post(move |body: String| {
                let captured = captured2.clone();
                async move {
                    captured
                        .bodies
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&body).unwrap());
                    let chunks = chunks.into_iter().map(Ok::<Vec<u8>, std::io::Error>);
                    Response::builder()
                        .status(status)
                        .body(Body::from_stream(futures_util::stream::iter(chunks)))
                        .unwrap()
                }
            }),
        );
        let tts = Qwen3Tts::new(serve(app).await, "vc_test".to_string());
        let outcome = match tts.synthesize("Hello there.").await {
            Ok(stream) => Ok(stream
                .map(|item| item.map(|pcm| pcm.to_vec()).map_err(|_| ()))
                .collect::<Vec<_>>()
                .await),
            Err(_) => Err(()),
        };
        assert_eq!(outcome, expected, "{name}");
        assert_eq!(
            captured.bodies.lock().unwrap()[..],
            [json!({"input": "Hello there.", "voice": "vc_test"})],
            "{name}"
        );
    }
}

//! Exercises local writing through a fake child and a real loopback HTTP endpoint.

use super::*;
use crate::chat::{Build, Chat, Files, fakes::FakeLaunch};
use crate::engine::fakes::{FakeCuePlayer, FakeWake};
use crate::models::{Asset, ModelKind, ModelStore};
use crate::recognizer::FakeRecognizer;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use std::sync::Mutex;

/// The requests the fake child's endpoint was sent.
type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn answer(
    State(asked): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ([(&'static str, &'static str); 1], &'static str) {
    asked.lock().unwrap().push((headers, body));
    (
        [("content-type", "text/event-stream")],
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello.\"}}]}\n\ndata: [DONE]\n\n",
    )
}

/// A chat-completions endpoint on loopback that answers "Hello." and keeps what it was sent.
async fn endpoint() -> (u16, Requests, tokio::task::JoinHandle<()>) {
    let asked = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = Router::new()
        .route("/v1/chat/completions", post(answer))
        .with_state(Arc::clone(&asked));
    let http = tokio::spawn(async {
        axum::serve(listener, router).await.unwrap();
    });
    (port, asked, http)
}

#[tokio::test]
async fn writing_uses_the_owned_runtime_and_a_second_crash_stays_failed() {
    let (port, asked, http) = endpoint().await;
    let dir = std::env::temp_dir().join(format!("thalvi-engine-chat-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("runtime")).unwrap();
    std::fs::write(dir.join("runtime/llama-server"), "fake").unwrap();
    std::fs::write(dir.join("model.gguf"), "gguf").unwrap();
    let files = Files {
        runtime: Build::Cpu.files().runtime,
        folder: "runtime",
        model: Asset {
            kind: ModelKind::ChatModel,
            file_name: "model.gguf",
            url: "http://127.0.0.1:9/unused",
            size: 4,
            sha256: "unused",
        },
    };
    let (launch, child) = FakeLaunch::new();
    child.listen_on(port);
    let chat = Chat::new(
        ModelStore::in_dir(dir.clone()),
        Build::Cpu,
        files,
        Box::new(launch),
    );
    let config = DesktopConfig::from_json_str(r#"{"writing":{"local":true}}"#).unwrap();
    let (recognizer, _) = FakeRecognizer::new();
    let (wake, _) = FakeWake::new();
    let (cues, _) = FakeCuePlayer::new();
    let (_key, hold) = watch::channel(crate::hold::KeyState::default());
    let (mut engine, _) = Engine::new(
        Parts {
            recognizer,
            wake,
            cues,
            gate: InputGate::default(),
            http: reqwest::Client::new(),
            chat: Some(chat),
            hold,
            notice: Box::new(|_| {}),
            spotter: None,
            spotter_off: None,
        },
        &config,
        Recorder::disabled(),
    );
    assert_eq!(
        engine.write("hello".to_owned()).await,
        CaptureOutcome::Accepted(CaptureRequest {
            route: CaptureRoute::Paste,
            text: "Hello.".to_owned()
        })
    );
    {
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let spawned = child.spawned(0);
        for (headers, body) in asked.iter() {
            assert_eq!(
                headers["authorization"],
                format!("Bearer {}", spawned.key().unwrap())
            );
            assert_eq!(body["model"], "model.gguf");
        }
        assert_eq!(
            asked[0].1["messages"][0]["content"],
            writing::DEFAULT_PROMPT
        );
    }
    assert_eq!(child.spawns(), 1);
    child.exit(0);
    assert!(matches!(
        engine.write("hello".to_owned()).await,
        CaptureOutcome::Accepted(_)
    ));
    child.exit(1);
    assert!(matches!(
        engine.write("hello".to_owned()).await,
        CaptureOutcome::Failed {
            error: CaptureError::Writing(WritingError::Stopped),
            ..
        }
    ));
    assert_eq!(child.spawns(), 2);
    engine.remove_chat().await.unwrap();
    assert!(child.killed(1));
    assert!(!dir.join("model.gguf").exists());
    drop(engine);
    http.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

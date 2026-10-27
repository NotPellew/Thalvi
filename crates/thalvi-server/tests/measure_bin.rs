//! Runs the `measure` binary against fake model and speech servers.
#![expect(clippy::unwrap_used, reason = "test setup failures panic immediately")]

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::post;
use serde_json::Value;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn results_are_named_and_dated_by_when_the_run_started() {
    let first_request: Arc<Mutex<Option<u128>>> = Arc::default();
    let first_request2 = Arc::clone(&first_request);
    let chat = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let first_request = Arc::clone(&first_request2);
            async move {
                first_request.lock().unwrap().get_or_insert_with(now_ms);
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Hi there.\"}}]}\n\ndata: [DONE]\n\n",
                    ))
                    .unwrap()
            }
        }),
    );
    let speech = Router::new().route(
        "/v1/audio/speech",
        post(|| async { Response::new(Body::from(vec![0_u8; 4800])) }),
    );
    let (chat_url, speech_url) = (serve(chat).await, serve(speech).await);
    let dir = std::env::temp_dir().join(format!("thalvi-measure-bin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_measure"))
        .args(["--runs", "1", "--only", "hello", "--no-judge"])
        .env_clear()
        .current_dir(&dir)
        .envs([
            ("THALVI_ADDR", "127.0.0.1:0"),
            ("THALVI_WEB_DIR", "web"),
            ("LLM_BASE_URL", chat_url.as_str()),
            ("LLM_MODEL", "m"),
            ("LLM_API_KEY", ""),
            ("STT_URL", "ws://127.0.0.1:1"),
            ("STT_MODEL", "s"),
            ("STT_API_KEY", ""),
            ("TTS_API", "openai"),
            ("TTS_BASE_URL", speech_url.as_str()),
            ("TTS_MODEL", "t"),
            ("TTS_VOICE", "v"),
            ("TTS_API_KEY", ""),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let files: Vec<_> = std::fs::read_dir(dir.join("data/measure"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let results: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    let started = u128::from(results["started_unix_ms"].as_u64().unwrap());
    assert_eq!(
        files[0].file_name().unwrap().to_str().unwrap(),
        format!("run-{started}.json")
    );
    assert_eq!(results["samples"].as_array().unwrap().len(), 1);
    assert_eq!(results["samples"][0]["error"], Value::Null);
    let first_request = first_request.lock().unwrap().unwrap();
    assert!(
        started <= first_request,
        "started {started} is after the first request {first_request}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

//! Exercises the desktop transcription-only route against a local ASR WebSocket.
#![expect(clippy::unwrap_used, reason = "test setup failures panic immediately")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thalvi_server::config::Config;
use tokio_tungstenite::tungstenite::Message;

static NEXT: AtomicUsize = AtomicUsize::new(0);

async fn serve(stt: &str) -> (String, std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let dir = std::env::temp_dir().join(format!(
        "thalvi-transcription-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let vars: std::collections::HashMap<_, _> = [
        ("THALVI_ADDR", "127.0.0.1:0".to_string()),
        ("THALVI_WEB_DIR", "web/dist".to_string()),
        ("THALVI_DATA_DIR", dir.display().to_string()),
        ("LLM_BASE_URL", "http://127.0.0.1:1/v1".to_string()),
        ("LLM_MODEL", "must-not-be-called".to_string()),
        ("LLM_API_KEY", String::new()),
        ("STT_URL", stt.to_string()),
        ("STT_MODEL", "fake-asr".to_string()),
        ("STT_API_KEY", "fake-key".to_string()),
        ("TTS_API", "openai".to_string()),
        ("TTS_BASE_URL", "http://127.0.0.1:1/v1".to_string()),
        ("TTS_MODEL", "must-not-be-called".to_string()),
        ("TTS_VOICE", "fake".to_string()),
        ("TTS_API_KEY", String::new()),
    ]
    .into_iter()
    .collect();
    let config = Config::from_lookup(|key| vars.get(key).cloned()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = thalvi_server::server::router(&config).unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("ws://{address}/transcription"), dir, server)
}

#[tokio::test]
async fn capture_returns_incremental_text_without_answering_and_accepts_a_typed_cancel() {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let asr = tokio::spawn(async move {
        let (stream, _) = upstream.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let init: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(init["model"], "fake-asr");
        socket
            .send(Message::Text(r#"{"sessionId":"fake"}"#.into()))
            .await
            .unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap().into_data(),
            vec![0_u8, 1, 2, 3]
        );
        for event in [
            json!({"type":"transcript","transcript":"first partial"}),
            json!({"type":"speechComplete","turnId":"1","transcript":"first piece"}),
            json!({"type":"speechComplete","turnId":"2","transcript":"second piece paste that"}),
        ] {
            socket
                .send(Message::Text(event.to_string().into()))
                .await
                .unwrap();
        }
        let end = socket.next().await.unwrap().unwrap();
        assert_eq!(end.to_text().unwrap(), r#"{"type":"endStream"}"#);
    });
    let (url, dir, server) = serve(&format!("ws://{address}")).await;
    let (mut client, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let ready = client.next().await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(ready.to_text().unwrap()).unwrap(),
        json!({"type":"ready"})
    );
    client
        .send(Message::Binary(vec![0, 1, 2, 3].into()))
        .await
        .unwrap();
    let mut events = Vec::new();
    for _ in 0..3 {
        let event = tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        events.push(serde_json::from_str::<Value>(event.to_text().unwrap()).unwrap());
    }
    assert_eq!(
        events,
        [
            json!({"type":"partial","text":"first partial"}),
            json!({"type":"final","turn_id":"1","text":"first piece"}),
            json!({"type":"final","turn_id":"2","text":"second piece paste that"})
        ]
    );
    client
        .send(Message::Text("{ \"type\" : \"cancel\" }".into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), asr)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn malformed_audio_and_upstream_failure_are_reported_without_a_completed_capture() {
    for audio in [vec![1_u8], Vec::new()] {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let asr = tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.next().await.unwrap().unwrap();
            socket
                .send(Message::Text(r#"{"sessionId":"fake"}"#.into()))
                .await
                .unwrap();
            let end = socket.next().await.unwrap().unwrap();
            assert_eq!(end.to_text().unwrap(), r#"{"type":"endStream"}"#);
        });
        let (url, dir, server) = serve(&format!("ws://{address}")).await;
        let (mut client, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        client.next().await.unwrap().unwrap();
        client.send(Message::Binary(audio.into())).await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let event: Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
        assert_eq!(event["type"], "error");
        assert!(event["message"].as_str().unwrap().contains("24 kHz"));
        asr.await.unwrap();
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }
    let (url, dir, server) = serve("ws://127.0.0.1:1").await;
    let (mut client, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let event: Value = serde_json::from_str(event.to_text().unwrap()).unwrap();
    assert_eq!(event["type"], "error");
    server.abort();
    let _ = std::fs::remove_dir_all(dir);
}

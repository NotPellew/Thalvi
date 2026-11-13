//! The chosen recognizer hands audio to the entry's session and ends it when asked, and only an on-device entry has a model to preload.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thalvi_desktop::chosen::{Chosen, ChosenSession};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::device::Adapters;
use thalvi_desktop::frame::FRAME_SAMPLES;
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::{Recognizer, RecognizerSession, UPSAMPLED_SAMPLES};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(5);

fn chosen(entry: &Value) -> Chosen {
    let config = DesktopConfig::from_json_str(&json!({"stt": {"entries": [entry]}}).to_string())
        .expect("config");
    Chosen::new(
        config.stt.entries.active_live(),
        ModelStore::in_dir(env!("CARGO_TARGET_TMPDIR")),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
        &WordList::new(Vec::new()).expect("list"),
        Adapters::default(),
    )
}

fn realtime(address: &str) -> Value {
    json!({"kind": "realtime", "id": "r", "name": "Cybrex2", "active": true,
           "address": address, "model": "nemotron"})
}

/// Serves one connection that reports the audio bytes it got and how the client ended it.
async fn serve() -> (String, tokio::sync::mpsc::UnboundedReceiver<Value>) {
    let (seen_tx, seen) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = format!("ws://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("upgrade");
        socket.next().await.expect("init").expect("init");
        socket
            .send(Message::Text(json!({"sessionId": "s"}).to_string().into()))
            .await
            .expect("ready");
        while let Some(Ok(message)) = socket.next().await {
            let seen = match message {
                Message::Binary(audio) => json!({"audio_bytes": audio.len()}),
                Message::Text(end) => serde_json::from_str(&end).expect("json"),
                _ => continue,
            };
            seen_tx.send(seen).expect("report");
        }
        seen_tx.send(json!("hung up")).expect("report");
    });
    (address, seen)
}

#[tokio::test]
async fn audio_reaches_the_entry_and_closing_or_cancelling_ends_its_stream() {
    type End = fn(ChosenSession) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;
    let ends: [(&str, End); 2] = [
        ("close", |session| Box::pin(session.close())),
        ("cancel", |session| Box::pin(session.cancel())),
    ];
    for (how, end) in ends {
        let (address, mut seen) = serve().await;
        let mut session = tokio::time::timeout(WAIT, chosen(&realtime(&address)).connect())
            .await
            .expect("connects")
            .expect("ready");
        session
            .send_frame(&[100; FRAME_SAMPLES])
            .await
            .expect("send");
        end(session).await;
        let mut next = async || tokio::time::timeout(WAIT, seen.recv()).await.expect("seen");
        assert_eq!(
            next().await,
            Some(json!({"audio_bytes": UPSAMPLED_SAMPLES * 2})),
            "{how}"
        );
        assert_eq!(next().await, Some(json!({"type": "endStream"})), "{how}");
    }
}

#[test]
fn only_an_on_device_entry_has_a_model_to_preload() {
    let on_device = json!({"kind": "on_device", "id": "d", "name": "Device", "active": true});
    assert!(chosen(&on_device).local().is_some());
    assert!(chosen(&realtime("ws://127.0.0.1:9")).local().is_none());
}

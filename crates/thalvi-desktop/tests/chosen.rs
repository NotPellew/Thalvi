//! The chosen recognizer hands audio to the entry's session and ends it when asked, and only an on-device entry has a model to preload.

#![cfg(test)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thalvi_desktop::chosen::{Chosen, ChosenSession, Preload};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::device::Adapters;
use thalvi_desktop::frame::FRAME_SAMPLES;
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::{
    RELEASE_LIMIT, Recognizer, RecognizerEvent, RecognizerSession, UPSAMPLED_SAMPLES,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        std::path::Path::new("unused"),
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
                Message::Close(_) => json!("close frame"),
                _ => continue,
            };
            seen_tx.send(seen).expect("report");
        }
        seen_tx.send(json!("hung up")).expect("report");
    });
    (address, seen)
}

type End = fn(ChosenSession) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

const ENDS: [(&str, End); 2] = [
    ("close", |session| Box::pin(session.close())),
    ("cancel", |session| Box::pin(session.cancel())),
];

#[tokio::test]
async fn audio_reaches_the_entry_and_closing_or_cancelling_ends_its_stream() {
    for (how, end) in ENDS {
        let (address, mut seen) = serve().await;
        let mut session = tokio::time::timeout(WAIT, chosen(&realtime(&address)).connect())
            .await
            .expect("connects")
            .expect("ready");
        assert_eq!(session.final_wait(), Duration::from_secs(11));
        assert_eq!(session.release_limit(), RELEASE_LIMIT);
        assert!(
            !session.behind(),
            "a streaming entry owes nothing at its release"
        );
        assert!(
            !session.buffers_audio(),
            "realtime audio streams as it arrives"
        );
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
        assert_eq!(next().await, Some(json!("close frame")), "{how}");
    }
}

/// Serves one transcription request: reports it, then answers it when `answer` fires, or reports
/// that the client hung up first.
async fn serve_transcription() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (seen_tx, seen) = tokio::sync::mpsc::unbounded_channel();
    let (answer, answered) = tokio::sync::oneshot::channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = format!("http://{}/v1", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        let complete = |request: &[u8]| {
            let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                return false;
            };
            let length = String::from_utf8_lossy(&request[..end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .expect("a sized body")
                .parse::<usize>()
                .expect("a length");
            request.len() >= end + 4 + length
        };
        while !complete(&request) {
            let read = stream.read(&mut chunk).await.expect("read");
            assert!(read > 0, "the client hung up mid-request");
            request.extend_from_slice(&chunk[..read]);
        }
        seen_tx.send("request").expect("report");
        tokio::select! {
            _ = answered => {
                let body = json!({"text": "Heard."}).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("answer");
            }
            read = stream.read(&mut chunk) => {
                assert!(matches!(read, Ok(0) | Err(_)), "nothing follows the request");
                seen_tx.send("hung up").expect("report");
            }
        }
    });
    (address, seen, answer)
}

#[tokio::test]
async fn a_buffered_entry_sends_its_audio_once_finished_and_closing_or_cancelling_drops_the_request()
 {
    let entry = |address: &str| {
        json!({"kind": "open_ai", "id": "o", "name": "Cloud", "active": true,
               "address": address, "model": "whisper-1"})
    };
    for end in [None, Some(ENDS[0]), Some(ENDS[1])] {
        let how = end.map_or("answer", |(how, _)| how);
        let (address, mut seen, answer) = serve_transcription().await;
        let mut session = chosen(&entry(&address)).connect().await.expect("connects");
        assert!(session.buffers_audio(), "{how}");
        session
            .send_frame(&[100; FRAME_SAMPLES])
            .await
            .expect("send");
        session.finish_audio().await.expect("finish");
        let Some((_, end)) = end else {
            let heard = tokio::spawn(async move { session.next_event().await });
            let request = tokio::time::timeout(WAIT, seen.recv()).await;
            assert_eq!(
                request.expect("the finished capture is sent"),
                Some("request")
            );
            answer.send(()).expect("answer");
            let heard = tokio::time::timeout(WAIT, heard).await.expect("heard");
            assert_eq!(
                heard.expect("task").expect("final"),
                RecognizerEvent::Final("Heard.".to_owned())
            );
            continue;
        };
        tokio::select! {
            event = session.next_event() => panic!("{how}: answered before the server: {event:?}"),
            request = tokio::time::timeout(WAIT, seen.recv()) => {
                assert_eq!(request.expect("the finished capture is sent"), Some("request"), "{how}");
            }
        }
        end(session).await;
        let ended = tokio::time::timeout(WAIT, seen.recv()).await;
        assert_eq!(
            ended.expect("the request is dropped"),
            Some("hung up"),
            "{how}"
        );
        drop(answer);
    }
}

#[test]
fn only_an_on_device_entry_has_a_preloadable_recognizer_and_a_whisper_one_is_named_by_its_model() {
    let on_device = json!({"kind": "on_device", "id": "d", "name": "Device", "active": true});
    assert!(chosen(&on_device).local().is_some());
    assert!(chosen(&realtime("ws://127.0.0.1:9")).local().is_none());
    for model in ["whisper_turbo_cpu", "whisper_turbo_vulkan"] {
        let chosen = chosen(&json!({"kind": "on_device", "id": "w", "name": "Whisper",
                                    "active": true, "model": model}));
        let local = chosen.local().expect("an on-device entry loads a model");
        assert_eq!(local.name(), model);
        assert_eq!(
            local.device(),
            None,
            "nothing is loaded before it is asked for"
        );
        assert!(
            chosen.loading().is_some(),
            "{model}: a dictation waits for the load"
        );
    }
}

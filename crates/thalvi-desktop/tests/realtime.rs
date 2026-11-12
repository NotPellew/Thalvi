//! A recognizer reached directly speaks Meta's realtime ASR protocol to a local fake; no Thalvi server runs.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thalvi_desktop::chosen::Chosen;
use thalvi_desktop::device::Adapters;
use thalvi_desktop::entries::{LiveEntry, LiveKind, Realtime};
use thalvi_desktop::frame::FRAME_SAMPLES;
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::realtime::RealtimeRecognizer;
use thalvi_desktop::recognizer::{
    ConnectFailure, Recognizer, RecognizerError, RecognizerEvent, RecognizerSession,
    UPSAMPLED_SAMPLES,
};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const WAIT: Duration = Duration::from_secs(5);
const KEY: &str = "sk-direct-secret";

fn recognizer(address: &str, phrases: &[&str]) -> RealtimeRecognizer {
    let list = WordList::new(phrases.iter().map(ToString::to_string).collect()).expect("list");
    RealtimeRecognizer::new(
        "Cybrex2",
        &Realtime::new(address, "nemotron", KEY.to_owned()).expect("entry"),
        &list,
    )
}

fn text(value: &Value) -> Message {
    Message::Text(value.to_string().into())
}

/// Serves one connection with `script` and returns its address.
async fn serve<F, Fut>(script: F) -> String
where
    F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = format!("ws://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        script(
            tokio_tungstenite::accept_async(stream)
                .await
                .expect("upgrade"),
        )
        .await;
    });
    address
}

#[tokio::test]
async fn a_recognizer_is_used_directly_with_its_key_model_word_list_audio_and_events() {
    let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let address = serve(move |mut socket| async move {
        let init: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        seen_tx.send(init).unwrap();
        socket
            .send(text(&json!({"sessionId": "s", "phraseBoosting": true})))
            .await
            .unwrap();
        let phrases = socket.next().await.unwrap().unwrap();
        seen_tx
            .send(serde_json::from_str(phrases.to_text().unwrap()).unwrap())
            .unwrap();
        let audio = socket.next().await.unwrap().unwrap().into_data();
        seen_tx.send(json!({"audio_bytes": audio.len()})).unwrap();
        for frame in [
            json!({"type": "speechStart", "turnId": "1"}),
            json!({"type": "transcript", "transcript": "hel"}),
            json!({"type": "speechComplete", "turnId": "1", "transcript": "hello"}),
            json!({"type": "error", "message": "The model went away."}),
        ] {
            socket.send(text(&frame)).await.unwrap();
        }
        let end = socket.next().await.unwrap().unwrap();
        seen_tx
            .send(serde_json::from_str(end.to_text().unwrap()).unwrap())
            .unwrap();
    })
    .await;

    let recognizer = recognizer(&address, &["Cybrex2", "Thalvi"]);
    let mut session = tokio::time::timeout(WAIT, recognizer.connect())
        .await
        .expect("connects")
        .expect("ready");
    session
        .send_frame(&[100; FRAME_SAMPLES])
        .await
        .expect("send");
    let mut next = async || {
        tokio::time::timeout(WAIT, session.next_event())
            .await
            .expect("event")
    };
    assert_eq!(next().await, Ok(RecognizerEvent::Partial("hel".into())));
    assert_eq!(next().await, Ok(RecognizerEvent::Final("hello".into())));
    let failed = next().await.unwrap_err();
    assert_eq!(
        failed.to_string(),
        format!(
            "The speech server \"Cybrex2\" at {address} reported a problem: The model went away."
        ),
        "the entry is named"
    );
    session.close().await;

    let init = seen.recv().await.unwrap();
    assert_eq!(
        init["authorization"]["accessToken"],
        format!("Bearer {KEY}")
    );
    assert_eq!(init["model"], "nemotron");
    assert_eq!(init["audioEncoding"], "PCM_24KHZ");
    assert_eq!(
        seen.recv().await.unwrap(),
        json!({"type": "phrases", "phrases": ["Cybrex2", "Thalvi"]})
    );
    assert_eq!(
        seen.recv().await.unwrap(),
        json!({"audio_bytes": UPSAMPLED_SAMPLES * 2})
    );
    assert_eq!(seen.recv().await.unwrap(), json!({"type": "endStream"}));
}

#[tokio::test]
async fn a_recognizer_that_does_not_announce_boosting_never_sees_the_word_list() {
    let address = serve(|mut socket| async move {
        socket.next().await.unwrap().unwrap();
        socket.send(text(&json!({"sessionId": "s"}))).await.unwrap();
        let next = socket.next().await.unwrap().unwrap();
        assert!(
            next.is_binary(),
            "the first thing after the handshake is audio: {next:?}"
        );
    })
    .await;
    let mut session = recognizer(&address, &["Cybrex2"])
        .connect()
        .await
        .expect("ready");
    session.send_frame(&[1; FRAME_SAMPLES]).await.expect("send");
    session.close().await;
}

#[tokio::test]
async fn chosen_realtime_entry_does_not_buffer_audio() {
    let address = serve(|mut socket| async move {
        socket.next().await.expect("handshake").expect("message");
        socket
            .send(text(&json!({"sessionId": "s", "phraseBoosting": true})))
            .await
            .expect("ready");
        socket.next().await.expect("phrase list").expect("message");
        let end = socket.next().await.expect("end stream").expect("message");
        assert_eq!(
            serde_json::from_str::<Value>(end.to_text().expect("text")).expect("JSON"),
            json!({"type": "endStream"})
        );
    })
    .await;
    let entry = LiveEntry {
        id: "realtime".to_owned(),
        name: "Realtime speech".to_owned(),
        kind: LiveKind::Realtime(
            Realtime::new(&address, "nemotron", KEY.to_owned()).expect("entry"),
        ),
    };
    let chosen = Chosen::new(
        &entry,
        ModelStore::in_dir("unused"),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
        &WordList::default(),
        Adapters::default(),
    );
    let session = tokio::time::timeout(WAIT, chosen.connect())
        .await
        .expect("connects")
        .expect("ready");
    assert!(
        !session.buffers_audio(),
        "realtime audio streams as it arrives"
    );
    session.close().await;
}

#[tokio::test]
async fn a_failed_entry_says_which_one_and_never_shows_its_key() {
    let hangs_up = serve(|mut socket| async move {
        socket.next().await.unwrap().unwrap();
        socket.send(Message::Close(None)).await.unwrap();
    })
    .await;
    let forbids = serve(|mut socket| async move {
        socket.next().await.unwrap().unwrap();
        let frame = CloseFrame {
            code: CloseCode::Policy,
            reason: "Not allowed.".into(),
        };
        socket.send(Message::Close(Some(frame))).await.unwrap();
    })
    .await;
    let rejects = serve(|mut socket| async move {
        socket.next().await.unwrap().unwrap();
        socket
            .send(text(&json!({"type": "error", "message": "Bad key."})))
            .await
            .unwrap();
    })
    .await;
    let babbles = serve(|mut socket| async move {
        socket.next().await.unwrap().unwrap();
        socket.send(text(&json!({"hello": true}))).await.unwrap();
    })
    .await;

    // Freed only once every fake above holds its port, so the system cannot hand it to one of them.
    let unused = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let closed_port = format!("ws://{}", unused.local_addr().expect("address"));
    drop(unused);
    let mut errors = Vec::new();
    for address in [&closed_port, &hangs_up, &forbids, &rejects, &babbles] {
        let error = tokio::time::timeout(WAIT, recognizer(address, &[]).connect())
            .await
            .expect("finishes")
            .err()
            .expect("connect fails");
        errors.push((address.clone(), error));
    }
    assert!(matches!(
        &errors[0].1,
        RecognizerError::Unreachable {
            reason: ConnectFailure::Refused,
            ..
        }
    ));
    assert!(
        matches!(&errors[1].1, RecognizerError::Dropped { .. }),
        "{:?}",
        errors[1].1
    );
    assert!(
        matches!(&errors[2].1, RecognizerError::Reported { message, .. } if message.contains("Not allowed.")),
        "{:?}",
        errors[2].1
    );
    assert!(
        matches!(&errors[3].1, RecognizerError::Reported { message, .. } if message == "Bad key.")
    );
    assert!(matches!(
        &errors[4].1,
        RecognizerError::NotUnderstood { .. }
    ));
    for (address, error) in &errors {
        let words = error.to_string();
        assert!(words.contains("\"Cybrex2\""), "{words}");
        assert!(words.contains(address), "{words}");
        assert!(!words.contains(KEY) && !format!("{error:?}").contains(KEY));
    }
}

//! The server recognizer speaks the `/transcription` protocol against a local mock socket.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::recognizer::{
    ConnectFailure, FakeCall, FakeRecognizer, Recognizer, RecognizerError, RecognizerEvent,
    RecognizerSession, ServerRecognizer, ServerUrl, ServerUrlError, UPSAMPLED_SAMPLES, Upsampler,
};
use tokio::sync::{Notify, mpsc};

const WAIT: Duration = Duration::from_secs(5);

/// The error a scripted server should produce, given its address.
type Expected = fn(ServerUrl) -> RecognizerError;

async fn serve<F, Fut>(handler: F) -> ServerUrl
where
    F: Fn(WebSocket) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let app = Router::new().route(
        "/transcription",
        get(move |upgrade: WebSocketUpgrade| {
            let handler = handler.clone();
            async move { upgrade.on_upgrade(handler) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    ServerUrl::parse(&format!("ws://{address}")).expect("server url")
}

/// Sends `messages`, then keeps the socket open until the client goes away.
async fn scripted(url: Vec<Message>) -> ServerUrl {
    serve(move |mut socket: WebSocket| {
        let messages = url.clone();
        async move {
            for message in messages {
                if socket.send(message).await.is_err() {
                    return;
                }
            }
            while let Some(Ok(_)) = socket.recv().await {}
        }
    })
    .await
}

fn text(value: &str) -> Message {
    Message::Text(value.to_string().into())
}

fn ready() -> Message {
    text(r#"{"type":"ready"}"#)
}

#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

/// Sends `ready`, then reports everything the client sends.
async fn recording() -> (ServerUrl, mpsc::UnboundedReceiver<Seen>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let url = serve(move |mut socket: WebSocket| {
        let tx = tx.clone();
        async move {
            if socket.send(ready()).await.is_err() {
                return;
            }
            while let Some(Ok(message)) = socket.recv().await {
                let seen = match message {
                    Message::Text(t) => Seen::Text(t.to_string()),
                    Message::Binary(b) => Seen::Binary(b.to_vec()),
                    Message::Close(_) => Seen::Close,
                    _ => continue,
                };
                let _ = tx.send(seen);
            }
        }
    })
    .await;
    (url, rx)
}

async fn connect(url: ServerUrl) -> Result<impl RecognizerSession, RecognizerError> {
    tokio::time::timeout(WAIT, ServerRecognizer::new(url).connect())
        .await
        .expect("connect finishes")
}

fn ramp(index: usize) -> Frame {
    std::array::from_fn(|i| i16::try_from(3 * (index * FRAME_SAMPLES + i)).expect("small ramp"))
}

fn samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect()
}

#[tokio::test]
async fn connect_resolves_only_after_ready_and_audio_follows_it() {
    let (release, accepted) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let url = {
        let (release, accepted) = (Arc::clone(&release), Arc::clone(&accepted));
        serve(move |mut socket: WebSocket| {
            let (release, accepted, tx) = (Arc::clone(&release), Arc::clone(&accepted), tx.clone());
            async move {
                accepted.notify_one();
                release.notified().await;
                let _ = socket.send(ready()).await;
                while let Some(Ok(message)) = socket.recv().await {
                    if let Message::Binary(audio) = message {
                        let _ = tx.send(audio.len());
                    }
                }
            }
        })
        .await
    };
    let recognizer = ServerRecognizer::new(url);
    let mut connecting = pin!(recognizer.connect());
    tokio::select! {
        () = accepted.notified() => {}
        _ = &mut connecting => panic!("connect finished before the server accepted"),
    }

    // A pending connect cannot be observed except by waiting; this bound only ever delays a failure.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut connecting)
            .await
            .is_err(),
        "connect returned before the server sent ready"
    );
    release.notify_one();
    let mut session = tokio::time::timeout(WAIT, connecting)
        .await
        .expect("connect finishes")
        .expect("ready");
    session.send_frame(&ramp(0)).await.expect("send");
    assert_eq!(
        tokio::time::timeout(WAIT, rx.recv()).await.expect("audio"),
        Some(UPSAMPLED_SAMPLES * 2)
    );
}

#[tokio::test]
async fn connect_failures_say_why() {
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let refused =
        ServerUrl::parse(&format!("ws://{}", unused.local_addr().expect("address"))).expect("url");
    drop(unused);

    let not_understood: Expected = |server| RecognizerError::NotUnderstood { server };
    let dropped: Expected = |server| RecognizerError::Dropped { server };
    let cases: [(Vec<Message>, Expected); 6] = [
        (
            vec![text(
                r#"{"type":"error","message":"The model is loading."}"#,
            )],
            |_| RecognizerError::said("The model is loading."),
        ),
        (vec![text(r#"{"type":"error"}"#)], not_understood),
        (
            vec![text(r#"{"type":"partial","text":"hi"}"#)],
            not_understood,
        ),
        (vec![text("not json")], not_understood),
        (vec![Message::Binary(vec![0, 0].into())], not_understood),
        (vec![Message::Close(None)], dropped),
    ];
    for (script, expected) in cases {
        let url = scripted(script.clone()).await;
        let error = connect(url.clone()).await.err().expect("connect fails");
        assert_eq!(error, expected(url), "script {script:?}");
    }
    let error = connect(refused.clone()).await.err().expect("connect fails");
    assert!(
        matches!(&error, RecognizerError::Unreachable { server, reason: ConnectFailure::Refused, .. } if *server == refused),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "Thalvi could not reach the speech server at {refused} (connection refused). Start the Thalvi server, or fix Settings → Speech → Server address."
        )
    );
}

#[tokio::test]
async fn events_arrive_in_order_and_speech_markers_and_pings_are_skipped() {
    let url = scripted(vec![
        Message::Ping(vec![1].into()),
        ready(),
        Message::Ping(vec![2].into()),
        text(r#"{"type":"speech_start","turn_id":"t","audio_processed_ms":0}"#),
        text(r#"{"type":"partial","text":"he"}"#),
        text(r#"{"type":"speech_end","turn_id":"t","audio_processed_ms":80}"#),
        text(r#"{"type":"final","turn_id":"t","text":"hello"}"#),
        text(r#"{"type":"error","message":"The model went away."}"#),
        text(r#"{"type":"error"}"#),
    ])
    .await;
    let mut session = connect(url).await.expect("ready");
    let mut next = async || {
        tokio::time::timeout(WAIT, session.next_event())
            .await
            .expect("event")
    };
    assert_eq!(next().await, Ok(RecognizerEvent::Partial("he".into())));
    assert_eq!(next().await, Ok(RecognizerEvent::Final("hello".into())));
    assert_eq!(
        next().await,
        Err(RecognizerError::said("The model went away."))
    );
    assert_eq!(
        next().await,
        Err(RecognizerError::said("Speech recognition failed."))
    );
}

#[tokio::test]
async fn unusable_messages_after_ready_end_the_stream_with_a_reason() {
    let not_understood: Expected = |server| RecognizerError::NotUnderstood { server };
    let cases: [(Message, Expected); 8] = [
        (text(r#"{"type":"mystery"}"#), not_understood),
        (text(r#"{"type":"partial","text":7}"#), not_understood),
        (text(r#"{"type":"final"}"#), not_understood),
        (text("[]"), not_understood),
        (text("not json"), not_understood),
        (ready(), not_understood),
        (Message::Binary(vec![0, 0].into()), not_understood),
        (Message::Close(None), |server| RecognizerError::Dropped {
            server,
        }),
    ];
    for (message, expected) in cases {
        let url = scripted(vec![ready(), message.clone()]).await;
        let mut session = connect(url.clone()).await.expect("ready");
        let error = tokio::time::timeout(WAIT, session.next_event())
            .await
            .expect("event")
            .expect_err("the stream fails");
        assert_eq!(error, expected(url), "message {message:?}");
    }
}

#[tokio::test]
async fn a_message_is_accepted_at_100_kb_and_refused_past_it() {
    let overhead = r#"{"type":"partial","text":""}"#.len();
    for (size, accepted) in [(100_000, true), (100_001, false)] {
        let filler = "a".repeat(size - overhead);
        let url = scripted(vec![
            ready(),
            text(&format!(r#"{{"type":"partial","text":"{filler}"}}"#)),
        ])
        .await;
        let mut session = connect(url.clone()).await.expect("ready");
        let event = tokio::time::timeout(WAIT, session.next_event())
            .await
            .expect("event");
        assert_eq!(
            event,
            if accepted {
                Ok(RecognizerEvent::Partial(filler))
            } else {
                Err(RecognizerError::Dropped { server: url })
            },
            "{size} bytes"
        );
    }
}

#[tokio::test]
async fn frames_reach_the_server_as_continuous_24_khz_audio() {
    let (url, mut rx) = recording().await;
    let mut session = connect(url).await.expect("ready");
    for index in 0..2 {
        session.send_frame(&ramp(index)).await.expect("send");
    }
    let mut heard = Vec::new();
    for _ in 0..2 {
        let Some(Seen::Binary(bytes)) = tokio::time::timeout(WAIT, rx.recv()).await.expect("audio")
        else {
            panic!("the server only receives audio");
        };
        assert_eq!(bytes.len(), 3_840);
        heard.extend(samples(&bytes));
    }
    // The ramp 3i upsampled by 3/2 is the line 2n - 3 once past the first two samples,
    // across the frame boundary too.
    assert_eq!(heard[..3], [0, 0, 1]);
    for (n, sample) in heard.iter().enumerate().skip(3) {
        assert_eq!(
            i64::from(*sample),
            2 * i64::try_from(n).unwrap() - 3,
            "sample {n}"
        );
    }
}

#[test]
fn upsampler_keeps_constants_and_extremes_in_range() {
    let mut upsampler = Upsampler::default();
    for level in [0, 1_000, -1_000, i16::MAX, i16::MIN] {
        let mut out = Upsampler::default().push(&[level; FRAME_SAMPLES]);
        assert_eq!(out, [level; UPSAMPLED_SAMPLES], "constant {level}");
        out = upsampler.push(&std::array::from_fn(|i| {
            if i % 2 == 0 { i16::MAX } else { i16::MIN }
        }));
        assert_eq!(out.len(), UPSAMPLED_SAMPLES);
    }
}

#[test]
fn upsampler_rounds_to_the_nearest_sample() {
    for (a, b, rounded) in [(1, 0, 1), (-1, 0, -1)] {
        let alternating: Frame = std::array::from_fn(|i| if i % 2 == 0 { a } else { b });
        let out = Upsampler::default().push(&alternating);
        assert_eq!(out[..3], [a, a, rounded], "first pair of {a},{b}");
        assert_eq!(out[3..6], [b, rounded, rounded], "second pair of {a},{b}");
    }
}

#[tokio::test]
async fn cancel_tells_the_server_then_closes_and_close_only_closes() {
    let (url, mut rx) = recording().await;
    connect(url).await.expect("ready").cancel().await;
    let mut seen = Vec::new();
    while let Some(message) = tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("message")
    {
        seen.push(message);
        if seen.last() == Some(&Seen::Close) {
            break;
        }
    }
    assert_eq!(
        seen,
        [Seen::Text(r#"{"type":"cancel"}"#.into()), Seen::Close]
    );

    let (url, mut rx) = recording().await;
    connect(url).await.expect("ready").close().await;
    assert_eq!(
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("message"),
        Some(Seen::Close)
    );
}

#[test]
fn server_urls_are_validated_once_and_normalised() {
    let valid = [
        ("ws://127.0.0.1:8080", "ws://127.0.0.1:8080"),
        ("wss://example.com/", "wss://example.com"),
        ("  ws://localhost:9/  ", "ws://localhost:9"),
    ];
    for (input, expected) in valid {
        let url: ServerUrl = serde_json::from_value(serde_json::json!(input)).expect(input);
        assert_eq!(url.to_string(), expected, "{input}");
    }
    let invalid = [
        ("", ServerUrlError::Invalid),
        ("127.0.0.1:8080", ServerUrlError::Invalid),
        ("http://127.0.0.1:8080", ServerUrlError::Invalid),
        ("ws://", ServerUrlError::Invalid),
        ("ws://user@host", ServerUrlError::Invalid),
        ("ws://user:pass@host", ServerUrlError::Invalid),
        ("ws://:pass@host", ServerUrlError::Invalid),
        ("ws://host:99999", ServerUrlError::Invalid),
        ("ws://host/api", ServerUrlError::NotRoot),
        ("ws://host/?a=1", ServerUrlError::NotRoot),
        ("ws://host/#frag", ServerUrlError::NotRoot),
    ];
    for (input, error) in invalid {
        assert_eq!(ServerUrl::parse(input), Err(error), "{input:?}");
    }
    assert!(serde_json::from_value::<ServerUrl>(serde_json::json!(5)).is_err());
}

#[tokio::test]
async fn the_fake_recognizer_is_driven_by_its_control() {
    let (fake, mut control) = FakeRecognizer::new();
    let mut connecting = pin!(fake.connect());
    assert!(futures_util::poll!(&mut connecting).is_pending());
    control.ready();
    let mut session = connecting.await.expect("ready");
    control.emit(Ok(RecognizerEvent::Final("hi".into())));
    session.send_frame(&ramp(0)).await.expect("send");
    assert_eq!(
        session.next_event().await,
        Ok(RecognizerEvent::Final("hi".into()))
    );
    session.cancel().await;
    assert_eq!(
        control.calls(),
        [FakeCall::Frame(Box::new(ramp(0))), FakeCall::Cancel]
    );

    let (fake, mut control) = FakeRecognizer::new();
    control.fail("No model.");
    assert_eq!(
        fake.connect().await.err().expect("fails").to_string(),
        "No model."
    );
}

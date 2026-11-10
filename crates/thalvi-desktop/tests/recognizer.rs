//! The server recognizer speaks the `/transcription` protocol against a local mock socket.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame, SAMPLE_RATE};
use thalvi_desktop::recognizer::{
    ConnectFailure, FakeCall, FakeRecognizer, Recognizer, RecognizerError, RecognizerEvent,
    RecognizerSession, ServerRecognizer, ServerUrl, ServerUrlError, SpeechSource,
    UPSAMPLED_SAMPLES, Upsampler,
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

const OFFICE: &str = "Office";

fn office(url: &ServerUrl) -> SpeechSource {
    SpeechSource::thalvi_server(OFFICE, url)
}

async fn connect(url: ServerUrl) -> Result<impl RecognizerSession, RecognizerError> {
    tokio::time::timeout(WAIT, ServerRecognizer::new(OFFICE, url).connect())
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
    let recognizer = ServerRecognizer::new(OFFICE, url);
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

    let not_understood: Expected = |server| RecognizerError::NotUnderstood {
        speech: office(&server),
    };
    let dropped: Expected = |server| RecognizerError::Dropped {
        speech: office(&server),
    };
    let cases: [(Vec<Message>, Expected); 6] = [
        (
            vec![text(
                r#"{"type":"error","message":"The model is loading."}"#,
            )],
            |server| RecognizerError::Reported {
                speech: office(&server),
                message: "The model is loading.".to_owned(),
            },
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
        matches!(&error, RecognizerError::Unreachable { speech, reason: ConnectFailure::Refused, .. } if *speech == office(&refused)),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "Thalvi could not reach the Thalvi server \"Office\" at {refused} (connection refused). Start the Thalvi server, or fix its address under Settings → Speech."
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
    let mut session = connect(url.clone()).await.expect("ready");
    let mut next = async || {
        tokio::time::timeout(WAIT, session.next_event())
            .await
            .expect("event")
    };
    assert_eq!(next().await, Ok(RecognizerEvent::Partial("he".into())));
    assert_eq!(next().await, Ok(RecognizerEvent::Final("hello".into())));
    let office = office(&url);
    assert_eq!(
        next().await,
        Err(RecognizerError::Reported {
            speech: office.clone(),
            message: "The model went away.".to_owned()
        })
    );
    assert_eq!(
        next().await,
        Err(RecognizerError::Reported {
            speech: office,
            message: "Speech recognition failed.".to_owned()
        })
    );
}

#[tokio::test]
async fn unusable_messages_after_ready_end_the_stream_with_a_reason() {
    let not_understood: Expected = |server| RecognizerError::NotUnderstood {
        speech: office(&server),
    };
    let cases: [(Message, Expected); 8] = [
        (text(r#"{"type":"mystery"}"#), not_understood),
        (text(r#"{"type":"partial","text":7}"#), not_understood),
        (text(r#"{"type":"final"}"#), not_understood),
        (text("[]"), not_understood),
        (text("not json"), not_understood),
        (ready(), not_understood),
        (Message::Binary(vec![0, 0].into()), not_understood),
        (Message::Close(None), |server| RecognizerError::Dropped {
            speech: office(&server),
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
                Err(RecognizerError::Dropped {
                    speech: office(&url),
                })
            },
            "{size} bytes"
        );
    }
}

#[tokio::test]
async fn frames_reach_the_server_upsampled_to_24_khz() {
    let (url, mut rx) = recording().await;
    let mut session = connect(url).await.expect("ready");
    let mut next = tone(1_000.0, 8_000.0);
    let frames = [next(), next()];
    let mut upsampler = Upsampler::default();
    for frame in &frames {
        session.send_frame(frame).await.expect("send");
        let Some(Seen::Binary(bytes)) = tokio::time::timeout(WAIT, rx.recv()).await.expect("audio")
        else {
            panic!("the server only receives audio");
        };
        assert_eq!(samples(&bytes), upsampler.push(frame));
    }
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "sample indices are tiny and the tone stays within i16"
)]
fn tone(hz: f64, amplitude: f64) -> impl FnMut() -> Frame {
    let mut n = 0_usize;
    move || {
        std::array::from_fn(|_| {
            n += 1;
            let phase = std::f64::consts::TAU * hz * n as f64 / f64::from(SAMPLE_RATE);
            (amplitude * phase.sin()).round() as i16
        })
    }
}

/// The amplitude of `hz` in 24 kHz `signal`, which must hold a whole number of its periods.
#[expect(clippy::cast_precision_loss, reason = "sample indices are tiny")]
fn amplitude(signal: &[i16], hz: f64) -> f64 {
    let (re, im) = signal
        .iter()
        .enumerate()
        .fold((0.0, 0.0), |(re, im), (n, &x)| {
            let phase = std::f64::consts::TAU * hz * n as f64 / 24_000.0;
            (
                re + f64::from(x) * phase.cos(),
                im - f64::from(x) * phase.sin(),
            )
        });
    2.0 * re.hypot(im) / signal.len() as f64
}

#[test]
fn upsampling_keeps_speech_up_to_7_khz_and_drops_its_images() {
    // Images of f sit at 16 kHz ± f before the final halving, which folds them to these.
    for (hz, images) in [
        (1_000.0, [7_000.0, 9_000.0]),
        (3_000.0, [5_000.0, 11_000.0]),
        (7_000.0, [1_000.0, 9_000.0]),
    ] {
        let mut next = tone(hz, 16_384.0);
        let mut upsampler = Upsampler::default();
        upsampler.push(&next());
        // Four frames are 0.32 s, a whole number of periods of every frequency here.
        let out: Vec<i16> = (0..4).flat_map(|_| upsampler.push(&next())).collect();
        let db = |level: f64| 20.0 * (level / 16_384.0).log10();
        assert!(
            db(amplitude(&out, hz)).abs() < 0.1,
            "{hz} Hz: {:.2} dB",
            db(amplitude(&out, hz))
        );
        for image in images {
            assert!(
                db(amplitude(&out, image)) < -60.0,
                "{hz} Hz image at {image} Hz: {:.1} dB",
                db(amplitude(&out, image))
            );
        }
    }
}

#[test]
fn upsampling_clips_the_overshoot_of_loud_input() {
    let mut upsampler = Upsampler::default();
    upsampler.push(&[i16::MIN; FRAME_SAMPLES]);
    let out = upsampler.push(&[i16::MAX; FRAME_SAMPLES]);
    // The step reaches the output after the filter's 1.5 ms; its ringing must not wrap to negative.
    assert!(out[60..].iter().all(|&sample| sample > 0));
    assert_eq!(out.iter().max(), Some(&i16::MAX));
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

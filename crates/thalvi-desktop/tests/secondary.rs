//! The secondary transcription uploads a 24 kHz WAV in the shape each API expects.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]
#![expect(clippy::unwrap_used, reason = "a test fails by panicking")]

use std::time::Duration;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::{body::Body, response::Response};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::openai_live::OpenAiRecognizer;
use thalvi_desktop::recognizer::Upsampler;
use thalvi_desktop::recognizer::{Recognizer, RecognizerEvent, RecognizerSession};
use thalvi_desktop::secondary::{
    SecondaryApi, SecondaryError, SecondaryStt, SecondarySttError, transcribe,
};
use tokio::sync::mpsc;

struct Captured {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// Answers every request with `status` and `reply`, reporting what it received.
async fn serve(status: u16, reply: &'static str) -> (String, mpsc::UnboundedReceiver<Captured>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Router::new().fallback(move |request: Request| {
        let tx = tx.clone();
        async move {
            let path = request.uri().path().to_string();
            let headers = request.headers().clone();
            let body = to_bytes(request.into_body(), usize::MAX).await.unwrap();
            let _ = tx.send(Captured {
                path,
                headers,
                body: body.to_vec(),
            });
            (StatusCode::from_u16(status).unwrap(), reply)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}/v1/", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, rx)
}

async fn serve_sse(reply: &'static str) -> (String, mpsc::UnboundedReceiver<Captured>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Router::new().fallback(move |request: Request| {
        let tx = tx.clone();
        async move {
            let path = request.uri().path().to_string();
            let headers = request.headers().clone();
            let body = to_bytes(request.into_body(), usize::MAX).await.unwrap();
            let _ = tx.send(Captured {
                path,
                headers,
                body: body.to_vec(),
            });
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(reply))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}/v1", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, rx)
}

async fn serve_sequence(
    responses: Vec<(StatusCode, &'static str)>,
) -> (String, mpsc::UnboundedReceiver<Captured>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let app = Router::new().fallback(move |request: Request| {
        let tx = tx.clone();
        let responses = Arc::clone(&responses);
        async move {
            let path = request.uri().path().to_string();
            let headers = request.headers().clone();
            let body = to_bytes(request.into_body(), usize::MAX).await.unwrap();
            let _ = tx.send(Captured {
                path,
                headers,
                body: body.to_vec(),
            });
            let (status, body) = responses.lock().unwrap().pop_front().unwrap();
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}/v1", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, rx)
}

fn settings(url: &str, api: SecondaryApi, key: &str) -> SecondaryStt {
    SecondaryStt::new(url, "the-model", key.to_string(), api, true).expect("valid settings")
}

#[tokio::test]
async fn openai_streaming_emits_cumulative_text_and_plain_json_still_works() {
    let (url, mut rx) = serve_sse(
        "data: {\"type\":\"transcript.text.delta\",\"delta\":\"hello \"}\n\ndata: {\"type\":\"transcript.text.delta\",\"delta\":\"world\"}\n\ndata: {\"type\":\"transcript.text.done\",\"text\":\"hello world\"}\n\n",
    ).await;
    let metrics = Recorder::disabled().metrics();
    let client = reqwest::Client::builder()
        .user_agent("thalvi-second-pass-client")
        .build()
        .expect("configured shared client");
    let recognizer = OpenAiRecognizer::new(
        "local",
        settings(&url, SecondaryApi::OpenAi, ""),
        client,
        metrics,
    );
    let mut session = recognizer
        .connect()
        .await
        .expect("connected without uploading");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    assert!(
        rx.try_recv().is_err(),
        "upload starts only when capture ends"
    );
    session.finish_audio().await.expect("capture ended");
    let mut events = Vec::new();
    loop {
        let event = session.next_event().await.expect("transcription event");
        let final_event = matches!(event, RecognizerEvent::Final(_));
        events.push(event);
        if final_event {
            break;
        }
    }
    assert_eq!(
        events,
        [
            RecognizerEvent::Partial("hello ".to_owned()),
            RecognizerEvent::Partial("hello world".to_owned()),
            RecognizerEvent::Final("hello world".to_owned())
        ]
    );
    let captured = rx.recv().await.expect("one upload");
    assert_eq!(captured.path, "/v1/audio/transcriptions");
    assert_eq!(captured.headers["user-agent"], "thalvi-second-pass-client");
    assert!(
        captured
            .body
            .windows(b"name=\"stream\"".len())
            .any(|part| part == b"name=\"stream\"")
    );
    assert!(rx.try_recv().is_err(), "one upload for one capture");

    let (url, mut rx) = serve(200, r#"{"text":"plain answer"}"#).await;
    let recognizer = OpenAiRecognizer::new(
        "local",
        settings(&url, SecondaryApi::OpenAi, ""),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
    );
    let mut session = recognizer.connect().await.expect("connected");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    session.finish_audio().await.expect("capture ended");
    let events = [session.next_event().await.expect("final transcription")];
    assert_eq!(events, [RecognizerEvent::Final("plain answer".to_owned())]);
    assert_eq!(
        rx.recv().await.expect("one upload").path,
        "/v1/audio/transcriptions"
    );
    assert!(rx.try_recv().is_err(), "one upload for one capture");
}

#[tokio::test]
async fn a_failed_capture_reports_plainly_and_the_next_capture_can_succeed() {
    let (url, mut rx) = serve_sequence(vec![
        (StatusCode::INTERNAL_SERVER_ERROR, "{}"),
        (StatusCode::OK, r#"{"text":"recovered"}"#),
    ])
    .await;
    let recognizer = OpenAiRecognizer::new(
        "local",
        settings(&url, SecondaryApi::OpenAi, ""),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
    );
    let mut first = recognizer.connect().await.expect("first session");
    first.send_frame(&ramp(0)).await.expect("buffer");
    first.finish_audio().await.expect("release");
    let failure = first
        .next_event()
        .await
        .expect_err("server failure is shown");
    assert!(failure.to_string().contains("HTTP 500"));

    let mut next = recognizer.connect().await.expect("next capture session");
    next.send_frame(&ramp(1)).await.expect("buffer");
    next.finish_audio().await.expect("release");
    assert_eq!(
        next.next_event().await.expect("recovered transcription"),
        RecognizerEvent::Final("recovered".to_owned())
    );
    assert_eq!(
        rx.recv().await.expect("first upload").path,
        "/v1/audio/transcriptions"
    );
    assert_eq!(
        rx.recv().await.expect("next upload").path,
        "/v1/audio/transcriptions"
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn an_unreachable_or_slow_live_endpoint_says_what_failed() {
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", unused.local_addr().expect("address"));
    drop(unused);
    let recognizer = OpenAiRecognizer::new(
        "local",
        settings(&url, SecondaryApi::OpenAi, ""),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
    );
    let mut session = recognizer.connect().await.expect("connected");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    session.finish_audio().await.expect("capture ended");
    let failure = session.next_event().await.expect_err("endpoint is down");
    assert!(
        failure
            .to_string()
            .contains("could not reach the speech server")
    );

    let app = Router::new().fallback(std::future::pending::<()>);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let recognizer = OpenAiRecognizer::new(
        "local",
        settings(&url, SecondaryApi::OpenAi, ""),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
    );
    let mut session = recognizer.connect().await.expect("connected");
    session.send_frame(&ramp(1)).await.expect("buffered frame");
    session.finish_audio().await.expect("capture ended");
    let failure = session.next_event().await.expect_err("request deadline");
    assert!(failure.to_string().contains("took too long to transcribe"));
}

fn ramp(index: usize) -> Frame {
    std::array::from_fn(|i| i16::try_from(3 * (index * FRAME_SAMPLES + i)).expect("small ramp"))
}

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("needle present")
}

/// Splits a multipart body into (part headers, part content).
fn parts(captured: &Captured) -> Vec<(String, Vec<u8>)> {
    let content_type = captured.headers["content-type"].to_str().unwrap();
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .expect("multipart content type");
    let delimiter = format!("--{boundary}").into_bytes();
    let mut rest = &captured.body[..];
    let mut out = Vec::new();
    rest = &rest[find(rest, &delimiter) + delimiter.len()..];
    while !rest.starts_with(b"--") {
        let end = find(rest, &delimiter);
        let part = &rest[2..end - 2];
        let split = find(part, b"\r\n\r\n");
        out.push((
            String::from_utf8(part[..split].to_vec()).unwrap(),
            part[split + 4..].to_vec(),
        ));
        rest = &rest[end + delimiter.len()..];
    }
    out
}

#[tokio::test]
async fn each_api_gets_its_request_and_a_24_khz_wav_of_the_whole_capture() {
    let cases = [
        (
            SecondaryApi::OpenAi,
            "/v1/audio/transcriptions",
            r#"{"text":"polished words"}"#,
            "Content-Disposition: form-data; name=\"model\"",
            b"the-model".to_vec(),
            "name=\"file\"",
        ),
        (
            SecondaryApi::Meta,
            "/v1/asr/transcribe",
            r#"{"transcript":"polished words"}"#,
            "Content-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json",
            br#"{"mode":"PUSH_TO_TALK","model":"the-model","audioEncoding":"WAV"}"#.to_vec(),
            "name=\"audio\"",
        ),
    ];
    for (api, path, reply, first_header, first_body, audio_field) in cases {
        let (url, mut rx) = serve(200, reply).await;
        let text = transcribe(
            &reqwest::Client::new(),
            &settings(&url, api, ""),
            &[ramp(0), ramp(1)],
        )
        .await;
        assert_eq!(text.as_deref(), Ok("polished words"), "{api:?}");

        let captured = rx.recv().await.expect("a request");
        assert_eq!(captured.path, path);
        assert_eq!(captured.headers["accept"], "application/json");
        let parts = parts(&captured);
        assert_eq!(parts.len(), 2, "{api:?}");
        assert_eq!(parts[0].0, first_header);
        if api == SecondaryApi::Meta {
            let sent: serde_json::Value = serde_json::from_slice(&parts[0].1).unwrap();
            assert_eq!(
                sent,
                serde_json::from_slice::<serde_json::Value>(&first_body).unwrap()
            );
        } else {
            assert_eq!(parts[0].1, first_body);
        }
        assert_eq!(
            parts[1].0,
            format!(
                "Content-Disposition: form-data; {audio_field}; filename=\"capture.wav\"\r\nContent-Type: audio/wav"
            )
        );

        let wav = &parts[1].1;
        let field = |from: usize, length: usize| {
            wav[from..from + length]
                .iter()
                .rev()
                .fold(0_usize, |value, byte| value << 8 | usize::from(*byte))
        };
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(field(4, 4), wav.len() - 8);
        assert_eq!(field(20, 2), 1, "PCM");
        assert_eq!(field(22, 2), 1, "mono");
        assert_eq!(field(24, 4), 24_000, "sample rate");
        assert_eq!(field(28, 4), 48_000, "byte rate");
        assert_eq!(field(32, 2), 2, "block align");
        assert_eq!(field(34, 2), 16, "bits");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(field(40, 4), 2 * 3_840, "two 80 ms frames at 24 kHz");
        let heard: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect();
        let mut upsampler = Upsampler::default();
        let live: Vec<i16> = [ramp(0), ramp(1)]
            .iter()
            .flat_map(|frame| upsampler.push(frame))
            .collect();
        assert_eq!(heard, live, "the live stream's 24 kHz audio");
    }
}

#[tokio::test]
async fn the_bearer_header_is_sent_only_with_a_key() {
    for api in [SecondaryApi::OpenAi, SecondaryApi::Meta] {
        for (key, expected) in [("secret", Some("Bearer secret")), ("", None)] {
            let (url, mut rx) = serve(200, r#"{"text":"t","transcript":"t"}"#).await;
            transcribe(
                &reqwest::Client::new(),
                &settings(&url, api, key),
                &[ramp(0)],
            )
            .await
            .expect("transcribed");
            let captured = rx.recv().await.expect("a request");
            assert_eq!(
                captured
                    .headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap()),
                expected,
                "{api:?} with key {key:?}"
            );
        }
    }
}

#[tokio::test]
async fn failures_are_typed() {
    let cases = [
        (SecondaryApi::OpenAi, 500, "{}", SecondaryError::Status(500)),
        (
            SecondaryApi::OpenAi,
            200,
            "not json",
            SecondaryError::BadResponse,
        ),
        (SecondaryApi::OpenAi, 200, "{}", SecondaryError::BadResponse),
        (
            SecondaryApi::OpenAi,
            200,
            r#"{"text":7}"#,
            SecondaryError::BadResponse,
        ),
        (
            SecondaryApi::Meta,
            200,
            r#"{"text":"wrong field for meta"}"#,
            SecondaryError::BadResponse,
        ),
    ];
    for (api, status, reply, expected) in cases {
        let (url, _rx) = serve(status, reply).await;
        let result = transcribe(
            &reqwest::Client::new(),
            &settings(&url, api, ""),
            &[ramp(0)],
        )
        .await;
        assert_eq!(result, Err(expected), "{api:?} {status} {reply}");
    }
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", unused.local_addr().expect("address"));
    drop(unused);
    assert_eq!(
        transcribe(
            &reqwest::Client::new(),
            &settings(&url, SecondaryApi::OpenAi, ""),
            &[ramp(0)]
        )
        .await,
        Err(SecondaryError::Unreachable)
    );
}

#[tokio::test(start_paused = true)]
async fn a_silent_endpoint_times_out_after_ten_seconds() {
    let app = Router::new().fallback(std::future::pending::<()>);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        transcribe(
            &reqwest::Client::new(),
            &settings(&url, SecondaryApi::OpenAi, ""),
            &[ramp(0)],
        ),
    )
    .await
    .expect("the endpoint is given up on");
    assert_eq!(result, Err(SecondaryError::Timeout));
    assert_eq!(started.elapsed(), Duration::from_secs(10));
}

#[test]
fn settings_are_validated_once_where_they_are_read() {
    let parse = |value: serde_json::Value| serde_json::from_value::<SecondaryStt>(value);

    let minimal = parse(
        serde_json::json!({"enabled": true, "url": " https://api.example.com/v1/ ", "model": " m "}),
    )
    .expect("minimal settings");
    assert!(minimal.enabled());
    assert_eq!(
        minimal,
        SecondaryStt::new(
            "https://api.example.com/v1",
            "m",
            String::new(),
            SecondaryApi::OpenAi,
            true
        )
        .unwrap()
    );
    assert_eq!(
        parse(serde_json::json!({"enabled": true, "url": "https://api.example.com/v1", "model": "m", "api": "openai"}))
        .expect("the openai api by name"),
        minimal
    );
    let meta = parse(serde_json::json!({
        "enabled": true, "url": "http://localhost:9000", "model": "m", "api_key": "hunter2", "api": "meta"
    }))
    .expect("full settings");
    assert_eq!(
        meta,
        SecondaryStt::new(
            "http://localhost:9000",
            "m",
            "hunter2".into(),
            SecondaryApi::Meta,
            true
        )
        .unwrap()
    );
    assert!(
        !format!("{meta:?}").contains("hunter2"),
        "the key stays out of logs"
    );

    let url_error = SecondarySttError::Url.to_string();
    let model_error = SecondarySttError::Model.to_string();
    let invalid = [
        (
            serde_json::json!({"enabled": true, "url": "ftp://h", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "ws://h", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://user:pw@h", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://user@h", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://:pw@h", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://h:99999", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://h/v1?x=1", "model": "m"}),
            url_error.clone(),
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://h/v1#x", "model": "m"}),
            url_error,
        ),
        (
            serde_json::json!({"enabled": true, "url": "http://h", "model": "  "}),
            model_error,
        ),
    ];
    for (value, message) in invalid {
        let error = parse(value.clone()).expect_err("settings are refused");
        assert!(error.to_string().contains(&message), "{value}: {error}");
    }
    for value in [
        serde_json::json!({"model": "m"}),
        serde_json::json!({"url": "http://h"}),
        serde_json::json!({"url": "http://h", "model": "m", "api": "azure"}),
        serde_json::json!({"url": "http://h", "model": "m", "api_key": 7}),
        serde_json::json!({"url": "http://h", "model": "m", "extra": true}),
    ] {
        assert!(parse(value.clone()).is_err(), "{value}");
    }
}

#[test]
fn the_switch_defaults_off_and_off_keeps_the_endpoint() {
    let parse = |value: serde_json::Value| serde_json::from_value::<SecondaryStt>(value);
    let legacy = parse(serde_json::json!({"url": "https://api.example.com/v1", "model": "m"}))
        .expect("legacy settings stay off");
    assert!(!legacy.enabled(), "present without a switch stays off");
    let off = parse(serde_json::json!({
        "enabled": false, "url": "http://localhost:9000", "model": "m", "api_key": "hunter2", "api": "meta"
    }))
    .expect("off settings keep the endpoint");
    assert!(!off.enabled());
    assert_eq!(
        (off.address(), off.model(), off.api_key(), off.api()),
        ("http://localhost:9000", "m", "hunter2", SecondaryApi::Meta)
    );
}

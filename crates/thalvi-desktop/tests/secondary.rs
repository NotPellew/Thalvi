//! The secondary transcription uploads a 24 kHz WAV in the shape each API expects.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]
#![expect(clippy::unwrap_used, reason = "a test fails by panicking")]

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::{body::Body, response::Response};
use base64::{Engine, engine::general_purpose::STANDARD};
use thalvi_desktop::chosen::Chosen;
use thalvi_desktop::device::Adapters;
use thalvi_desktop::entries::{LiveEntry, LiveKind};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::metrics::{RecordKinds, Recorder};
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::openai_live::OpenAiRecognizer;
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::Upsampler;
use thalvi_desktop::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerSession};
use thalvi_desktop::report::UserError;
use thalvi_desktop::secondary::{
    Language, LiveRequest, OpenAiLive, SecondaryApi, SecondaryError, SecondaryStt,
    SecondarySttError, transcribe,
};
use tokio::sync::mpsc;

/// The next request the fake received, or none when nothing arrives in time.
async fn received(rx: &mut mpsc::UnboundedReceiver<Captured>) -> Option<Captured> {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .ok()
        .flatten()
}

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

fn live(url: &str, key: &str) -> OpenAiLive {
    OpenAiLive {
        settings: settings(url, SecondaryApi::OpenAi, key),
        request: LiveRequest::Transcriptions,
        language: None,
    }
}

fn chat(url: &str, key: &str, language: &str) -> OpenAiLive {
    OpenAiLive {
        request: LiveRequest::Chat,
        language: Language::parse(language).unwrap(),
        ..live(url, key)
    }
}

/// One buffered capture of `ramp(0)` through `recognizer`, and what it delivered.
async fn dictate(recognizer: &OpenAiRecognizer) -> Result<String, RecognizerError> {
    let mut session = recognizer.connect().await.expect("connected");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    session.finish_audio().await.expect("capture ended");
    loop {
        if let RecognizerEvent::Final(text) = session.next_event().await? {
            return Ok(text);
        }
    }
}

fn recognizer(live: OpenAiLive) -> OpenAiRecognizer {
    OpenAiRecognizer::new(
        "local",
        live,
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
        &WordList::default(),
    )
}

fn active(live: OpenAiLive) -> LiveEntry {
    LiveEntry {
        id: "live".to_owned(),
        name: "Local speech".to_owned(),
        kind: LiveKind::OpenAi(live),
        extra: false,
    }
}

fn chosen(live: OpenAiLive, words: &WordList) -> Chosen {
    Chosen::new(
        &active(live),
        ModelStore::in_dir("unused"),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
        words,
        Adapters::default(),
        std::path::Path::new("unused"),
    )
}

/// One buffered capture through the entry's chosen recognizer, and what it delivered.
async fn dictate_chosen(chosen: &Chosen) -> Result<String, RecognizerError> {
    let mut session = chosen.connect().await.expect("connected");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    session.finish_audio().await.expect("capture ended");
    loop {
        if let RecognizerEvent::Final(text) = session.next_event().await? {
            return Ok(text);
        }
    }
}

/// The content of the multipart part `name`, if the request has one.
fn sent_part(captured: &Captured, name: &str) -> Option<String> {
    parts(captured)
        .into_iter()
        .find(|(headers, _)| headers.contains(&format!("name=\"{name}\"")))
        .map(|(_, value)| String::from_utf8(value).expect("part text"))
}

#[tokio::test]
async fn an_active_after_you_stop_entry_sends_the_word_list_as_prompt() {
    let two = WordList::new(vec!["Cybrex2".to_owned(), "Thalvi".to_owned()]).expect("list");
    let empty = WordList::default();
    // (case, SSE, the list, the prompt the request should carry)
    let cases = [
        ("a plain reply", false, &two, Some("Cybrex2, Thalvi")),
        ("a streamed reply", true, &two, Some("Cybrex2, Thalvi")),
        ("a plain reply", false, &empty, None),
        ("a streamed reply", true, &empty, None),
    ];
    for (case, sse, words, expected) in cases {
        let (url, mut rx) = if sse {
            serve_sse("data: {\"type\":\"transcript.text.done\",\"text\":\"ok\"}\n\n").await
        } else {
            serve(200, r#"{"text":"ok"}"#).await
        };
        assert_eq!(
            dictate_chosen(&chosen(live(&url, ""), words))
                .await
                .expect("transcription"),
            "ok",
            "{case}"
        );
        let captured = received(&mut rx).await.expect("one upload");
        assert_eq!(
            sent_part(&captured, "prompt").as_deref(),
            expected,
            "{case}"
        );
    }
}

#[tokio::test]
async fn an_active_after_you_stop_chat_entry_sends_the_word_list_as_a_system_message() {
    let (url, mut rx) = serve(
        200,
        r#"{"choices":[{"message":{"role":"assistant","content":"hallo welt"}}]}"#,
    )
    .await;
    let words = WordList::new(vec!["Cybrex2".to_owned(), "Thalvi".to_owned()]).expect("list");
    assert_eq!(
        dictate_chosen(&chosen(chat(&url, "", ""), &words))
            .await
            .expect("transcription"),
        "hallo welt"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&received(&mut rx).await.expect("one request").body)
            .expect("JSON request");
    assert_eq!(
        body["messages"][0],
        serde_json::json!({"role": "system", "content": "Cybrex2, Thalvi"})
    );
    assert_eq!(body["messages"].as_array().expect("messages").len(), 2);
}

#[tokio::test]
async fn speaches_streams_transcribed_segments_as_cumulative_text() {
    let (url, mut rx) = serve_sse(
        "data: {\"text\":\"We are checking the endpoint.\"}\n\ndata: {\"text\":\"It returns the next sentence.\"}\n\ndata: {\"text\":\"This is the end.\"}\n\n",
    )
    .await;
    let recognizer = OpenAiRecognizer::new(
        "local",
        live(&url, ""),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
        &WordList::default(),
    );
    let mut session = recognizer.connect().await.expect("connected");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    session.finish_audio().await.expect("capture ended");
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            let event = session.next_event().await.expect("transcription event");
            let final_event = matches!(event, RecognizerEvent::Final(_));
            events.push(event);
            if final_event {
                return events;
            }
        }
    })
    .await
    .expect("all speech segments arrive");
    assert_eq!(
        events,
        [
            RecognizerEvent::Partial("We are checking the endpoint.".to_owned()),
            RecognizerEvent::Partial(
                "We are checking the endpoint. It returns the next sentence.".to_owned()
            ),
            RecognizerEvent::Partial(
                "We are checking the endpoint. It returns the next sentence. This is the end."
                    .to_owned()
            ),
            RecognizerEvent::Final(
                "We are checking the endpoint. It returns the next sentence. This is the end."
                    .to_owned()
            ),
        ]
    );
    assert_eq!(
        received(&mut rx).await.expect("one upload").path,
        "/v1/audio/transcriptions"
    );
    assert!(rx.try_recv().is_err(), "one request per capture");
}

#[tokio::test]
async fn openai_streaming_emits_cumulative_text() {
    let (url, mut rx) = serve_sse(
        "data: {\"type\":\"transcript.text.delta\",\"delta\":\"hello \"}\n\ndata: {\"type\":\"transcript.text.delta\",\"delta\":\"world\"}\n\ndata: {\"type\":\"transcript.text.done\",\"text\":\"cleaned transcript\"}\n\n",
    ).await;
    let metrics_dir = std::env::temp_dir().join(format!(
        "thalvi-live-stt-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    let recorder = Recorder::in_dir(
        RecordKinds {
            metrics: true,
            ..RecordKinds::default()
        },
        &metrics_dir,
    )
    .expect("metrics recorder");
    let client = reqwest::Client::builder()
        .user_agent("thalvi-second-pass-client")
        .build()
        .expect("configured shared client");
    let recognizer = OpenAiRecognizer::new(
        "local",
        live(&url, "stream-key"),
        client,
        recorder.metrics(),
        &WordList::default(),
    );
    let mut session = recognizer
        .connect()
        .await
        .expect("connected without uploading");
    assert!(session.buffers_audio(), "audio waits until capture ends");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    assert!(
        rx.try_recv().is_err(),
        "upload starts only when capture ends"
    );
    session.finish_audio().await.expect("capture ended");
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            let event = session.next_event().await.expect("transcription event");
            let final_event = matches!(event, RecognizerEvent::Final(_));
            events.push(event);
            if final_event {
                break events;
            }
        }
    })
    .await
    .expect("transcription completes after release");
    assert_eq!(
        events,
        [
            RecognizerEvent::Partial("hello ".to_owned()),
            RecognizerEvent::Partial("hello world".to_owned()),
            RecognizerEvent::Final("cleaned transcript".to_owned())
        ]
    );
    let captured = received(&mut rx).await.expect("one upload");
    assert_eq!(captured.path, "/v1/audio/transcriptions");
    assert_eq!(captured.headers["user-agent"], "thalvi-second-pass-client");
    assert_eq!(captured.headers["authorization"], "Bearer stream-key");
    assert!(
        captured
            .body
            .windows(b"name=\"stream\"".len())
            .any(|part| part == b"name=\"stream\"")
    );
    assert!(rx.try_recv().is_err(), "one upload for one capture");
    recorder.finish().await;
    let metrics = std::fs::read_to_string(metrics_dir.join("metrics.jsonl"))
        .expect("live transcription metrics");
    let metrics: Vec<serde_json::Value> = metrics
        .lines()
        .map(|line| serde_json::from_str(line).expect("metric line"))
        .collect();
    assert_eq!(
        metrics
            .iter()
            .filter(|event| event["event"] == "desktop_live_stt_first_text")
            .count(),
        1,
        "first-text latency is recorded once"
    );
    assert_eq!(
        metrics
            .iter()
            .filter(|event| event["event"] == "desktop_live_stt_delta")
            .count(),
        2,
        "every streamed partial is recorded"
    );
    std::fs::remove_dir_all(metrics_dir).expect("remove test metrics");
}

#[tokio::test]
async fn openai_streaming_accepts_a_clean_empty_stream() {
    let (url, _rx) = serve_sse("").await;
    let recognizer = OpenAiRecognizer::new(
        "local",
        live(&url, ""),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
        &WordList::default(),
    );
    let mut session = recognizer.connect().await.expect("connected");
    session.send_frame(&ramp(0)).await.expect("buffered frame");
    session.finish_audio().await.expect("capture ended");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), session.next_event())
            .await
            .expect("clean stream completes")
            .expect("empty stream is a transcript"),
        RecognizerEvent::Final(String::new())
    );
}

#[tokio::test]
async fn openai_transcriptions_report_the_detected_language_by_code_or_name() {
    let cases = [
        (
            r#"{"text":"plain answer","language":"de"}"#,
            false,
            Some("de"),
        ),
        (
            r#"{"text":"plain answer","language":"German"}"#,
            false,
            Some("de"),
        ),
        (r#"{"text":"plain answer"}"#, false, None),
        (
            "data: {\"type\":\"transcript.text.done\",\"text\":\"hello\",\"language\":\"fr\"}\n\n",
            true,
            Some("fr"),
        ),
        (
            "data: {\"type\":\"transcript.text.done\",\"text\":\"hello\",\"language\":\"spanish\"}\n\n",
            true,
            Some("es"),
        ),
    ];
    for (reply, sse, expected) in cases {
        let (url, _rx) = if sse {
            serve_sse(reply).await
        } else {
            serve(200, reply).await
        };
        let recognizer = OpenAiRecognizer::new(
            "local",
            live(&url, ""),
            reqwest::Client::new(),
            Recorder::disabled().metrics(),
            &WordList::default(),
        );
        let mut session = recognizer.connect().await.expect("connected");
        session.send_frame(&ramp(0)).await.expect("buffered frame");
        session.finish_audio().await.expect("capture ended");
        loop {
            if let RecognizerEvent::Final(_) =
                tokio::time::timeout(Duration::from_secs(10), session.next_event())
                    .await
                    .expect("transcription completes")
                    .expect("final transcription")
            {
                break;
            }
        }
        assert_eq!(
            session.language().map(Language::code),
            expected,
            "{reply:?}"
        );
    }
}

#[tokio::test]
async fn chosen_live_openai_entry_buffers_and_forwards_the_whole_capture() {
    let (url, mut rx) = serve_sse(
        "data: {\"type\":\"transcript.text.delta\",\"delta\":\"hello\"}\n\ndata: {\"type\":\"transcript.text.done\",\"text\":\"hello\"}\n\n",
    )
    .await;
    let entry = LiveEntry {
        id: "live".to_owned(),
        name: "Local speech".to_owned(),
        kind: LiveKind::OpenAi(live(&url, "")),
        extra: false,
    };
    let chosen = Chosen::new(
        &entry,
        ModelStore::in_dir("unused"),
        reqwest::Client::new(),
        Recorder::disabled().metrics(),
        &WordList::default(),
        Adapters::default(),
        std::path::Path::new("unused"),
    );
    let mut session = chosen.connect().await.expect("connects before upload");
    assert!(session.buffers_audio(), "audio waits until capture ends");
    session.send_frame(&ramp(0)).await.expect("buffer frame");
    assert!(rx.try_recv().is_err(), "the request waits for release");
    session.finish_audio().await.expect("capture ended");
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            let event = session.next_event().await?;
            let final_event = matches!(event, RecognizerEvent::Final(_));
            events.push(event);
            if final_event {
                return Ok::<_, RecognizerError>(events);
            }
        }
    })
    .await
    .expect("the upload starts after release")
    .expect("transcription succeeds");
    assert_eq!(
        result,
        [
            RecognizerEvent::Partial("hello".to_owned()),
            RecognizerEvent::Final("hello".to_owned())
        ]
    );

    let captured = received(&mut rx).await.expect("one upload");
    assert_eq!(captured.path, "/v1/audio/transcriptions");
    let uploaded = parts(&captured)
        .into_iter()
        .find(|(headers, _)| headers.contains("name=\"file\""))
        .expect("WAV part")
        .1;
    assert!(
        uploaded[44..].iter().any(|byte| *byte != 0),
        "the frame is sent"
    );
    assert!(rx.try_recv().is_err(), "one request per capture");
}

#[tokio::test]
async fn a_failed_capture_reports_plainly_and_the_next_capture_can_succeed() {
    const BAD_REPLY: &str = "unreadable transcription";
    let server_error = (
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        "Speech server error — Settings",
        "HTTP 500",
    );
    let bad = |body| {
        (
            StatusCode::OK,
            body,
            "Bad speech server reply — Settings",
            BAD_REPLY,
        )
    };
    let kinds = [
        (
            "transcriptions",
            (|url| live(url, "")) as fn(&str) -> OpenAiLive,
            "/v1/audio/transcriptions",
            vec![server_error, bad("not json")],
            r#"{"text":"recovered"}"#,
            "recovered",
        ),
        (
            "chat",
            |url| chat(url, "", ""),
            "/v1/chat/completions",
            vec![
                server_error,
                bad("not json"),
                bad(r#"{"choices":[]}"#),
                bad(r#"{"choices":[{"message":{}}]}"#),
                bad(r#"{"choices":[{"message":{"content":null}}]}"#),
                bad(r#"{"choices":[{"message":{"content":[{"type":"text"}]}}]}"#),
            ],
            r#"{"choices":[{"message":{"content":"recovered"}}]}"#,
            "recovered",
        ),
    ];
    for (kind, entry, path, failures, recovery, recovered) in kinds {
        let mut replies: Vec<_> = failures
            .iter()
            .map(|(status, body, _, _)| (*status, *body))
            .collect();
        replies.push((StatusCode::OK, recovery));
        let (url, mut rx) = serve_sequence(replies).await;
        let recognizer = recognizer(entry(&url));
        for (_, body, headline, message) in &failures {
            let failure = dictate(&recognizer).await.expect_err(body);
            assert_eq!(failure.headline(), *headline, "{kind} {body}");
            assert!(
                failure.to_string().contains(message),
                "{kind} {body}: {failure}"
            );
            assert!(
                failure.to_string().contains("Nothing was delivered"),
                "{kind} {body}: {failure}"
            );
        }
        assert_eq!(
            dictate(&recognizer).await.expect("recovered"),
            recovered,
            "{kind}"
        );
        for _ in 0..=failures.len() {
            assert_eq!(received(&mut rx).await.expect("an upload").path, path);
        }
        assert!(rx.try_recv().is_err(), "{kind}: one upload per capture");
    }
}

#[tokio::test(start_paused = true)]
async fn an_unreachable_or_silent_endpoint_says_what_failed() {
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let unreachable = format!("http://{}", unused.local_addr().expect("address"));
    drop(unused);
    let app = Router::new().fallback(std::future::pending::<()>);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let silent = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    for (kind, entry) in [
        (
            "transcriptions",
            (|url| live(url, "")) as fn(&str) -> OpenAiLive,
        ),
        ("chat", |url| chat(url, "", "")),
    ] {
        for (case, url, headline, message) in [
            (
                "unreachable",
                &unreachable,
                "Can't reach speech server — Settings",
                "could not be reached",
            ),
            (
                "silent",
                &silent,
                "Speech server timed out — Settings",
                "took too long to transcribe",
            ),
        ] {
            let failure =
                tokio::time::timeout(Duration::from_secs(15), dictate(&recognizer(entry(url))))
                    .await
                    .expect("the failure comes back within its bounded deadline")
                    .expect_err("the endpoint does not answer");
            assert_eq!(failure.headline(), headline, "{kind} {case}");
            assert!(
                failure.to_string().contains(message),
                "{kind} {case}: {failure}"
            );
        }
    }
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
        let transcript = transcribe(
            &reqwest::Client::new(),
            &settings(&url, api, ""),
            &[ramp(0), ramp(1)],
            &WordList::default(),
        )
        .await;
        assert_eq!(
            transcript
                .as_ref()
                .map(|transcript| transcript.text.as_str()),
            Ok("polished words"),
            "{api:?}"
        );

        let captured = received(&mut rx).await.expect("a request");
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
                &WordList::default(),
            )
            .await
            .expect("transcribed");
            let captured = received(&mut rx).await.expect("a request");
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

#[tokio::test(start_paused = true)]
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
            &WordList::default(),
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
            &[ramp(0)],
            &WordList::default(),
        )
        .await,
        Err(SecondaryError::Unreachable)
    );

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
            &WordList::default(),
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

#[tokio::test]
async fn chat_entry_sends_the_wav_as_input_audio_and_delivers_the_reply() {
    let (url, mut rx) = serve(
        200,
        r#"{"choices":[{"message":{"role":"assistant","content":"hallo welt"}}]}"#,
    )
    .await;
    let (plain, mut plain_rx) = serve(200, r#"{"text":"x"}"#).await;
    dictate(&recognizer(live(&plain, "")))
        .await
        .expect("transcriptions");
    let sent_wav = parts(&received(&mut plain_rx).await.expect("upload"))
        .into_iter()
        .find(|(headers, _)| headers.contains("name=\"file\""))
        .expect("WAV part")
        .1;

    let cases = [("", ""), ("DE", "k"), (" fr ", "")];
    for (language, key) in cases {
        let entry = LiveEntry {
            id: "live".to_owned(),
            name: "Chat speech".to_owned(),
            kind: LiveKind::OpenAi(chat(&url, key, language)),
            extra: false,
        };
        let chosen = Chosen::new(
            &entry,
            ModelStore::in_dir("unused"),
            reqwest::Client::new(),
            Recorder::disabled().metrics(),
            &WordList::default(),
            Adapters::default(),
            std::path::Path::new("unused"),
        );
        let mut session = chosen.connect().await.expect("connected");
        session.send_frame(&ramp(0)).await.expect("buffer frame");
        assert!(rx.try_recv().is_err(), "the request waits for release");
        session.finish_audio().await.expect("capture ended");
        let event = tokio::time::timeout(Duration::from_secs(10), session.next_event())
            .await
            .expect("the answer arrives")
            .expect("transcription succeeds");
        assert_eq!(
            event,
            RecognizerEvent::Final("hallo welt".to_owned()),
            "{language}"
        );

        let captured = received(&mut rx).await.expect("one request");
        assert_eq!(captured.path, "/v1/chat/completions");
        assert_eq!(captured.headers["content-type"], "application/json");
        match (key.is_empty(), captured.headers.get("authorization")) {
            (true, None) => {}
            (false, Some(value)) => assert_eq!(value, &format!("Bearer {key}")),
            other => panic!("key {key:?}: {other:?}"),
        }
        let mut body: serde_json::Value = serde_json::from_slice(&captured.body).unwrap();
        let data = body["messages"][0]["content"][0]["input_audio"]["data"]
            .as_str()
            .expect("base64 audio")
            .to_owned();
        assert_eq!(
            STANDARD.decode(&data).unwrap(),
            sent_wav,
            "same WAV as the upload"
        );
        body["messages"][0]["content"][0]["input_audio"]["data"] = serde_json::json!("<wav>");
        let mut expected = serde_json::json!({
            "model": "the-model",
            "messages": [{"role": "user", "content": [{
                "type": "input_audio",
                "input_audio": {"data": "<wav>", "format": "wav"}
            }]}],
        });
        if !language.trim().is_empty() {
            expected["language"] = serde_json::json!(language.trim().to_lowercase());
        }
        assert_eq!(body, expected, "{language}");
        assert!(rx.try_recv().is_err(), "one request per capture");
    }
}

#[tokio::test]
async fn transcriptions_entry_sends_language_only_when_set() {
    for (language, expected) in [("", None), ("DE", Some("de"))] {
        let (url, mut rx) = serve_sse("").await;
        let entry = OpenAiLive {
            language: Language::parse(language).unwrap(),
            ..live(&url, "")
        };
        dictate(&recognizer(entry)).await.expect("transcription");
        let found = parts(&received(&mut rx).await.expect("upload"))
            .into_iter()
            .find(|(headers, _)| headers.contains("name=\"language\""))
            .map(|(_, value)| String::from_utf8(value).unwrap());
        assert_eq!(found.as_deref(), expected, "{language:?}");
    }
}

#[tokio::test]
async fn the_live_metric_names_the_request_kind() {
    let (url, _rx) = serve(
        200,
        r#"{"text":"plain","choices":[{"message":{"content":"chat"}}]}"#,
    )
    .await;
    for (entry, kind) in [
        (live(&url, ""), "transcriptions"),
        (chat(&url, "", ""), "chat"),
    ] {
        let dir =
            std::env::temp_dir().join(format!("thalvi-live-kind-{}-{kind}", std::process::id()));
        let kinds = RecordKinds {
            metrics: true,
            ..RecordKinds::default()
        };
        let recorder = Recorder::in_dir(kinds, &dir).expect("metrics recorder");
        let recognizer = OpenAiRecognizer::new(
            "local",
            entry,
            reqwest::Client::new(),
            recorder.metrics(),
            &WordList::default(),
        );
        dictate(&recognizer).await.expect("transcription");
        recorder.finish().await;
        let lines = std::fs::read_to_string(dir.join("metrics.jsonl")).expect("metrics");
        let done: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).expect("metric line"))
            .filter(|event: &serde_json::Value| event["event"] == "desktop_live_stt")
            .collect();
        assert_eq!(done.len(), 1, "{kind}");
        assert_eq!(
            (&done[0]["outcome"], &done[0]["request"]),
            (&"ok".into(), &kind.into())
        );
        std::fs::remove_dir_all(dir).expect("remove test metrics");
    }
}

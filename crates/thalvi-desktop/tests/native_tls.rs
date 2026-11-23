//! Exercises private-CA trust through real transcription and secure WebSocket requests.

#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test failures panic")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use thalvi_desktop::entries::Realtime;
use thalvi_desktop::frame::FRAME_SAMPLES;
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::openai_live::OpenAiRecognizer;
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::realtime::RealtimeRecognizer;
use thalvi_desktop::recognizer::{Recognizer, RecognizerSession};
use thalvi_desktop::report::{CertificateProblem, UserError};
use thalvi_desktop::secondary::{
    LiveRequest, OpenAiLive, SecondaryApi, SecondaryError, SecondaryStt, transcribe,
};
use thalvi_desktop::writing::{Writing, WritingError};

#[derive(Deserialize)]
struct Fixture {
    ca: String,
    directory: String,
    trusted: u16,
    untrusted: u16,
}

#[test]
fn native_certificate_trust() {
    if let Ok(fixture) = std::env::var("THALVI_TLS_FIXTURE") {
        let fixture: Fixture = serde_json::from_str(&fixture).unwrap();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(check(&fixture));
        return;
    }

    let mut server = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/native_tls.py"
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let fixture: Fixture = serde_json::from_str(&line).unwrap();
    // Trust is scoped to a fresh process; the host certificate store is never changed.
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "native_certificate_trust", "--nocapture"])
        .env("THALVI_TLS_FIXTURE", &line)
        .env("SSL_CERT_FILE", &fixture.ca)
        .env("SSL_CERT_DIR", &fixture.directory)
        .status();
    server.stdin.take().unwrap().write_all(b"stop\n").unwrap();
    assert!(server.wait().unwrap().success());
    assert!(result.unwrap().success());
}

async fn check(fixture: &Fixture) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (host, port, problem) in [
        ("localhost", fixture.trusted, None),
        (
            "localhost",
            fixture.untrusted,
            Some(CertificateProblem::UntrustedIssuer),
        ),
        (
            "127.0.0.1",
            fixture.trusted,
            Some(CertificateProblem::WrongHost),
        ),
    ] {
        let settings = SecondaryStt::new(
            &format!("https://{host}:{port}/v1"),
            "fixture",
            String::new(),
            SecondaryApi::OpenAi,
            true,
        )
        .unwrap();
        let response = transcribe(
            &client,
            &settings,
            &[],
            &thalvi_desktop::phrases::WordList::default(),
        )
        .await;
        if let Some(problem) = problem {
            assert_eq!(response.unwrap_err(), SecondaryError::Certificate(problem));
            let writer = Writing::new(
                &format!("https://{host}:{port}/v1"),
                "fixture",
                String::new(),
                None,
                vec![],
                vec![],
            )
            .unwrap();
            let error = writer.start(&client, "Test").await.err().unwrap();
            assert_eq!(error, WritingError::Certificate(problem));
            assert_eq!(error.headline(), "Server certificate problem");
            assert!(error.to_string().contains(&problem.to_string()));
            let recognizer = OpenAiRecognizer::new(
                "TLS fixture",
                OpenAiLive {
                    settings,
                    request: LiveRequest::Transcriptions,
                    language: None,
                },
                client.clone(),
                Recorder::disabled().metrics(),
                &thalvi_desktop::phrases::WordList::default(),
            );
            let mut session = recognizer.connect().await.unwrap();
            session.send_frame(&[0; FRAME_SAMPLES]).await.unwrap();
            session.finish_audio().await.unwrap();
            let error = tokio::time::timeout(Duration::from_secs(10), session.next_event())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.headline(), "Server certificate problem");
            assert!(error.to_string().contains(&problem.to_string()));
            assert!(error.to_string().contains("Nothing was delivered"));
            session.close().await;
        } else {
            assert_eq!(response.unwrap().text, "Trusted transcription.");
        }
        let websocket = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(format!("wss://{host}:{port}/")),
        )
        .await
        .unwrap();
        match problem {
            Some(problem) => {
                assert_eq!(
                    CertificateProblem::from_error(&websocket.unwrap_err()),
                    Some(problem)
                );
                let realtime =
                    Realtime::new(&format!("wss://{host}:{port}/"), "fixture", String::new())
                        .unwrap();
                let recognizer =
                    RealtimeRecognizer::new("TLS fixture", &realtime, &WordList::default());
                let error = recognizer.connect().await.err().unwrap();
                assert_eq!(error.headline(), "Server certificate problem");
                assert!(error.to_string().contains(&problem.to_string()));
            }
            None => assert!(websocket.is_ok(), "WSS trust for {host}:{port}"),
        }
    }
}

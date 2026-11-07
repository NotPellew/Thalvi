//! The desktop asks the server whether its recognizer boosts a word list, and saves the list there.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Value, json};
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::phrases::{WordList, server_boosts, share};
use thalvi_desktop::recognizer::ServerUrl;
use tokio::sync::mpsc;

async fn serve(status: u16, reply: Value) -> (ServerUrl, mpsc::UnboundedReceiver<Value>) {
    let (sent, received) = mpsc::unbounded_channel();
    let status = StatusCode::from_u16(status).expect("status");
    let get_reply = reply.clone();
    let app = Router::new().route(
        "/phrases",
        get(move || async move { (status, axum::Json(get_reply)) }).put(
            move |axum::Json(body): axum::Json<Value>| async move {
                let _ = sent.send(body);
                (status, axum::Json(reply))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let server = ServerUrl::parse(&format!("ws://{address}")).expect("server url");
    (server, received)
}

async fn unreachable() -> ServerUrl {
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = unused.local_addr().expect("address");
    drop(unused);
    ServerUrl::parse(&format!("ws://{address}")).expect("server url")
}

#[tokio::test]
async fn the_server_says_whether_its_recognizer_boosts_a_word_list() {
    let metrics = Recorder::disabled().metrics();
    let client = reqwest::Client::new();
    for (boosting, expected) in [
        (json!(true), Ok(true)),
        (json!(false), Ok(false)),
        (Value::Null, Err("could not reach its speech recognizer")),
    ] {
        let (server, _) = serve(200, json!({"phrases": [], "boosting": boosting})).await;
        let got = server_boosts(&client, &server, &metrics).await;
        match expected {
            Ok(expected) => assert_eq!(got, Ok(expected)),
            Err(part) => assert!(got.expect_err("an error").contains(part)),
        }
    }
    let error = server_boosts(&client, &unreachable().await, &metrics)
        .await
        .expect_err("unreachable");
    assert!(error.starts_with("Thalvi could not reach the server at ws://127.0.0.1:"));
}

#[tokio::test]
async fn the_word_list_is_sent_to_the_server_and_a_refusal_is_shown_as_the_server_words_it() {
    let metrics = Recorder::disabled().metrics();
    let client = reqwest::Client::new();
    let list = WordList::new(vec!["Cybrex2".to_owned(), "Thalvi".to_owned()]).expect("list");
    let (server, mut sent) = serve(204, Value::Null).await;
    assert_eq!(share(&client, &server, &list, &metrics).await, Ok(()));
    assert_eq!(
        sent.recv().await.expect("body"),
        json!({"phrases": ["Cybrex2", "Thalvi"]})
    );
    let refusal = "Each word list entry must be 1 to 40 characters.";
    let (server, _) = serve(400, json!({"error": refusal})).await;
    assert_eq!(
        share(&client, &server, &list, &metrics).await,
        Err(refusal.to_owned())
    );
    let error = share(&client, &unreachable().await, &list, &metrics)
        .await
        .expect_err("unreachable");
    assert!(error.starts_with("The word list is saved here, but Thalvi could not reach"));
}

#[tokio::test]
async fn a_reachable_server_reports_unsupported_word_lists_and_bad_replies_as_such() {
    let metrics = Recorder::disabled().metrics();
    let client = reqwest::Client::new();
    let list = WordList::new(vec!["Thalvi".to_owned()]).expect("list");
    for (status, reply, expected) in [
        (
            404,
            Value::Null,
            "doesn't support a word list yet; it may need updating",
        ),
        (503, Value::Null, "HTTP 503"),
        (
            200,
            json!({"boosting": "yes"}),
            "response Thalvi could not read",
        ),
    ] {
        let (server, _) = serve(status, reply).await;
        let error = server_boosts(&client, &server, &metrics)
            .await
            .expect_err("unavailable word list");
        assert!(error.contains(expected), "{status}: {error}");
        assert!(!error.contains("could not reach"), "{status}: {error}");
        if status == 404 {
            let error = share(&client, &server, &list, &metrics)
                .await
                .expect_err("unsupported word list");
            assert!(error.contains(expected), "{error}");
        }
    }
}

//! Model downloads are verified, cached, and never leave a bad file behind.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::fmt::Write;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::routing::get;
use sha2::{Digest, Sha256};
use thalvi_desktop::models::{Asset, ModelError, ModelKind, ModelStore};

const GOOD: &[u8] = b"pretend this is a model";

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

async fn serve() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/good.onnx", get(|| async { GOOD }))
        .route("/other.onnx", get(|| async { "something else entirely" }))
        .route("/gone.onnx", get(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/stalls.onnx",
            get(|| async {
                Body::from_stream(futures_util::stream::unfold(0, |sent| async move {
                    if sent == 0 {
                        Some((
                            Ok::<_, std::io::Error>(Bytes::from_static(b"some bytes")),
                            1,
                        ))
                    } else {
                        std::future::pending().await
                    }
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, server)
}

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("models-{test}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn leftovers(dir: &PathBuf) -> Vec<String> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_missing_file_is_downloaded_once_and_then_served_from_disk() {
    let (address, server) = serve().await;
    let dir = dir("download");
    let store = ModelStore::in_dir(&dir);
    let url = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let asset = Asset {
        kind: ModelKind::Wake,
        file_name: "good.onnx",
        url: &url,
        size: GOOD.len() as u64,
        sha256: &sha,
    };

    let path = store.fetch(&asset).await.expect("downloads");
    assert_eq!(fs::read(&path).expect("saved"), GOOD);
    assert_eq!(leftovers(&dir), ["good.onnx"]);

    server.abort();
    let _ = server.await;
    assert_eq!(store.fetch(&asset).await.expect("cached"), path);
}

#[tokio::test]
async fn a_download_with_the_wrong_checksum_is_deleted_and_refused() {
    let (address, _server) = serve().await;
    let dir = dir("mismatch");
    let url = format!("http://{address}/other.onnx");
    let sha = sha256(GOOD);
    let asset = Asset {
        kind: ModelKind::Wake,
        file_name: "good.onnx",
        url: &url,
        size: GOOD.len() as u64,
        sha256: &sha,
    };

    let error = ModelStore::in_dir(&dir)
        .fetch(&asset)
        .await
        .expect_err("refused");
    assert!(matches!(error, ModelError::Checksum { .. }));
    assert_eq!(
        error.to_string(),
        "The downloaded wake model good.onnx did not match its expected checksum, so it was thrown away. Try again, and report it if it keeps happening."
    );
    assert_eq!(leftovers(&dir), Vec::<String>::new());
}

#[tokio::test]
async fn a_failed_download_says_so_and_leaves_nothing_behind() {
    let (address, _server) = serve().await;
    let dir = dir("missing");
    let url = format!("http://{address}/gone.onnx");
    let sha = sha256(GOOD);
    let asset = Asset {
        kind: ModelKind::Wake,
        file_name: "good.onnx",
        url: &url,
        size: GOOD.len() as u64,
        sha256: &sha,
    };

    let error = ModelStore::in_dir(&dir)
        .fetch(&asset)
        .await
        .expect_err("fails");
    assert!(matches!(error, ModelError::Download { .. }));
    assert_eq!(
        error.to_string(),
        "Could not download the wake model good.onnx. Check the internet connection and try again."
    );
    assert_eq!(leftovers(&dir), Vec::<String>::new());
}

#[tokio::test]
async fn a_damaged_file_on_disk_is_downloaded_again() {
    let (address, _server) = serve().await;
    let dir = dir("damaged");
    fs::create_dir_all(&dir).expect("dir");
    fs::write(dir.join("good.onnx"), b"half a mod").expect("damaged file");
    let url = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let asset = Asset {
        kind: ModelKind::Wake,
        file_name: "good.onnx",
        url: &url,
        size: GOOD.len() as u64,
        sha256: &sha,
    };

    let path = ModelStore::in_dir(&dir)
        .fetch(&asset)
        .await
        .expect("repairs");
    assert_eq!(fs::read(path).expect("saved"), GOOD);
}

#[tokio::test]
async fn a_speech_model_is_named_as_one_when_it_cannot_be_trusted() {
    let (address, _server) = serve().await;
    let dir = dir("speech");
    let url = format!("http://{address}/other.onnx");
    let sha = sha256(GOOD);
    let asset = Asset {
        kind: ModelKind::Speech,
        file_name: "speech.onnx",
        url: &url,
        size: GOOD.len() as u64,
        sha256: &sha,
    };

    let error = ModelStore::in_dir(&dir)
        .fetch(&asset)
        .await
        .expect_err("refused");
    assert_eq!(
        error.to_string(),
        "The downloaded speech model speech.onnx did not match its expected checksum, so it was thrown away. Try again, and report it if it keeps happening."
    );
}

#[tokio::test]
async fn a_download_that_is_cancelled_leaves_no_partial_file() {
    let (address, _server) = serve().await;
    let dir = dir("cancelled");
    let url = format!("http://{address}/stalls.onnx");
    let sha = sha256(GOOD);
    let task = {
        let dir = dir.clone();
        tokio::spawn(async move {
            let asset = Asset {
                kind: ModelKind::Speech,
                file_name: "good.onnx",
                url: &url,
                size: GOOD.len() as u64,
                sha256: &sha,
            };
            ModelStore::in_dir(dir).fetch(&asset).await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        // A directory listing can report an open file as empty on Windows; ask the file itself.
        while !fs::read_dir(&dir).into_iter().flatten().any(|entry| {
            fs::metadata(entry.expect("entry").path()).is_ok_and(|meta| meta.len() > 0)
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the download starts");

    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    assert_eq!(leftovers(&dir), Vec::<String>::new());
}

#[tokio::test]
async fn a_damaged_file_is_removed_even_when_it_cannot_be_downloaded_again() {
    let (address, _server) = serve().await;
    let dir = dir("damaged-offline");
    fs::create_dir_all(&dir).expect("dir");
    fs::write(dir.join("good.onnx"), b"half a mod").expect("damaged file");
    let url = format!("http://{address}/gone.onnx");
    let sha = sha256(GOOD);
    let asset = Asset {
        kind: ModelKind::Speech,
        file_name: "good.onnx",
        url: &url,
        size: GOOD.len() as u64,
        sha256: &sha,
    };

    let error = ModelStore::in_dir(&dir)
        .fetch(&asset)
        .await
        .expect_err("cannot download");
    assert!(matches!(error, ModelError::Replaced { .. }));
    assert_eq!(
        error.to_string(),
        "The stored speech model good.onnx was damaged, so it was removed. Could not download the speech model good.onnx. Check the internet connection and try again."
    );
    assert_eq!(leftovers(&dir), Vec::<String>::new());
}

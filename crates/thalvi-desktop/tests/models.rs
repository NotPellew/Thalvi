//! Model downloads are verified, cached, and never leave a bad file behind.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::fmt::Write;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::header::USER_AGENT;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use sha2::{Digest, Sha256};
use thalvi_desktop::models::{
    Asset, CatalogSpeech, ModelError, ModelKind, ModelStore, SPOTTER, SpeechAssets,
    SpeechDownloads, SpeechProgress,
};

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
        .route(
            "/needs-agent.onnx",
            get(|headers: HeaderMap| async move {
                if headers.contains_key(USER_AGENT) {
                    Ok(GOOD)
                } else {
                    Err(StatusCode::FORBIDDEN)
                }
            }),
        )
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
async fn a_download_names_the_client_for_hosts_that_refuse_anonymous_ones() {
    let (address, _server) = serve().await;
    let url = format!("http://{address}/needs-agent.onnx");
    let sha = sha256(GOOD);
    let asset = Asset {
        kind: ModelKind::Spotter,
        file_name: "needs-agent.onnx",
        url: &url,
        size: GOOD.len() as u64,
        sha256: &sha,
    };
    let path = ModelStore::in_dir(dir("agent"))
        .fetch(&asset)
        .await
        .expect("downloads");
    assert_eq!(fs::read(path).expect("saved"), GOOD);
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

fn asset<'a>(url: &'a str, sha: &'a str) -> Asset<'a> {
    Asset {
        kind: ModelKind::Speech,
        file_name: "good.onnx",
        url,
        size: GOOD.len() as u64,
        sha256: sha,
    }
}

#[tokio::test]
async fn two_fetches_of_one_asset_at_once_both_succeed() {
    let (address, _server) = serve().await;
    let dir = dir("twice");
    let url = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let store = ModelStore::in_dir(&dir);

    let asset = asset(&url, &sha);

    let (first, second) = tokio::join!(store.fetch(&asset), store.fetch(&asset));

    assert_eq!(first.expect("first"), second.expect("second"));
    assert_eq!(fs::read(dir.join("good.onnx")).expect("saved"), GOOD);
    assert_eq!(leftovers(&dir), ["good.onnx"]);
}

/// Serves `GOOD`, but first stores it as the final file the way a fetch that finished earlier
/// would have, and keeps that file open so that Windows will not replace it.
async fn serve_after_another_fetch_finished(
    dir: PathBuf,
    held: Arc<Mutex<Vec<fs::File>>>,
) -> SocketAddr {
    let app = Router::new().route(
        "/good.onnx",
        get(move || {
            let (dir, held) = (dir.clone(), Arc::clone(&held));
            async move {
                fs::create_dir_all(&dir).expect("dir");
                let mut options = fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(windows)]
                std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 1);
                let mut file = options.open(dir.join("good.onnx")).expect("final file");
                std::io::Write::write_all(&mut file, GOOD).expect("written");
                file.set_modified(SystemTime::now() - Duration::from_hours(1))
                    .expect("backdated");
                held.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(file);
                GOOD
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    address
}

#[tokio::test]
async fn a_file_that_another_fetch_stored_first_is_used_and_not_replaced() {
    let dir = dir("raced");
    let held = Arc::new(Mutex::new(Vec::new()));
    let address = serve_after_another_fetch_finished(dir.clone(), Arc::clone(&held)).await;
    let url = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);

    let path = ModelStore::in_dir(&dir)
        .fetch(&asset(&url, &sha))
        .await
        .expect("uses the stored file");

    assert_eq!(fs::read(&path).expect("readable"), GOOD);
    let age = SystemTime::now()
        .duration_since(fs::metadata(&path).expect("meta").modified().expect("time"))
        .expect("past");
    assert!(age > Duration::from_mins(30), "replaced: {age:?}");
    assert_eq!(leftovers(&dir), ["good.onnx"]);
}

const FILES: [&str; 4] = [
    "m.encoder.onnx",
    "m.decoder.onnx",
    "m.joiner.onnx",
    "m.tokens.txt",
];

/// A four-file speech model whose files all hold `GOOD` and are fetched from `urls`.
fn speech<'a>(urls: [&'a str; 4], sha: &'a str) -> SpeechAssets<'a> {
    let [encoder, decoder, joiner, tokens] = std::array::from_fn(|i| Asset {
        kind: ModelKind::Speech,
        file_name: FILES[i],
        url: urls[i],
        size: GOOD.len() as u64,
        sha256: sha,
    });
    SpeechAssets {
        encoder,
        decoder,
        joiner,
        tokens,
    }
}

#[tokio::test]
async fn a_speech_model_is_cached_only_when_every_file_has_its_pinned_size() {
    let sha = sha256(GOOD);
    let assets = speech(["http://unused"; 4], &sha);
    for (case, files, cached) in [
        ("all present", &[GOOD, GOOD, GOOD, GOOD][..], true),
        ("one missing", &[GOOD, GOOD, GOOD][..], false),
        (
            "one cut short",
            &[GOOD, GOOD, b"pretend this", GOOD][..],
            false,
        ),
        ("none", &[][..], false),
    ] {
        let dir = dir(&format!("cached-{}", case.replace(' ', "-")));
        fs::create_dir_all(&dir).expect("dir");
        for (file, bytes) in FILES.iter().zip(files) {
            fs::write(dir.join(file), bytes).expect("file");
        }
        assert_eq!(
            ModelStore::in_dir(&dir).is_cached(&assets).await,
            cached,
            "{case}"
        );
    }
}

#[tokio::test]
async fn the_catalog_lists_each_model_with_its_size_license_languages_source_and_whether_it_is_downloaded()
 {
    let models = ModelStore::in_dir(dir("catalog")).speech_models().await;

    let listed: Vec<_> = models
        .iter()
        .map(|model| {
            (
                model.name,
                model.title,
                model.size,
                model.license,
                model.languages.len(),
                model.source,
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (
                "nemotron_160ms",
                "Nemotron Streaming",
                661_919_416,
                "NVIDIA Open Model License",
                1,
                "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25"
            ),
            (
                "unified_160ms",
                "Nemotron Unified",
                646_518_349,
                "OpenMDW-1.1",
                25,
                "https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b"
            ),
        ]
    );
    assert!(models.iter().all(|model| !model.cached));
    let unified = &models[1].languages;
    assert!(unified.is_sorted() && unified.contains(&"English") && unified.contains(&"Ukrainian"));
    assert_eq!(models[0].languages, ["English"]);
    for model in &models {
        assert!(CatalogSpeech::named(model.name).is_some(), "{}", model.name);
    }
    assert_eq!(CatalogSpeech::named("elsewhere"), None);
}

#[test]
fn catalog_models_keep_their_files_apart_and_pinned() {
    let mut names = Vec::new();
    for model in CatalogSpeech::ALL {
        let assets = model.assets();
        for asset in [assets.encoder, assets.decoder, assets.joiner, assets.tokens] {
            assert!(
                asset.file_name.starts_with(model.name()),
                "{}",
                asset.file_name
            );
            assert!(asset.url.starts_with("https://"), "{}", asset.url);
            assert_eq!(asset.sha256.len(), 64, "{}", asset.file_name);
            names.push(asset.file_name);
        }
    }
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count);
}

#[test]
fn the_ending_phrase_model_files_are_pinned_and_apart_from_the_other_models() {
    let assets = [
        SPOTTER.encoder,
        SPOTTER.decoder,
        SPOTTER.joiner,
        SPOTTER.pieces,
    ];
    let mut names: Vec<&str> = assets.iter().map(|asset| asset.file_name).collect();
    for asset in assets {
        assert_eq!(asset.kind, ModelKind::Spotter, "{}", asset.file_name);
        assert!(asset.url.starts_with("https://"), "{}", asset.url);
        assert_eq!(asset.sha256.len(), 64, "{}", asset.file_name);
    }
    for model in CatalogSpeech::ALL {
        let speech = model.assets();
        names.extend(
            [speech.encoder, speech.decoder, speech.joiner, speech.tokens].map(|a| a.file_name),
        );
    }
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count);
}

#[tokio::test]
async fn a_speech_download_reports_progress_up_to_done() {
    let (address, _server) = serve().await;
    let dir = dir("speech-progress");
    let url = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let assets = speech([&url; 4], &sha);
    let store = ModelStore::in_dir(&dir);

    let mut events = Vec::new();
    store
        .fetch_speech(&assets, |event| events.push(event))
        .await
        .expect("downloads");

    assert_eq!(
        events,
        [
            SpeechProgress::Verifying,
            SpeechProgress::Downloading { percent: 25 },
            SpeechProgress::Downloading { percent: 50 },
            SpeechProgress::Downloading { percent: 75 },
            SpeechProgress::Downloading { percent: 100 },
            SpeechProgress::Done,
        ]
    );
    assert!(store.is_cached(&assets).await);
    assert_eq!(leftovers(&dir).len(), 4);
}

#[tokio::test]
async fn a_speech_download_with_a_bad_checksum_says_so_and_is_not_cached() {
    let (address, _server) = serve().await;
    let dir = dir("speech-checksum");
    let good = format!("http://{address}/good.onnx");
    let other = format!("http://{address}/other.onnx");
    let sha = sha256(GOOD);
    let assets = speech([&good, &good, &other, &good], &sha);
    let store = ModelStore::in_dir(&dir);

    let mut events = Vec::new();
    let error = store
        .fetch_speech(&assets, |event| events.push(event))
        .await
        .expect_err("refused");

    assert_eq!(
        error.to_string(),
        "The downloaded speech model m.joiner.onnx did not match its expected checksum, so it was thrown away. Try again, and report it if it keeps happening."
    );
    assert_ne!(events.last(), Some(&SpeechProgress::Done));
    let mut kept = leftovers(&dir);
    kept.sort();
    assert_eq!(kept, ["m.decoder.onnx", "m.encoder.onnx"]);
    assert!(!store.is_cached(&assets).await);
}

#[tokio::test]
async fn a_speech_model_is_downloaded_once_at_a_time() {
    let (address, _server) = serve().await;
    let dir = dir("speech-twice");
    let stalls = format!("http://{address}/stalls.onnx");
    let good = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let downloads = Arc::new(SpeechDownloads::default());
    let store = ModelStore::in_dir(&dir);
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    let first = {
        let (downloads, store, stalls, sha) = (
            Arc::clone(&downloads),
            store.clone(),
            stalls.clone(),
            sha.clone(),
        );
        tokio::spawn(async move {
            let assets = speech([&stalls; 4], &sha);
            downloads
                .run("m", &store, &assets, |event| {
                    let _ = started_tx.send(event);
                })
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .expect("the first download starts");

    let assets = speech([&good; 4], &sha);
    let refused = downloads
        .run("m", &store, &assets, |_| {})
        .await
        .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        "The speech model m is already being downloaded."
    );
    downloads
        .run("other", &store, &assets, |_| {})
        .await
        .expect("a different model is not held up");

    first.abort();
    assert!(first.await.expect_err("cancelled").is_cancelled());
    downloads
        .run("m", &store, &assets, |_| {})
        .await
        .expect("a finished or cancelled download frees the model");
}

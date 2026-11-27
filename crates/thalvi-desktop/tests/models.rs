//! Model downloads are verified, cached, and never leave a bad file behind.

#![cfg(test)]

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
use thalvi_desktop::device::{Device, Fallback, Gpu};
use thalvi_desktop::models::{
    Asset, CatalogSpeech, ListedSpeech, ModelError, ModelKind, ModelStore, SPOTTER, SpeechAssets,
    SpeechDownloads, SpeechProgress, WHISPER_TURBO,
};
use thalvi_desktop::report::causes;
use thalvi_desktop::transducer::SpeechChoice;
use thalvi_desktop::whisper::WhisperBackend;

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
        .route("/forbidden.onnx", get(|| async { StatusCode::FORBIDDEN }))
        .route(
            "/endless.onnx",
            get(|| async {
                Body::from_stream(futures_util::stream::repeat_with(|| {
                    Ok::<_, std::io::Error>(Bytes::from_static(GOOD))
                }))
            }),
        )
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
async fn a_download_that_is_not_the_pinned_file_is_deleted_and_refused() {
    let (address, _server) = serve().await;
    for (served, kind, file_name, noun) in [
        ("other.onnx", ModelKind::Wake, "good.onnx", "wake"),
        ("endless.onnx", ModelKind::Wake, "good.onnx", "wake"),
        ("other.onnx", ModelKind::Speech, "speech.onnx", "speech"),
    ] {
        let dir = dir(&format!("mismatch-{noun}-{served}"));
        let url = format!("http://{address}/{served}");
        let sha = sha256(GOOD);
        let asset = Asset {
            kind,
            file_name,
            url: &url,
            size: GOOD.len() as u64,
            sha256: &sha,
        };

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ModelStore::in_dir(&dir).fetch(&asset),
        )
        .await
        .expect("stops on its own")
        .expect_err("refused");
        assert!(matches!(error, ModelError::Checksum { .. }), "{served}");
        assert_eq!(
            error.to_string(),
            format!(
                "The downloaded {noun} model {file_name} did not match its expected checksum, so it was thrown away. Try again, and report it if it keeps happening."
            ),
            "{served}"
        );
        assert_eq!(leftovers(&dir), Vec::<String>::new(), "{served}");
    }
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
        "Could not download the wake model good.onnx: the server refused it (HTTP 404 Not Found). Try again, and report it if it keeps happening."
    );
    assert_eq!(leftovers(&dir), Vec::<String>::new());
}

/// Serves one connection with `reply` written verbatim, then closes it.
async fn serve_raw(reply: &'static [u8]) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0; 1024];
        let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, reply).await;
    });
    address
}

#[tokio::test]
async fn a_failed_download_says_which_step_failed_and_keeps_the_causes_without_the_address() {
    let (address, _server) = serve().await;
    let cut = serve_raw(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nsome bytes").await;
    let sha = sha256(GOOD);
    for (url, message, metric) in [
        (
            // Binding to port 0 always assigns the listener a different port.
            "http://127.0.0.1:0/good.onnx?signature=secret".to_owned(),
            "could not connect to 127.0.0.1. Check the internet connection and try again.",
            "connect",
        ),
        (
            format!("http://{address}/forbidden.onnx?signature=secret"),
            "the server refused it (HTTP 403 Forbidden). Try again, and report it if it keeps happening.",
            "status:403",
        ),
        (
            format!("http://{cut}/good.onnx?signature=secret"),
            "the download broke off. Try again, and report it if it keeps happening.",
            "body",
        ),
    ] {
        let dir = dir("failure");
        let error = ModelStore::in_dir(&dir)
            .fetch(&asset(&url, &sha))
            .await
            .expect_err("fails");

        assert_eq!(
            error.to_string(),
            format!("Could not download the speech model good.onnx: {message}")
        );
        assert_eq!(error.metric_kind(), metric, "{url}");
        let causes = causes(&error);
        assert!(!causes.is_empty(), "{url}");
        assert!(
            causes.iter().all(|cause| !cause.contains("signature")),
            "{causes:?}"
        );
        assert_eq!(leftovers(&dir), Vec::<String>::new(), "{url}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_download_that_stalls_times_out_instead_of_hanging_after_a_wait_that_grows_with_its_size()
{
    let (address, _server) = serve().await;
    let url = format!("http://{address}/stalls.onnx");
    let sha = sha256(GOOD);

    // Two minutes, and a second for every 500 kB.
    for (size, wait) in [(GOOD.len() as u64, 120), (60_000_000, 240)] {
        let started = tokio::time::Instant::now();
        let error = ModelStore::in_dir(dir("timeout"))
            .fetch(&Asset {
                size,
                ..asset(&url, &sha)
            })
            .await
            .expect_err("times out");

        let waited = started.elapsed().as_secs();
        assert!((wait..wait + 10).contains(&waited), "{size}: {waited} s");
        assert_eq!(
            error.to_string(),
            "Could not download the speech model good.onnx: the download timed out. Check the internet connection and try again."
        );
        assert_eq!(error.metric_kind(), "timeout");
        assert!(!causes(&error).is_empty(), "{error}");
    }
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_file_blocking_the_model_folder_reports_that_the_path_is_not_a_directory() {
    let dir = dir("folder-is-a-file");
    fs::write(&dir, b"a file, not the model folder").expect("blocked folder");
    let sha = sha256(GOOD);
    let error = ModelStore::in_dir(&dir)
        .fetch(&asset("http://127.0.0.1:1/good.onnx", &sha))
        .await
        .expect_err("a file cannot contain model files");

    assert!(
        matches!(&error, ModelError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotADirectory),
        "{error}"
    );
    assert_eq!(
        fs::read(&dir).expect("blocking file remains"),
        b"a file, not the model folder"
    );
    fs::remove_file(dir).expect("cleanup");
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
        "The stored speech model good.onnx was damaged, so it was removed. Could not download the speech model good.onnx: the server refused it (HTTP 404 Not Found). Try again, and report it if it keeps happening."
    );
    assert_eq!(error.metric_kind(), "status:404");
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
        encoder_data: None,
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
    let models = ModelStore::in_dir(dir("catalog"))
        .speech_models(|_| Device::Cpu(Fallback::CpuModel))
        .await;

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
            (
                "parakeet_tdt_v3",
                "Parakeet TDT v3",
                670_576_784,
                "CC BY 4.0",
                25,
                "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3"
            ),
        ]
    );
    assert!(models.iter().all(|model| !model.cached));
    let unified = &models[1].languages;
    assert!(unified.is_sorted() && unified.contains(&"English") && unified.contains(&"Ukrainian"));
    assert_eq!(models[0].languages, ["English"]);
    let mut descriptions: Vec<_> = models.iter().map(|model| model.description).collect();
    descriptions.sort_unstable();
    descriptions.dedup();
    assert_eq!(
        descriptions.len(),
        models.len(),
        "each card says its own trade-off"
    );
    for model in &models {
        assert!(CatalogSpeech::named(model.name).is_some(), "{}", model.name);
        assert_eq!(
            ListedSpeech::named(model.name).expect("listed").source(),
            model.source,
            "the model page opened from Settings"
        );
    }
    assert_eq!(CatalogSpeech::named("elsewhere"), None);
}

#[test]
fn model_files_are_pinned_and_kept_apart() {
    let mut names = Vec::new();
    for asset in [
        SPOTTER.encoder,
        SPOTTER.decoder,
        SPOTTER.joiner,
        SPOTTER.pieces,
    ] {
        assert_eq!(asset.kind, ModelKind::Spotter, "{}", asset.file_name);
        assert!(asset.url.starts_with("https://"), "{}", asset.url);
        assert_eq!(asset.sha256.len(), 64, "{}", asset.file_name);
        names.push(asset.file_name);
    }
    for model in CatalogSpeech::ALL {
        for assets in std::iter::once(model.assets()).chain(model.gpu_assets()) {
            let files = [assets.encoder, assets.decoder, assets.joiner, assets.tokens]
                .into_iter()
                .chain(assets.encoder_data);
            for asset in files {
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
    }
    // The GPU export reads the CPU files' tokens, so those are the one name listed twice.
    let tokens = CatalogSpeech::Unified160ms.assets().tokens.file_name;
    names.retain(|name| *name != tokens);
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count);
}

#[test]
fn a_discrete_gpu_loads_the_fp16_export_and_every_other_case_the_int8_files() {
    let rtx = Device::Gpu(Gpu {
        name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
        api: "Vulkan",
    });
    let cpu = Device::Cpu(Fallback::NoUsableGpu);
    for (model, device, encoder, with_data) in [
        (
            CatalogSpeech::Unified160ms,
            &rtx,
            "unified_160ms.gpu/encoder.fp16.onnx",
            true,
        ),
        (
            CatalogSpeech::Unified160ms,
            &cpu,
            "unified_160ms.encoder.int8.onnx",
            false,
        ),
        (
            CatalogSpeech::Nemotron160ms,
            &rtx,
            "nemotron_160ms.encoder.int8.onnx",
            false,
        ),
        (
            CatalogSpeech::ParakeetTdtV3,
            &rtx,
            "parakeet_tdt_v3.encoder.int8.onnx",
            false,
        ),
    ] {
        let assets = model.assets_on(device);
        assert_eq!(assets.encoder.file_name, encoder, "{model:?} on {device:?}");
        assert_eq!(
            assets.encoder_data.is_some(),
            with_data,
            "{model:?} on {device:?}"
        );
        assert_eq!(
            model.uses_gpu(),
            model == CatalogSpeech::Unified160ms,
            "{model:?}"
        );
    }
    let gpu = CatalogSpeech::Unified160ms.assets_on(&rtx);
    assert_eq!(
        gpu.encoder_data.map(|data| data.file_name),
        Some("unified_160ms.gpu/encoder.fp16.onnx.data"),
        "the encoder finds its data file beside itself"
    );
    assert_eq!(
        gpu.tokens.sha256,
        CatalogSpeech::Unified160ms.assets().tokens.sha256,
        "the export's tokens are the CPU files' tokens"
    );
}

#[tokio::test]
async fn a_model_with_a_data_file_in_a_folder_of_its_own_is_cached_only_with_its_data_file() {
    let (address, _server) = serve().await;
    let dir = dir("speech-data-file");
    let url = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let mut assets = speech([&url; 4], &sha);
    assets.encoder.file_name = "m.gpu/encoder.onnx";
    assets.encoder_data = Some(Asset {
        file_name: "m.gpu/encoder.onnx.data",
        ..assets.encoder
    });
    let store = ModelStore::in_dir(&dir);
    assert_eq!(assets.size(), 5 * GOOD.len() as u64);

    store
        .fetch_speech(&assets, |_| {})
        .await
        .expect("downloads");
    assert!(store.is_cached(&assets).await);
    assert_eq!(
        fs::read(dir.join("m.gpu/encoder.onnx.data")).expect("data file"),
        GOOD
    );

    fs::remove_file(dir.join("m.gpu/encoder.onnx.data")).expect("remove");
    assert!(!store.is_cached(&assets).await, "the data file is missing");
}

#[tokio::test]
async fn a_speech_download_reports_progress_up_to_done_and_shows_each_step_it_reports() {
    let (address, _server) = serve().await;
    let dir = dir("speech-progress");
    let url = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let assets = speech([&url; 4], &sha);
    let downloads = SpeechDownloads::default();
    let store = ModelStore::in_dir(&dir);

    let mut shown = Vec::new();
    downloads
        .run("m", &store, &assets, |event| {
            shown.push((event, downloads.progress("m")));
        })
        .await
        .expect("downloads");

    let events: Vec<_> = shown.iter().map(|(event, _)| *event).collect();
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
    assert!(shown.iter().all(|(event, seen)| *seen == Some(*event)));
    assert_eq!(
        downloads.progress("m"),
        None,
        "a finished download is not shown"
    );
    assert!(store.is_cached(&assets).await);
    assert_eq!(leftovers(&dir).len(), 4);
    assert_eq!(store.path(&assets.encoder), dir.join("m.encoder.onnx"));
    assert!(store.path(&assets.tokens).is_file());
}

#[tokio::test]
async fn a_file_already_at_its_pinned_size_is_kept_without_reading_it() {
    let sha = sha256(GOOD);
    let unreachable = "http://127.0.0.1:1/none";
    let present = dir("pinned-size-other-bytes");
    fs::create_dir_all(&present).expect("dir");
    let filler = vec![0xAB; GOOD.len()];
    for file in FILES {
        fs::write(present.join(file), &filler).expect("file");
    }
    let assets = speech([unreachable; 4], &sha);
    let mut events = Vec::new();
    ModelStore::in_dir(&present)
        .fetch_speech(&assets, |event| events.push(event))
        .await
        .expect("kept");
    assert_eq!(
        events,
        [SpeechProgress::Verifying, SpeechProgress::Done],
        "a file on disk is not fetched"
    );
    for file in FILES {
        assert_eq!(fs::read(present.join(file)).expect("still there"), filler);
    }

    // A directory can be statted and cannot be read as a file. Its metadata length is the pin.
    let unreadable = dir("pinned-size-unreadable");
    let file = unreadable.join("good.onnx");
    fs::create_dir_all(&file).expect("unreadable");
    let size = fs::metadata(&file).expect("metadata").len();
    let asset = Asset {
        kind: ModelKind::Wake,
        file_name: "good.onnx",
        url: unreachable,
        size,
        sha256: &sha,
    };
    ModelStore::in_dir(&unreadable)
        .fetch(&asset)
        .await
        .expect("kept without reading");
    assert!(fs::metadata(&file).expect("still there").is_dir());
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
    let stalls = format!("http://{address}/stalls.onnx");
    let good = format!("http://{address}/good.onnx");
    let sha = sha256(GOOD);
    let downloads = Arc::new(SpeechDownloads::default());
    let store = ModelStore::in_dir(dir("speech-twice"));
    // Its own folder: models that share a stored file wait for each other's download of it.
    let elsewhere = ModelStore::in_dir(dir("speech-twice-other"));
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
    assert_eq!(downloads.progress("m"), Some(SpeechProgress::Verifying));
    assert_eq!(downloads.progress("other"), None);

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
        .run("other", &elsewhere, &assets, |_| {})
        .await
        .expect("a different model is not held up");

    first.abort();
    assert!(first.await.expect_err("cancelled").is_cancelled());
    assert_eq!(
        downloads.progress("m"),
        None,
        "a cancelled download is not shown"
    );
    downloads
        .run("m", &store, &assets, |_| {})
        .await
        .expect("a finished or cancelled download frees the model");
}

#[tokio::test]
async fn the_whisper_entries_are_listed_with_one_shared_download_that_marks_both_cached() {
    let dir = dir("whisper");
    fs::create_dir_all(&dir).expect("dir");
    let store = ModelStore::in_dir(&dir);
    let listed = |models: &[thalvi_desktop::models::SpeechModelInfo]| {
        models
            .iter()
            .map(|model| (model.name, model.title, model.cached))
            .collect::<Vec<_>>()
    };

    let before = store.whisper_models().await;
    assert_eq!(
        listed(&before),
        [
            (
                "whisper_turbo_cpu",
                "Whisper Turbo \u{b7} CPU (experimental)",
                false
            ),
            (
                "whisper_turbo_vulkan",
                "Whisper Turbo \u{b7} Vulkan GPU (experimental)",
                false
            ),
        ]
    );
    for model in &before {
        assert_eq!(model.size, 574_041_195 + 885_098);
        assert_eq!(model.license, "MIT");
        assert_eq!(model.source, "https://huggingface.co/ggerganov/whisper.cpp");
        assert_eq!(
            ListedSpeech::named(model.name).expect("listed").source(),
            model.source,
            "the model page opened from Settings"
        );
        assert!(
            model.description.contains("Experimental")
                && !model.description.contains("word list is ignored"),
            "{}",
            model.description
        );
    }
    assert!(
        before[0]
            .description
            .contains("only after you stop speaking")
            && before[0].description.contains("slow")
            && before[1].description.contains("while you speak")
            && before[1].description.contains("extra GPU work")
            && before[1].description.contains("untested"),
        "each card says its own trade-off"
    );
    assert_eq!(
        before
            .iter()
            .map(|model| model.word_list)
            .collect::<Vec<_>>(),
        [true, true]
    );

    for asset in [WHISPER_TURBO.model, WHISPER_TURBO.vad] {
        fs::File::create(dir.join(asset.file_name))
            .and_then(|file| file.set_len(asset.size))
            .expect("a file at the pinned size");
    }
    assert!(
        store
            .whisper_models()
            .await
            .iter()
            .all(|model| model.cached)
    );
    let catalog = store
        .speech_models(|_| Device::Cpu(Fallback::CpuModel))
        .await;
    assert!(
        catalog.iter().all(|model| !model.cached),
        "the catalog is untouched"
    );
    assert_eq!(
        catalog
            .iter()
            .map(|model| (model.name, model.word_list))
            .collect::<Vec<_>>(),
        [
            ("nemotron_160ms", true),
            ("unified_160ms", true),
            ("parakeet_tdt_v3", true),
        ]
    );
}

#[test]
fn the_whisper_files_are_pinned_and_a_listed_model_provides_only_the_files_its_entry_loads() {
    for asset in [WHISPER_TURBO.model, WHISPER_TURBO.vad] {
        assert!(
            asset.url.starts_with("https://huggingface.co/"),
            "{}",
            asset.url
        );
        assert!(
            !asset.url.contains("/main/"),
            "pinned to a commit: {}",
            asset.url
        );
        assert_eq!(asset.sha256.len(), 64);
    }
    assert_eq!(
        (WHISPER_TURBO.model.size, WHISPER_TURBO.vad.size),
        (574_041_195, 885_098)
    );
    let cpu = SpeechChoice::Whisper(WhisperBackend::Cpu);
    let vulkan = SpeechChoice::Whisper(WhisperBackend::Vulkan);
    let nemotron = SpeechChoice::Catalog(CatalogSpeech::Nemotron160ms);
    let listed = |name| ListedSpeech::named(name).expect("listed");
    for (case, name, choice, provides) in [
        ("the same Whisper entry", "whisper_turbo_cpu", &cpu, true),
        (
            "the other Whisper entry shares the files",
            "whisper_turbo_cpu",
            &vulkan,
            true,
        ),
        (
            "a catalog model does not load Whisper's files",
            "nemotron_160ms",
            &cpu,
            false,
        ),
        (
            "Whisper does not provide a catalog model",
            "whisper_turbo_vulkan",
            &nemotron,
            false,
        ),
        ("the same catalog model", "nemotron_160ms", &nemotron, true),
        ("another catalog model", "unified_160ms", &nemotron, false),
        (
            "a folder is never downloaded",
            "nemotron_160ms",
            &SpeechChoice::Folder("/m".into()),
            false,
        ),
    ] {
        assert_eq!(listed(name).provides(choice), provides, "{case}");
    }
    assert_eq!(ListedSpeech::named("elsewhere"), None);
}

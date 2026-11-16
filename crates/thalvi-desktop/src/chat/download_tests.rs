//! Installs a tiny fake runtime and model through the real checksum-verified store.

use super::*;
use sha2::{Digest, Sha256};

#[tokio::test]
async fn installation_checks_downloads_unpacks_the_runtime_and_preserves_it_on_model_removal() {
    let dir = std::env::temp_dir().join(format!("thalvi-chat-download-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("fixture")).unwrap();
    std::fs::write(dir.join("fixture/llama-server"), "fake").unwrap();
    let archive = dir.join("runtime.tar");
    assert!(
        Command::new("tar")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.join("fixture"))
            .arg("llama-server")
            .status()
            .unwrap()
            .success()
    );
    let bytes = std::fs::read(&archive).unwrap();
    let size = u64::try_from(bytes.len()).unwrap();
    let checksum = crate::models::hex(&Sha256::digest(&bytes));
    let router = axum::Router::new()
        .route(
            "/runtime",
            axum::routing::get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        )
        .route("/model", axum::routing::get(|| async { "gguf" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async {
        axum::serve(listener, router).await.unwrap();
    });
    let runtime_url = format!("{base}/runtime");
    let model_url = format!("{base}/model");
    let model_sha = crate::models::hex(&Sha256::digest(b"gguf"));
    let files = Files {
        runtime: Asset {
            kind: ModelKind::ChatRuntime,
            file_name: "runtimes/test.tar",
            url: &runtime_url,
            size,
            sha256: &checksum,
        },
        folder: "runtimes/test",
        model: Asset {
            kind: ModelKind::ChatModel,
            file_name: "model.gguf",
            url: &model_url,
            size: 4,
            sha256: &model_sha,
        },
    };
    let store = ModelStore::in_dir(dir.join("store"));
    let mut progress = Vec::new();
    install(&store, &files, |p| progress.push(p)).await.unwrap();
    assert!(files.server_path(&store).is_some());
    assert_eq!(std::fs::read(files.model_path(&store)).unwrap(), b"gguf");
    assert_eq!(progress.first(), Some(&SpeechProgress::Verifying));
    assert_eq!(progress.last(), Some(&SpeechProgress::Done));
    remove_model(&store, &files).await.unwrap();
    assert!(files.server_path(&store).is_some());
    assert!(!files.model_path(&store).exists());
    let damaged = Files {
        model: Asset {
            sha256: "wrong",
            ..files.model
        },
        ..files
    };
    assert!(matches!(
        install(&store, &damaged, |_| {}).await,
        Err(ModelError::Checksum { .. })
    ));
    assert!(
        !files.model_path(&store).exists(),
        "a failed checksum never installs the model"
    );
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

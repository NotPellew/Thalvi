//! Model catalogs (wake heads, speech recognition) and their checksum-verified download into the per-user data directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The most a download may take besides moving its bytes.
const DOWNLOAD_BASE: Duration = Duration::from_secs(120);

/// The slowest connection a download is given time for, so a large model is not cut off.
const SLOWEST_BYTES_PER_SECOND: u64 = 500_000;

/// Makes each download's temporary file name unique, so concurrent fetches do not share one.
static DOWNLOADS: AtomicU64 = AtomicU64::new(0);

/// What a model file is for; error messages name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Wake,
    Speech,
}

impl ModelKind {
    fn noun(self) -> &'static str {
        match self {
            Self::Wake => "wake model",
            Self::Speech => "speech model",
        }
    }
}

/// A downloadable model file pinned by size and checksum.
#[derive(Debug, Clone, Copy)]
pub struct Asset<'a> {
    pub kind: ModelKind,
    pub file_name: &'a str,
    pub url: &'a str,
    pub size: u64,
    pub sha256: &'a str,
}

/// Feature stage 1: audio to log-mel spectrogram (openWakeWord v0.5.1).
pub const MELSPECTROGRAM: Asset<'static> = Asset {
    kind: ModelKind::Wake,
    file_name: "melspectrogram.onnx",
    url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/melspectrogram.onnx",
    size: 1_087_958,
    sha256: "ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f",
};

/// Feature stage 2: spectrogram windows to 96-value embeddings (openWakeWord v0.5.1).
pub const EMBEDDING: Asset<'static> = Asset {
    kind: ModelKind::Wake,
    file_name: "embedding_model.onnx",
    url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/embedding_model.onnx",
    size: 1_326_578,
    sha256: "70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f",
};

/// A wake head that ships in the catalog and is chosen by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogHead {
    HeyThalvi,
    HeyMycroft,
    HeyJarvis,
}

impl CatalogHead {
    pub const ALL: [Self; 3] = [Self::HeyThalvi, Self::HeyMycroft, Self::HeyJarvis];

    /// The name used in config.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::HeyThalvi => "hey_thalvi",
            Self::HeyMycroft => "hey_mycroft",
            Self::HeyJarvis => "hey_jarvis",
        }
    }

    /// The download that provides this head.
    #[must_use]
    pub fn asset(self) -> Asset<'static> {
        match self {
            Self::HeyThalvi => Asset {
                kind: ModelKind::Wake,
                file_name: "hey_thalvi_v0.0.13.onnx",
                url: "https://github.com/NotPellew/Thalvi/releases/download/wake-models-v1/hey_thalvi_v0.0.13.onnx",
                size: 395_095,
                sha256: "f0491728ee36bb0b7f8860ad35a290b3b5b518cf8d2a378179b31ef9494b37e3",
            },
            Self::HeyMycroft => Asset {
                kind: ModelKind::Wake,
                file_name: "hey_mycroft_v0.1.onnx",
                url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/hey_mycroft_v0.1.onnx",
                size: 857_691,
                sha256: "c2a311e8fa1338de89c31b3b46dc4dffd4af2f9a8d6ddead48893c2d301b1f18",
            },
            Self::HeyJarvis => Asset {
                kind: ModelKind::Wake,
                file_name: "hey_jarvis_v0.1.onnx",
                url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/hey_jarvis_v0.1.onnx",
                size: 1_271_370,
                sha256: "94a13cfe60075b132f6a472e7e462e8123ee70861bc3fb58434a73712ee0d2cb",
            },
        }
    }
}

/// A speech model that ships in the catalog and is chosen by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSpeech {
    Nemotron160ms,
}

/// A catalog model's files, which a model folder must also contain under these names.
#[derive(Debug, Clone, Copy)]
pub struct SpeechAssets<'a> {
    pub encoder: Asset<'a>,
    pub decoder: Asset<'a>,
    pub joiner: Asset<'a>,
    pub tokens: Asset<'a>,
}

impl CatalogSpeech {
    pub const ALL: [Self; 1] = [Self::Nemotron160ms];

    /// The name used in config.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Nemotron160ms => "nemotron_160ms",
        }
    }

    /// The downloads that provide this model: NVIDIA's Nemotron Speech Streaming 0.6B (NVIDIA
    /// Open Model License) as exported to int8 ONNX for sherpa-onnx, pinned by repository commit.
    #[must_use]
    pub fn assets(self) -> SpeechAssets<'static> {
        match self {
            Self::Nemotron160ms => SpeechAssets {
                encoder: Asset {
                    kind: ModelKind::Speech,
                    file_name: "nemotron_160ms.encoder.int8.onnx",
                    url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25/resolve/237e551abd7a411ef92d3595454d9f6ab5fe7d6c/encoder.int8.onnx",
                    size: 652_916_849,
                    sha256: "71111f61b18e1e65e01e369434a5c0434868d2f44892742ae54240600c681209",
                },
                decoder: Asset {
                    kind: ModelKind::Speech,
                    file_name: "nemotron_160ms.decoder.int8.onnx",
                    url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25/resolve/237e551abd7a411ef92d3595454d9f6ab5fe7d6c/decoder.int8.onnx",
                    size: 7_257_753,
                    sha256: "0be9702c2f427a2b6bb241d298e0d3836a558de1f5b9fd3018f1cce6e2b3fa98",
                },
                joiner: Asset {
                    kind: ModelKind::Speech,
                    file_name: "nemotron_160ms.joiner.int8.onnx",
                    url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25/resolve/237e551abd7a411ef92d3595454d9f6ab5fe7d6c/joiner.int8.onnx",
                    size: 1_735_862,
                    sha256: "a35eac38a22ebceb04d230ed7afe0d68f446ba6914a036b97f14fece95967e23",
                },
                tokens: Asset {
                    kind: ModelKind::Speech,
                    file_name: "nemotron_160ms.tokens.txt",
                    url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25/resolve/237e551abd7a411ef92d3595454d9f6ab5fe7d6c/tokens.txt",
                    size: 8_952,
                    sha256: "dc0b4584ab2e4ddbf888425c076c61b736e7356a015250db7d307e6f1a8188ff",
                },
            },
        }
    }
}

/// Why a model could not be provided. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("Thalvi could not find a folder to keep its models in.")]
    NoDataDir,
    #[error(
        "Could not download the {} {file}. Check the internet connection and try again.",
        kind.noun()
    )]
    Download {
        kind: ModelKind,
        file: String,
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "The downloaded {} {file} did not match its expected checksum, so it was thrown away. Try again, and report it if it keeps happening.",
        kind.noun()
    )]
    Checksum { kind: ModelKind, file: String },
    #[error("The stored {} {file} was damaged, so it was removed. {cause}", kind.noun())]
    Replaced {
        kind: ModelKind,
        file: String,
        /// Why fetching it again failed.
        #[source]
        cause: Box<ModelError>,
    },
    #[error("Could not save the {} {file} in {}: {source}", kind.noun(), dir.display())]
    Io {
        kind: ModelKind,
        file: String,
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A directory of verified model files that fills itself by downloading on first use.
#[derive(Debug, Clone)]
pub struct ModelStore {
    dir: PathBuf,
}

impl ModelStore {
    /// The per-user data directory (`~/.local/share/thalvi/models`, `%LOCALAPPDATA%\thalvi\models`).
    ///
    /// # Errors
    /// Fails when the platform has no per-user data directory.
    pub fn per_user() -> Result<Self, ModelError> {
        let dir = dirs::data_local_dir().ok_or(ModelError::NoDataDir)?;
        Ok(Self::in_dir(dir.join("thalvi").join("models")))
    }

    #[must_use]
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Returns the path of the verified file, downloading it first if it is missing. A stored file
    /// that does not match its checksum is deleted before anything else.
    ///
    /// # Errors
    /// Fails when the download fails, the checksum does not match (the file is deleted), or the
    /// directory cannot be written. When a damaged stored file was removed first, the error says so.
    pub async fn fetch(&self, asset: &Asset<'_>) -> Result<PathBuf, ModelError> {
        let path = self.dir.join(asset.file_name);
        let damaged = match stored(&path, asset).await {
            Stored::Intact => return Ok(path),
            Stored::Missing => false,
            Stored::Damaged => {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|source| ModelError::Io {
                        kind: asset.kind,
                        file: asset.file_name.to_owned(),
                        dir: self.dir.clone(),
                        source,
                    })?;
                true
            }
        };
        match self.download(asset, &path).await {
            Ok(()) => Ok(path),
            Err(cause) if damaged => Err(ModelError::Replaced {
                kind: asset.kind,
                file: asset.file_name.to_owned(),
                cause: Box::new(cause),
            }),
            Err(error) => Err(error),
        }
    }

    async fn download(&self, asset: &Asset<'_>, path: &Path) -> Result<(), ModelError> {
        let io = |source| ModelError::Io {
            kind: asset.kind,
            file: asset.file_name.to_owned(),
            dir: self.dir.clone(),
            source,
        };
        let net = |source| ModelError::Download {
            kind: asset.kind,
            file: asset.file_name.to_owned(),
            source,
        };
        tokio::fs::create_dir_all(&self.dir).await.map_err(io)?;
        let part = Partial(path.with_extension(format!(
            "{}-{}.part",
            std::process::id(),
            DOWNLOADS.fetch_add(1, Ordering::Relaxed)
        )));
        let mut response = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(DOWNLOAD_BASE + Duration::from_secs(asset.size / SLOWEST_BYTES_PER_SECOND))
            .build()
            .map_err(net)?
            .get(asset.url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(net)?;
        let mut file = tokio::fs::File::create(&part.0).await.map_err(io)?;
        let mut hasher = Sha256::new();
        let received = async {
            while let Some(chunk) = response.chunk().await.map_err(net)? {
                hasher.update(&chunk);
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                    .await
                    .map_err(io)?;
            }
            tokio::io::AsyncWriteExt::flush(&mut file).await.map_err(io)
        }
        .await;
        drop(file);
        received?;
        if hex(&hasher.finalize()) != asset.sha256 {
            return Err(ModelError::Checksum {
                kind: asset.kind,
                file: asset.file_name.to_owned(),
            });
        }
        // Another fetch of this asset may have finished first, and Windows will not replace a
        // file that is open: then the file that is there, if intact, is the one to use.
        let intact = || async { matches!(stored(path, asset).await, Stored::Intact) };
        if intact().await {
            return Ok(());
        }
        match tokio::fs::rename(&part.0, path).await {
            Err(source) if !intact().await => Err(io(source)),
            _ => Ok(()),
        }
    }
}

/// A download in progress, deleted when it is dropped: finished, failed or cancelled.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// What is on disk under a pinned name.
enum Stored {
    Missing,
    Intact,
    Damaged,
}

/// Checks the file on disk against the pin; streamed, because speech models are large.
async fn stored(path: &Path, asset: &Asset<'_>) -> Stored {
    let (path, sha256) = (path.to_owned(), asset.sha256.to_owned());
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Stored::Missing,
            Err(_) => return Stored::Damaged,
        };
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => {
                    return if hex(&hasher.finalize()) == sha256 {
                        Stored::Intact
                    } else {
                        Stored::Damaged
                    };
                }
                Ok(read) => hasher.update(&buffer[..read]),
                Err(_) => return Stored::Damaged,
            }
        }
    })
    .await
    .unwrap_or(Stored::Damaged)
}

fn hex(digest: &[u8]) -> String {
    use std::fmt::Write;
    digest.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

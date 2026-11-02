//! Wake model catalog and its checksum-verified download into the per-user data directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Makes each download's temporary file name unique, so concurrent fetches do not share one.
static DOWNLOADS: AtomicU64 = AtomicU64::new(0);

/// A downloadable model file pinned by checksum.
#[derive(Debug, Clone, Copy)]
pub struct Asset<'a> {
    pub file_name: &'a str,
    pub url: &'a str,
    pub sha256: &'a str,
}

/// Feature stage 1: audio to log-mel spectrogram (openWakeWord v0.5.1).
pub const MELSPECTROGRAM: Asset<'static> = Asset {
    file_name: "melspectrogram.onnx",
    url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/melspectrogram.onnx",
    sha256: "ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f",
};

/// Feature stage 2: spectrogram windows to 96-value embeddings (openWakeWord v0.5.1).
pub const EMBEDDING: Asset<'static> = Asset {
    file_name: "embedding_model.onnx",
    url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/embedding_model.onnx",
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
                file_name: "hey_thalvi_v0.0.13.onnx",
                url: "https://github.com/NotPellew/Thalvi/releases/download/wake-models-v1/hey_thalvi_v0.0.13.onnx",
                sha256: "f0491728ee36bb0b7f8860ad35a290b3b5b518cf8d2a378179b31ef9494b37e3",
            },
            Self::HeyMycroft => Asset {
                file_name: "hey_mycroft_v0.1.onnx",
                url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/hey_mycroft_v0.1.onnx",
                sha256: "c2a311e8fa1338de89c31b3b46dc4dffd4af2f9a8d6ddead48893c2d301b1f18",
            },
            Self::HeyJarvis => Asset {
                file_name: "hey_jarvis_v0.1.onnx",
                url: "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1/hey_jarvis_v0.1.onnx",
                sha256: "94a13cfe60075b132f6a472e7e462e8123ee70861bc3fb58434a73712ee0d2cb",
            },
        }
    }
}

/// Why a model could not be provided. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("Thalvi could not find a folder to keep its wake models in.")]
    NoDataDir,
    #[error(
        "Could not download the wake model {file}. Check the internet connection and try again."
    )]
    Download {
        file: String,
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "The downloaded wake model {file} did not match its expected checksum, so it was thrown away. Try again, and report it if it keeps happening."
    )]
    Checksum { file: String },
    #[error("Could not save the wake model {file} in {}: {source}", dir.display())]
    Io {
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

    /// Returns the path of the verified file, downloading it first if it is missing or damaged.
    ///
    /// # Errors
    /// Fails when the download fails, the checksum does not match (the file is deleted), or the
    /// directory cannot be written.
    pub async fn fetch(&self, asset: &Asset<'_>) -> Result<PathBuf, ModelError> {
        let path = self.dir.join(asset.file_name);
        if let Ok(bytes) = tokio::fs::read(&path).await
            && hex(&Sha256::digest(&bytes)) == asset.sha256
        {
            return Ok(path);
        }
        self.download(asset, &path).await?;
        Ok(path)
    }

    async fn download(&self, asset: &Asset<'_>, path: &Path) -> Result<(), ModelError> {
        let io = |source| ModelError::Io {
            file: asset.file_name.to_owned(),
            dir: self.dir.clone(),
            source,
        };
        let net = |source| ModelError::Download {
            file: asset.file_name.to_owned(),
            source,
        };
        tokio::fs::create_dir_all(&self.dir).await.map_err(io)?;
        let part = path.with_extension(format!(
            "{}-{}.part",
            std::process::id(),
            DOWNLOADS.fetch_add(1, Ordering::Relaxed)
        ));
        let mut response = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(net)?
            .get(asset.url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(net)?;
        let mut file = tokio::fs::File::create(&part).await.map_err(io)?;
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
        if let Err(error) = received {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(error);
        }
        if hex(&hasher.finalize()) != asset.sha256 {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(ModelError::Checksum {
                file: asset.file_name.to_owned(),
            });
        }
        tokio::fs::rename(&part, path).await.map_err(io)
    }
}

fn hex(digest: &[u8]) -> String {
    use std::fmt::Write;
    digest.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

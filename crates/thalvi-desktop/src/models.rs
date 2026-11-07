//! Model catalogs (wake heads, speech recognition) and their checksum-verified download into the per-user data directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::report::UserError;

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
    Spotter,
}

impl ModelKind {
    fn noun(self) -> &'static str {
        match self {
            Self::Wake => "wake model",
            Self::Speech => "speech model",
            Self::Spotter => "ending phrase model",
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

/// The files of the ending phrase spotter: sherpa-onnx's streaming Zipformer keyword model trained
/// on `GigaSpeech` (Apache-2.0), 2024-01-01. Each file is byte-identical to its member of the
/// official `sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2` and is fetched from the
/// model author's `ModelScope` repository, which holds them apart from test audio.
#[derive(Debug, Clone, Copy)]
pub struct SpotterAssets<'a> {
    pub encoder: Asset<'a>,
    pub decoder: Asset<'a>,
    pub joiner: Asset<'a>,
    /// The `SentencePiece` model whose piece numbers are the model's token numbers.
    pub pieces: Asset<'a>,
}

pub const SPOTTER: SpotterAssets<'static> = SpotterAssets {
    encoder: Asset {
        kind: ModelKind::Spotter,
        file_name: "kws_gigaspeech_3m.encoder.int8.onnx",
        url: "https://www.modelscope.cn/models/pkufool/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01/resolve/master/encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        size: 4_807_159,
        sha256: "1e721676515bcd42a186979733981213c66c80db680e1cc582dfedf3be76e678",
    },
    decoder: Asset {
        kind: ModelKind::Spotter,
        file_name: "kws_gigaspeech_3m.decoder.onnx",
        url: "https://www.modelscope.cn/models/pkufool/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01/resolve/master/decoder-epoch-12-avg-2-chunk-16-left-64.onnx",
        size: 1_063_189,
        sha256: "f61ebd3eed3773a44d088d53dfae92dbb6aec4839f4dcaee2d402414741663a3",
    },
    joiner: Asset {
        kind: ModelKind::Spotter,
        file_name: "kws_gigaspeech_3m.joiner.int8.onnx",
        url: "https://www.modelscope.cn/models/pkufool/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01/resolve/master/joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx",
        size: 163_380,
        sha256: "eae9da0c7e1e6c6a3f4cc42d167899c388f6c6701b94cb96320e4f55df79624c",
    },
    pieces: Asset {
        kind: ModelKind::Spotter,
        file_name: "kws_gigaspeech_3m.bpe.model",
        url: "https://www.modelscope.cn/models/pkufool/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01/resolve/master/bpe.model",
        size: 244_837,
        sha256: "c8a2a0129c4ab8e463164c142f82d25649661b122c8cd0b7aab5c9e80b90ad24",
    },
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

    /// The phrase as shown to the user.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::HeyThalvi => "Hey Thalvi",
            Self::HeyMycroft => "Hey Mycroft",
            Self::HeyJarvis => "Hey Jarvis",
        }
    }

    /// The catalog head called `name` in config.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|head| head.name() == name)
    }

    /// The download that provides this head.
    #[must_use]
    pub fn asset(self) -> Asset<'static> {
        match self {
            Self::HeyThalvi => Asset {
                kind: ModelKind::Wake,
                file_name: "hey_thalvi_v0.0.14.onnx",
                url: "https://github.com/NotPellew/Thalvi/releases/download/wake-models-v2/hey_thalvi_v0.0.14.onnx",
                size: 395_406,
                sha256: "2462290e29875bc9458c866387ddf7a6f75168849aae9f45cfc587d2f07ad29e",
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

/// What the settings window shows of a catalog wake head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeModelInfo {
    pub name: &'static str,
    pub title: &'static str,
    /// Bytes to download.
    pub size: u64,
    /// The file is on disk at its pinned size.
    pub cached: bool,
}

/// A speech model that ships in the catalog and is chosen by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSpeech {
    Nemotron160ms,
    Unified160ms,
}

/// A catalog model's files, which a model folder must also contain under these names.
#[derive(Debug, Clone, Copy)]
pub struct SpeechAssets<'a> {
    pub encoder: Asset<'a>,
    pub decoder: Asset<'a>,
    pub joiner: Asset<'a>,
    pub tokens: Asset<'a>,
}

impl<'a> SpeechAssets<'a> {
    fn all(&self) -> [Asset<'a>; 4] {
        [self.encoder, self.decoder, self.joiner, self.tokens]
    }

    /// The bytes of all files together.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.all().iter().map(|asset| asset.size).sum()
    }
}

/// What the settings window shows of a catalog speech model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpeechModelInfo {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Bytes to download in all.
    pub size: u64,
    /// The model's page, for reading about it.
    pub source: &'static str,
    pub license: &'static str,
    /// The languages it recognizes.
    pub languages: &'static [&'static str],
    /// Every file is on disk and matches its checksum.
    pub cached: bool,
}

/// One file of a catalog speech model, stored under `<model>.<file>`.
macro_rules! speech_file {
    ($model:literal, $base:literal, $file:literal, $size:expr, $sha256:literal) => {
        Asset {
            kind: ModelKind::Speech,
            file_name: concat!($model, ".", $file),
            url: concat!($base, $file),
            size: $size,
            sha256: $sha256,
        }
    };
}

/// The four files of a catalog speech model, each with its size and checksum.
macro_rules! speech_assets {
    ($model:literal, $base:literal, $encoder:tt, $decoder:tt, $joiner:tt, $tokens:tt) => {
        SpeechAssets {
            encoder: speech_assets!(@file $model, $base, "encoder.int8.onnx", $encoder),
            decoder: speech_assets!(@file $model, $base, "decoder.int8.onnx", $decoder),
            joiner: speech_assets!(@file $model, $base, "joiner.int8.onnx", $joiner),
            tokens: speech_assets!(@file $model, $base, "tokens.txt", $tokens),
        }
    };
    (@file $model:literal, $base:literal, $file:literal, ($size:expr, $sha256:literal)) => {
        speech_file!($model, $base, $file, $size, $sha256)
    };
}

/// The languages on NVIDIA's card for the unified model.
const UNIFIED_LANGUAGES: [&str; 25] = [
    "Bulgarian",
    "Croatian",
    "Czech",
    "Danish",
    "Dutch",
    "English",
    "Estonian",
    "Finnish",
    "French",
    "German",
    "Greek",
    "Hungarian",
    "Italian",
    "Latvian",
    "Lithuanian",
    "Maltese",
    "Polish",
    "Portuguese",
    "Romanian",
    "Russian",
    "Slovak",
    "Slovenian",
    "Spanish",
    "Swedish",
    "Ukrainian",
];

impl CatalogSpeech {
    pub const ALL: [Self; 2] = [Self::Nemotron160ms, Self::Unified160ms];

    /// The catalog model called `name` in config.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|model| model.name() == name)
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Nemotron160ms => "Nemotron Streaming",
            Self::Unified160ms => "Nemotron Unified",
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Nemotron160ms => {
                "From NVIDIA: the lightest on the CPU, and text appears soonest. Mostly without punctuation. The default."
            }
            Self::Unified160ms => {
                "From NVIDIA: fewest word errors in our tests, with punctuation and capitals. It reads the last six seconds again every 160 ms, so it needs a fast CPU and text appears about half a second later. Exported by Thalvi."
            }
        }
    }

    /// The model's license.
    #[must_use]
    pub fn license(self) -> &'static str {
        match self {
            Self::Nemotron160ms => "NVIDIA Open Model License",
            Self::Unified160ms => "OpenMDW-1.1",
        }
    }

    /// The languages it recognizes.
    #[must_use]
    pub fn languages(self) -> &'static [&'static str] {
        match self {
            Self::Nemotron160ms => &["English"],
            Self::Unified160ms => &UNIFIED_LANGUAGES,
        }
    }

    /// The page of the model the files come from.
    #[must_use]
    pub fn source(self) -> &'static str {
        match self {
            Self::Nemotron160ms => {
                "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25"
            }
            Self::Unified160ms => "https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b",
        }
    }

    /// The name used in config.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Nemotron160ms => "nemotron_160ms",
            Self::Unified160ms => "unified_160ms",
        }
    }

    /// The downloads that provide this model, each pinned by size and checksum. The Nemotron
    /// Speech Streaming ones (NVIDIA Open Model License) are sherpa-onnx's int8 exports, pinned by
    /// repository commit; Nemotron Unified ASR (OpenMDW-1.1) is Thalvi's own export, which is not
    /// published anywhere else.
    #[must_use]
    pub fn assets(self) -> SpeechAssets<'static> {
        match self {
            Self::Nemotron160ms => speech_assets!(
                "nemotron_160ms",
                "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25/resolve/237e551abd7a411ef92d3595454d9f6ab5fe7d6c/",
                (
                    652_916_849,
                    "71111f61b18e1e65e01e369434a5c0434868d2f44892742ae54240600c681209"
                ),
                (
                    7_257_753,
                    "0be9702c2f427a2b6bb241d298e0d3836a558de1f5b9fd3018f1cce6e2b3fa98"
                ),
                (
                    1_735_862,
                    "a35eac38a22ebceb04d230ed7afe0d68f446ba6914a036b97f14fece95967e23"
                ),
                (
                    8_952,
                    "dc0b4584ab2e4ddbf888425c076c61b736e7356a015250db7d307e6f1a8188ff"
                )
            ),
            Self::Unified160ms => speech_assets!(
                "unified_160ms",
                "https://github.com/NotPellew/Thalvi/releases/download/speech-models-v1/",
                (
                    633_538_635,
                    "d7a9da717484351960be90cb26190aadb7de9b4fbfc8cf614eed2dc6381002e6"
                ),
                (
                    9_223_194,
                    "18753da5ca260c9252106d9bb9574ff7d14273d7b064daf74ab90fd6db785b32"
                ),
                (
                    3_713_592,
                    "bd9c00dc2d16fab355b4bfc49e0462138771e883dddf9b3bb515684c8fafacfc"
                ),
                (
                    42_928,
                    "94139a0b2134fd40664ca72260026fa8a073336bb2f062239c8ecde2c8031763"
                )
            ),
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
    #[error("The speech model {name} is already being downloaded.")]
    AlreadyDownloading { name: String },
    #[error("Could not save the {} {file} in {}: {source}", kind.noun(), dir.display())]
    Io {
        kind: ModelKind,
        file: String,
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl UserError for ModelError {
    fn headline(&self) -> &'static str {
        match self {
            Self::NoDataDir => "No folder to keep models in",
            Self::Download { .. } => "Download failed — check internet",
            Self::Checksum { .. } => "Downloaded model was damaged",
            Self::Replaced { .. } => "Stored model was damaged",
            Self::AlreadyDownloading { .. } => "Model is already downloading",
            Self::Io { .. } => "Can't save the model file",
        }
    }
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
        self.fetch_reporting(asset, |_| {}).await
    }

    /// Like [`fetch`](Self::fetch); `received` is told how many bytes of the file have arrived.
    ///
    /// # Errors
    /// The same as [`fetch`](Self::fetch).
    pub async fn fetch_reporting(
        &self,
        asset: &Asset<'_>,
        received: impl FnMut(u64),
    ) -> Result<PathBuf, ModelError> {
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
        match self.download(asset, &path, received).await {
            Ok(()) => Ok(path),
            Err(cause) if damaged => Err(ModelError::Replaced {
                kind: asset.kind,
                file: asset.file_name.to_owned(),
                cause: Box::new(cause),
            }),
            Err(error) => Err(error),
        }
    }

    async fn download(
        &self,
        asset: &Asset<'_>,
        path: &Path,
        mut received: impl FnMut(u64),
    ) -> Result<(), ModelError> {
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
        let mut total = 0;
        let written = async {
            while let Some(chunk) = response.chunk().await.map_err(net)? {
                hasher.update(&chunk);
                total += chunk.len() as u64;
                received(total);
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                    .await
                    .map_err(io)?;
            }
            tokio::io::AsyncWriteExt::flush(&mut file).await.map_err(io)
        }
        .await;
        drop(file);
        written?;
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

    /// Whether every file of `assets` is on disk at its pinned size. Cheap, for showing download
    /// state; [`fetch`](Self::fetch) still verifies each checksum before a model is used.
    pub async fn is_cached(&self, assets: &SpeechAssets<'_>) -> bool {
        for asset in assets.all() {
            match tokio::fs::metadata(self.dir.join(asset.file_name)).await {
                Ok(meta) if meta.len() == asset.size => {}
                _ => return false,
            }
        }
        true
    }

    /// Whether one pinned file is on disk at its pinned size. Cheap, for showing download
    /// state; [`fetch`](Self::fetch) still verifies its checksum before it is used.
    pub async fn is_asset_cached(&self, asset: &Asset<'_>) -> bool {
        match tokio::fs::metadata(self.dir.join(asset.file_name)).await {
            Ok(meta) => meta.len() == asset.size,
            Err(_) => false,
        }
    }

    /// The catalog wake heads with whether each is already downloaded.
    pub async fn wake_models(&self) -> Vec<WakeModelInfo> {
        let mut models = Vec::new();
        for head in CatalogHead::ALL {
            let asset = head.asset();
            models.push(WakeModelInfo {
                name: head.name(),
                title: head.title(),
                size: asset.size,
                cached: self.is_asset_cached(&asset).await,
            });
        }
        models
    }

    /// The catalog models with whether each is already downloaded.
    pub async fn speech_models(&self) -> Vec<SpeechModelInfo> {
        let mut models = Vec::new();
        for model in CatalogSpeech::ALL {
            let assets = model.assets();
            models.push(SpeechModelInfo {
                name: model.name(),
                title: model.title(),
                description: model.description(),
                size: assets.size(),
                source: model.source(),
                license: model.license(),
                languages: model.languages(),
                cached: self.is_cached(&assets).await,
            });
        }
        models
    }

    /// Fetches the files of `assets` that are not intact yet, telling `progress` how it goes.
    ///
    /// # Errors
    /// The first file's error; files fetched before it stay.
    pub async fn fetch_speech(
        &self,
        assets: &SpeechAssets<'_>,
        mut progress: impl FnMut(SpeechProgress),
    ) -> Result<(), ModelError> {
        progress(SpeechProgress::Verifying);
        let total = assets.size();
        let mut done = 0;
        let mut shown = None;
        let mut report = |bytes: u64| {
            let percent = u8::try_from((bytes * 100).checked_div(total).unwrap_or(100).min(100))
                .unwrap_or(100);
            if shown != Some(percent) {
                shown = Some(percent);
                progress(SpeechProgress::Downloading { percent });
            }
        };
        for asset in assets.all() {
            let path = self.dir.join(asset.file_name);
            if !matches!(stored(&path, &asset).await, Stored::Intact) {
                self.fetch_reporting(&asset, |bytes| report(done + bytes))
                    .await?;
            }
            done += asset.size;
            report(done);
        }
        progress(SpeechProgress::Done);
        Ok(())
    }
}

/// How a speech model download is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechProgress {
    /// Checking which files are already intact.
    Verifying,
    Downloading {
        percent: u8,
    },
    Done,
}

/// The speech models being downloaded now, so a model is never fetched twice at once.
#[derive(Debug, Default)]
pub struct SpeechDownloads {
    running: Mutex<HashSet<String>>,
}

impl SpeechDownloads {
    /// Runs [`ModelStore::fetch_speech`] for the model called `name` unless it is already running.
    ///
    /// # Errors
    /// [`ModelError::AlreadyDownloading`], or the download's own error.
    pub async fn run(
        &self,
        name: &str,
        store: &ModelStore,
        assets: &SpeechAssets<'_>,
        progress: impl FnMut(SpeechProgress),
    ) -> Result<(), ModelError> {
        if !self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned())
        {
            return Err(ModelError::AlreadyDownloading {
                name: name.to_owned(),
            });
        }
        let _release = Release {
            running: &self.running,
            name,
        };
        store.fetch_speech(assets, progress).await
    }
}

struct Release<'a> {
    running: &'a Mutex<HashSet<String>>,
    name: &'a str,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(self.name);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wake_models_report_each_catalog_head_and_whether_it_is_cached() {
        let dir = std::env::temp_dir().join(format!("thalvi-wake-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = ModelStore::in_dir(dir.clone());
        let asset = CatalogHead::HeyThalvi.asset();
        std::fs::write(
            dir.join(asset.file_name),
            vec![0; usize::try_from(asset.size).expect("size fits")],
        )
        .expect("cached head at pinned size");
        let models = store.wake_models().await;
        let cached: Vec<(&str, &str, bool)> = models
            .iter()
            .map(|model| (model.name, model.title, model.cached))
            .collect();
        assert_eq!(
            cached,
            [
                ("hey_thalvi", "Hey Thalvi", true),
                ("hey_mycroft", "Hey Mycroft", false),
                ("hey_jarvis", "Hey Jarvis", false),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

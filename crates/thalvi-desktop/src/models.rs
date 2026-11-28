//! Model catalogs (wake heads, speech recognition) and their checksum-verified download into the per-user data directory.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::device::{Device, Export};
use crate::report::{CertificateProblem, UserError};
use crate::transducer::SpeechChoice;
use crate::whisper::WhisperBackend;

/// The most a download may take besides moving its bytes.
const DOWNLOAD_BASE: Duration = Duration::from_secs(120);

/// The slowest connection a download is given time for, so a large model is not cut off.
const SLOWEST_BYTES_PER_SECOND: u64 = 500_000;

/// Makes each download's temporary file name unique, so concurrent fetches do not share one.
static DOWNLOADS: AtomicU64 = AtomicU64::new(0);

/// One download per stored path: the app's listener and its Settings window can ask for a file at once.
static FETCHING: LazyLock<Mutex<HashMap<PathBuf, Arc<InFlight>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
enum AttemptOutcome {
    Finished(Result<(), ModelError>),
    Cancelled,
}

struct InFlight {
    progress: tokio::sync::watch::Sender<u64>,
    outcome: tokio::sync::watch::Sender<Option<AttemptOutcome>>,
}

impl InFlight {
    fn new() -> Self {
        let (progress, _) = tokio::sync::watch::channel(0);
        let (outcome, _) = tokio::sync::watch::channel(None);
        Self { progress, outcome }
    }
}

enum FetchRole {
    Leader(Arc<InFlight>),
    Follower(Arc<InFlight>),
}

fn join_fetch(path: &Path) -> FetchRole {
    let mut files = FETCHING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = files.get(path) {
        return FetchRole::Follower(Arc::clone(existing));
    }
    let active = Arc::new(InFlight::new());
    files.insert(path.to_path_buf(), Arc::clone(&active));
    FetchRole::Leader(active)
}

/// Removes an in-flight entry and wakes followers when a leader finishes or is cancelled.
struct ActiveFetch {
    path: PathBuf,
    in_flight: Arc<InFlight>,
    finished: bool,
}

impl ActiveFetch {
    fn remove(&self) {
        let mut files = FETCHING.lock().unwrap_or_else(PoisonError::into_inner);
        if files
            .get(&self.path)
            .is_some_and(|current| Arc::ptr_eq(current, &self.in_flight))
        {
            files.remove(&self.path);
        }
    }

    fn finish(mut self, result: Result<(), ModelError>) -> Result<(), ModelError> {
        self.finished = true;
        self.remove();
        self.in_flight
            .outcome
            .send_replace(Some(AttemptOutcome::Finished(result.clone())));
        result
    }
}

impl Drop for ActiveFetch {
    fn drop(&mut self) {
        if !self.finished {
            self.remove();
            self.in_flight
                .outcome
                .send_replace(Some(AttemptOutcome::Cancelled));
        }
    }
}

/// What a model file is for; error messages name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Wake,
    Speech,
    ChatModel,
    ChatRuntime,
}

impl ModelKind {
    fn noun(self) -> &'static str {
        match self {
            Self::Wake => "wake model",
            Self::Speech => "speech model",
            Self::ChatModel => "chat model",
            Self::ChatRuntime => "chat runtime",
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

    /// The phrase as shown to the user.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::HeyThalvi => "Hey Thalvi",
            Self::HeyMycroft => "Hey Mycroft",
            Self::HeyJarvis => "Hey Jarvis",
        }
    }

    /// The wake threshold used when config gives none.
    #[must_use]
    pub fn default_threshold(self) -> f32 {
        match self {
            // The false-wake and recall numbers for v0.0.14 were measured at 0.7.
            Self::HeyThalvi => 0.7,
            Self::HeyMycroft | Self::HeyJarvis => 0.5,
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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WakeModelInfo {
    pub name: &'static str,
    pub title: &'static str,
    /// Bytes to download.
    pub size: u64,
    /// The file is on disk at its pinned size.
    pub cached: bool,
    /// The threshold the head starts at.
    pub threshold: f32,
}

/// The files of one downloadable speech model.
pub trait ModelFiles {
    fn files(&self) -> Vec<Asset<'_>>;
}

/// The two files that both Whisper Turbo entries load, MIT-licensed: `OpenAI`'s Whisper large-v3-turbo
/// quantized to `q5_0` by whisper.cpp, and the Silero voice activity detector in whisper.cpp's format.
#[derive(Debug, Clone, Copy)]
pub struct WhisperAssets<'a> {
    pub model: Asset<'a>,
    pub vad: Asset<'a>,
}

pub const WHISPER_TURBO: WhisperAssets<'static> = WhisperAssets {
    model: Asset {
        kind: ModelKind::Speech,
        file_name: "ggml-large-v3-turbo-q5_0.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q5_0.bin",
        size: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    },
    vad: Asset {
        kind: ModelKind::Speech,
        file_name: "ggml-silero-v6.2.0.bin",
        url: "https://huggingface.co/ggml-org/whisper-vad/resolve/9ffd54a1e1ee413ddf265af9913beaf518d1639b/ggml-silero-v6.2.0.bin",
        size: 885_098,
        sha256: "2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987",
    },
};

impl WhisperAssets<'_> {
    /// The bytes of both files together.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.model.size + self.vad.size
    }
}

impl ModelFiles for WhisperAssets<'_> {
    fn files(&self) -> Vec<Asset<'_>> {
        vec![self.model, self.vad]
    }
}

/// The page of the whisper.cpp models that Whisper Turbo's files come from.
const WHISPER_SOURCE: &str = "https://huggingface.co/ggerganov/whisper.cpp";

/// A speech model that ships in the catalog and is chosen by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSpeech {
    Nemotron160ms,
    Unified160ms,
    ParakeetTdtV3,
}

/// A catalog model's files, which a model folder must also contain under these names.
#[derive(Debug, Clone, Copy)]
pub struct SpeechAssets<'a> {
    pub encoder: Asset<'a>,
    /// A file the encoder names in its own bytes and finds beside itself.
    pub encoder_data: Option<Asset<'a>>,
    pub decoder: Asset<'a>,
    pub joiner: Asset<'a>,
    pub tokens: Asset<'a>,
}

impl<'a> SpeechAssets<'a> {
    fn all(&self) -> impl Iterator<Item = Asset<'a>> {
        [self.encoder].into_iter().chain(self.encoder_data).chain([
            self.decoder,
            self.joiner,
            self.tokens,
        ])
    }

    /// The bytes of all files together.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.all().map(|asset| asset.size).sum()
    }
}

impl ModelFiles for SpeechAssets<'_> {
    fn files(&self) -> Vec<Asset<'_>> {
        self.all().collect()
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
    /// Every file is on disk at its pinned size.
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
            encoder_data: None,
            decoder: speech_assets!(@file $model, $base, "decoder.int8.onnx", $decoder),
            joiner: speech_assets!(@file $model, $base, "joiner.int8.onnx", $joiner),
            tokens: speech_assets!(@file $model, $base, "tokens.txt", $tokens),
        }
    };
    (@file $model:literal, $base:literal, $file:literal, ($size:expr, $sha256:literal)) => {
        speech_file!($model, $base, $file, $size, $sha256)
    };
}

/// The languages on NVIDIA's cards for the unified and Parakeet models.
const EUROPEAN_LANGUAGES: [&str; 25] = [
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
    pub const ALL: [Self; 3] = [Self::Nemotron160ms, Self::Unified160ms, Self::ParakeetTdtV3];

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
            Self::ParakeetTdtV3 => "Parakeet TDT v3",
        }
    }

    /// The catalog model that runs lighter on this computer, when this one may not keep up.
    #[must_use]
    pub fn lighter(self) -> Option<Self> {
        match self {
            Self::Nemotron160ms | Self::Unified160ms => Some(Self::ParakeetTdtV3),
            Self::ParakeetTdtV3 => None,
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Nemotron160ms => {
                "From NVIDIA: the lightest on the CPU, and text appears soonest. Mostly without punctuation. The default."
            }
            Self::Unified160ms => {
                "From NVIDIA: fewest word errors in our tests, with punctuation and capitals. It reads the last six seconds again every 160 ms, so it runs on a discrete GPU on Linux, and otherwise needs a fast CPU. Text appears about half a second later. Exported by Thalvi."
            }
            Self::ParakeetTdtV3 => {
                "From NVIDIA: light on the CPU, with punctuation and capitals. Text may change while you speak and settles when you pause. More word errors than Nemotron Unified in German and Spanish."
            }
        }
    }

    /// The model's license.
    #[must_use]
    pub fn license(self) -> &'static str {
        match self {
            Self::Nemotron160ms => "NVIDIA Open Model License",
            Self::Unified160ms => "OpenMDW-1.1",
            Self::ParakeetTdtV3 => "CC BY 4.0",
        }
    }

    /// The languages it recognizes.
    #[must_use]
    pub fn languages(self) -> &'static [&'static str] {
        match self {
            Self::Nemotron160ms => &["English"],
            Self::Unified160ms | Self::ParakeetTdtV3 => &EUROPEAN_LANGUAGES,
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
            Self::ParakeetTdtV3 => "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3",
        }
    }

    /// The name used in config.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Nemotron160ms => "nemotron_160ms",
            Self::Unified160ms => "unified_160ms",
            Self::ParakeetTdtV3 => "parakeet_tdt_v3",
        }
    }

    /// The downloads that provide this model, each pinned by size and checksum. Nemotron Speech
    /// Streaming (NVIDIA Open Model License) and Parakeet TDT (CC BY 4.0) are sherpa-onnx's int8
    /// exports, pinned by repository commit; Nemotron Unified ASR (OpenMDW-1.1) is Thalvi's own
    /// export, which is not published anywhere else.
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
            Self::ParakeetTdtV3 => speech_assets!(
                "parakeet_tdt_v3",
                "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/ed5d745623472f97cfc2e6c2916ee248798fc5a8/",
                (
                    652_282_294,
                    "1afbe69c1ffdabee94755f42e4daf1d93549131a3da11356bda84fcef0f2c479"
                ),
                (
                    11_845_274,
                    "0222a86147299d4dee90edf95b6d13827edb42ad8b7c9cf0b2582f030937c0b5"
                ),
                (
                    6_355_277,
                    "cbd02f3696e9944970111fb439d9c3fd365a80374565f13f2b9c7007d253fc98"
                ),
                (
                    93_939,
                    "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d"
                )
            ),
        }
    }
}

impl CatalogSpeech {
    /// Whether a discrete GPU runs this model faster than the CPU (measured on an RTX 5070 Ti; Nemotron
    /// Streaming and Parakeet TDT were no faster on any GPU).
    #[must_use]
    pub fn uses_gpu(self) -> bool {
        self.gpu_assets().is_some()
    }

    /// The fp16 export that a GPU runs, kept in a folder of its own so the encoder finds its data file by the
    /// name written in it. The tokens are the CPU files'. Exported by `speech/asr/export_unified_onnx.py --fp16`.
    #[must_use]
    pub fn gpu_assets(self) -> Option<SpeechAssets<'static>> {
        let Self::Unified160ms = self else {
            return None;
        };
        let file = |file_name, url, size, sha256| Asset {
            kind: ModelKind::Speech,
            file_name,
            url,
            size,
            sha256,
        };
        Some(SpeechAssets {
            encoder: file(
                "unified_160ms.gpu/encoder.fp16.onnx",
                "https://github.com/NotPellew/Thalvi/releases/download/speech-models-v2/encoder.fp16.onnx",
                8_431_807,
                "9a1cb2d08269264c7c6de0f2597c154c0607f7a30d94b97cc63020f721eee135",
            ),
            encoder_data: Some(file(
                "unified_160ms.gpu/encoder.fp16.onnx.data",
                "https://github.com/NotPellew/Thalvi/releases/download/speech-models-v2/encoder.fp16.onnx.data",
                1_227_462_144,
                "4961c570bf80837a1de4289e1673bb465c754961c57ec87b414f349f3fd5a492",
            )),
            decoder: file(
                "unified_160ms.gpu/decoder.fp32.onnx",
                "https://github.com/NotPellew/Thalvi/releases/download/speech-models-v2/decoder.fp32.onnx",
                36_745_424,
                "ad0af5918cae8d29b5fce9c9105bb932d5530f527c5c5b314532df8b21b23fdd",
            ),
            joiner: file(
                "unified_160ms.gpu/joiner.fp32.onnx",
                "https://github.com/NotPellew/Thalvi/releases/download/speech-models-v2/joiner.fp32.onnx",
                14_768_803,
                "955c2b24e16edcf9489c03141fa22c020626c1e1d566150da4113767c903917b",
            ),
            tokens: self.assets().tokens,
        })
    }

    /// The files of `export`; a model without a GPU export has only the int8 files.
    #[must_use]
    pub fn assets_of(self, export: Export) -> SpeechAssets<'static> {
        match export {
            Export::Gpu => self.gpu_assets().unwrap_or_else(|| self.assets()),
            Export::Cpu => self.assets(),
        }
    }

    /// The files `device` loads: the GPU export on a GPU, the int8 files otherwise.
    #[must_use]
    pub fn assets_on(self, device: &Device) -> SpeechAssets<'static> {
        self.assets_of(match device {
            Device::Gpu(_) => Export::Gpu,
            Device::Cpu(_) => Export::Cpu,
        })
    }
}

/// A speech model that the settings window lists and downloads by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListedSpeech {
    Catalog(CatalogSpeech),
    Whisper(WhisperBackend),
}

impl ListedSpeech {
    /// The listed model called `name` in config.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        CatalogSpeech::named(name)
            .map(Self::Catalog)
            .or_else(|| WhisperBackend::named(name).map(Self::Whisper))
    }

    /// The page of the model the files come from.
    #[must_use]
    pub fn source(self) -> &'static str {
        match self {
            Self::Catalog(model) => model.source(),
            Self::Whisper(_) => WHISPER_SOURCE,
        }
    }

    /// Whether the files of this model are the ones `choice` loads; both Whisper entries load the same.
    #[must_use]
    pub fn provides(self, choice: &SpeechChoice) -> bool {
        match (self, choice) {
            (Self::Catalog(listed), SpeechChoice::Catalog(chosen)) => listed == *chosen,
            (Self::Whisper(_), SpeechChoice::Whisper(_)) => true,
            _ => false,
        }
    }
}

/// Why a model could not be provided. Messages are shown to the user as written.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelError {
    #[error("Thalvi could not find a folder to keep its models in.")]
    NoDataDir,
    #[error("Could not download the {} {file}: {failure}", kind.noun())]
    Download {
        kind: ModelKind,
        file: String,
        failure: DownloadFailure,
        #[source]
        source: Arc<reqwest::Error>,
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
    #[error("The downloaded chat runtime {file} could not be unpacked: {reason}")]
    Unpack { file: String, reason: String },
    #[error("Could not save the {} {file} in {}: {source}", kind.noun(), dir.display())]
    Io {
        kind: ModelKind,
        file: String,
        dir: PathBuf,
        #[source]
        source: Arc<std::io::Error>,
    },
}

/// Which step of a download failed, as far as the user can act on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadFailure {
    #[error("could not connect to {host}. Check the internet connection and try again.")]
    Connect { host: String },
    #[error("the download timed out. Check the internet connection and try again.")]
    Timeout,
    #[error("the server refused it (HTTP {0}). Try again, and report it if it keeps happening.")]
    Status(reqwest::StatusCode),
    #[error(
        "the server's certificate was not trusted. Check this computer's date and time, or ask your IT team about the company certificate, then try again."
    )]
    Tls,
    #[error("the download broke off. Try again, and report it if it keeps happening.")]
    Broken,
}

impl DownloadFailure {
    fn of(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else if let Some(status) = error.status() {
            Self::Status(status)
        } else if error.is_connect() {
            if CertificateProblem::from_error(error).is_some() {
                Self::Tls
            } else {
                Self::Connect {
                    host: error
                        .url()
                        .and_then(|url| url.host_str())
                        .unwrap_or("the server")
                        .to_owned(),
                }
            }
        } else {
            Self::Broken
        }
    }
}

impl ModelError {
    fn download(kind: ModelKind, file: &str, source: reqwest::Error) -> Self {
        Self::Download {
            kind,
            file: file.to_owned(),
            failure: DownloadFailure::of(&source),
            // The signed CDN address in the error's text would reach the report.
            source: Arc::new(source.without_url()),
        }
    }

    /// What failed, for the opt-in metrics: `connect`, `timeout`, `status:<code>`, `tls`, `body`,
    /// `checksum`, `io`, `no_data_dir` or `already_downloading`.
    #[must_use]
    pub fn metric_kind(&self) -> String {
        match self {
            Self::NoDataDir => "no_data_dir".to_owned(),
            Self::Download { failure, .. } => match failure {
                DownloadFailure::Connect { .. } => "connect".to_owned(),
                DownloadFailure::Timeout => "timeout".to_owned(),
                DownloadFailure::Status(status) => format!("status:{}", status.as_u16()),
                DownloadFailure::Tls => "tls".to_owned(),
                DownloadFailure::Broken => "body".to_owned(),
            },
            Self::Checksum { .. } => "checksum".to_owned(),
            Self::Replaced { cause, .. } => cause.metric_kind(),
            Self::AlreadyDownloading { .. } => "already_downloading".to_owned(),
            Self::Io { .. } => "io".to_owned(),
            Self::Unpack { .. } => "unpack".to_owned(),
        }
    }
}

impl UserError for ModelError {
    fn headline(&self) -> String {
        match self {
            Self::NoDataDir => "No folder to keep models in",
            Self::Download { failure, .. } => match failure {
                DownloadFailure::Connect { .. } => "Can't reach the download server",
                DownloadFailure::Timeout => "Download timed out",
                DownloadFailure::Status(_) => "Download refused by server",
                DownloadFailure::Tls => "Download certificate not trusted",
                DownloadFailure::Broken => "Download broke off",
            },
            Self::Checksum { .. } => "Downloaded model was damaged",
            Self::Replaced { .. } => "Stored model was damaged",
            Self::AlreadyDownloading { .. } => "Model is already downloading",
            Self::Unpack { .. } => "Can't unpack the chat runtime",
            Self::Io { .. } => "Can't save the model file",
        }
        .to_owned()
    }
}

/// A directory of model files. A download is checked against its checksum once, as it is written;
/// a file already there is kept when its size matches.
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

    /// The directory holding verified model files and their runtimes.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `asset` is or would be stored; the file is not checked.
    #[must_use]
    pub fn path(&self, asset: &Asset<'_>) -> PathBuf {
        self.dir.join(asset.file_name)
    }

    /// Returns the path of the file, downloading it first if it is missing or the wrong size.
    /// A stored file of the pinned size is kept without reading it. A file of any other size is
    /// deleted before it is downloaded again.
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
        mut received: impl FnMut(u64),
    ) -> Result<PathBuf, ModelError> {
        let path = self.dir.join(asset.file_name);
        loop {
            match join_fetch(&path) {
                FetchRole::Leader(in_flight) => {
                    let guard = ActiveFetch {
                        path: path.clone(),
                        in_flight: Arc::clone(&in_flight),
                        finished: false,
                    };
                    let result = self
                        .fetch_as_leader(asset, &path, &in_flight, &mut received)
                        .await;
                    return guard.finish(result).map(|()| path);
                }
                FetchRole::Follower(in_flight) => {
                    let mut outcome = in_flight.outcome.subscribe();
                    let mut progress = in_flight.progress.subscribe();
                    let seen = *progress.borrow_and_update();
                    if seen > 0 {
                        received(seen);
                    }
                    loop {
                        if let Some(done) = outcome.borrow_and_update().clone() {
                            match done {
                                AttemptOutcome::Finished(result) => return result.map(|()| path),
                                AttemptOutcome::Cancelled => break,
                            }
                        }
                        tokio::select! {
                            biased;
                            changed = outcome.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                            changed = progress.changed() => {
                                if changed.is_ok() {
                                    received(*progress.borrow_and_update());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn fetch_as_leader(
        &self,
        asset: &Asset<'_>,
        path: &Path,
        in_flight: &InFlight,
        received: &mut impl FnMut(u64),
    ) -> Result<(), ModelError> {
        let damaged = match stored(path, asset).await {
            Stored::Intact => return Ok(()),
            Stored::Missing => false,
            Stored::Damaged => {
                tokio::fs::remove_file(path)
                    .await
                    .map_err(|source| ModelError::Io {
                        kind: asset.kind,
                        file: asset.file_name.to_owned(),
                        dir: self.dir.clone(),
                        source: Arc::new(source),
                    })?;
                true
            }
        };
        let result = self
            .download(asset, path, |bytes| {
                in_flight.progress.send_replace(bytes);
                received(bytes);
            })
            .await;
        match result {
            Ok(()) => Ok(()),
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
            source: Arc::new(source),
        };
        let net = |source| ModelError::download(asset.kind, asset.file_name, source);
        tokio::fs::create_dir_all(path.parent().unwrap_or(&self.dir))
            .await
            .map_err(io)?;
        let part = Partial(path.with_extension(format!(
            "{}-{}.part",
            std::process::id(),
            DOWNLOADS.fetch_add(1, Ordering::Relaxed)
        )));
        let mut response = reqwest::Client::builder()
            .user_agent(concat!("thalvi/", env!("CARGO_PKG_VERSION")))
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
                total += chunk.len() as u64;
                if total > asset.size {
                    return Err(ModelError::Checksum {
                        kind: asset.kind,
                        file: asset.file_name.to_owned(),
                    });
                }
                hasher.update(&chunk);
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

    /// Whether every file of `assets` is on disk at its pinned size.
    pub async fn is_cached(&self, assets: &impl ModelFiles) -> bool {
        for asset in assets.files() {
            match tokio::fs::metadata(self.dir.join(asset.file_name)).await {
                Ok(meta) if meta.len() == asset.size => {}
                _ => return false,
            }
        }
        true
    }

    /// Whether one pinned file is on disk at its pinned size.
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
                threshold: head.default_threshold(),
            });
        }
        models
    }

    /// The catalog models with whether each is already downloaded; the files counted are those `device`
    /// says the model loads.
    pub async fn speech_models(
        &self,
        device: impl Fn(CatalogSpeech) -> Device,
    ) -> Vec<SpeechModelInfo> {
        let mut models = Vec::new();
        for model in CatalogSpeech::ALL {
            let assets = model.assets_on(&device(model));
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

    /// The Whisper Turbo entries, which share one download, with whether it is on disk.
    pub async fn whisper_models(&self) -> Vec<SpeechModelInfo> {
        let cached = self.is_cached(&WHISPER_TURBO).await;
        WhisperBackend::ALL
            .map(|backend| SpeechModelInfo {
                name: backend.name(),
                title: backend.title(),
                description: backend.description(),
                size: WHISPER_TURBO.size(),
                source: WHISPER_SOURCE,
                license: "MIT",
                languages: &["English", "German"],
                cached,
            })
            .to_vec()
    }

    /// Fetches the files of `assets` that are not intact yet, telling `progress` how it goes.
    ///
    /// # Errors
    /// The first file's error; files fetched before it stay.
    pub async fn fetch_speech(
        &self,
        assets: &impl ModelFiles,
        mut progress: impl FnMut(SpeechProgress),
    ) -> Result<(), ModelError> {
        progress(SpeechProgress::Verifying);
        let files = assets.files();
        let total = files.iter().map(|asset| asset.size).sum::<u64>();
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
        for asset in files {
            let path = self.dir.join(asset.file_name);
            if !matches!(stored(&path, &asset).await, Stored::Intact) {
                self.fetch_reporting(&asset, |bytes| report(done + bytes))
                    .await?;
                report(done + asset.size);
            }
            done += asset.size;
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

/// The speech models being downloaded now and how far each is, so a model is never fetched twice at once.
#[derive(Debug, Default)]
pub struct SpeechDownloads {
    running: Mutex<HashMap<String, SpeechProgress>>,
}

impl SpeechDownloads {
    /// How far the download of the model called `name` is, if one is running.
    pub fn progress(&self, name: &str) -> Option<SpeechProgress> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .copied()
    }

    /// Runs [`ModelStore::fetch_speech`] for the model called `name` unless it is already running.
    ///
    /// # Errors
    /// [`ModelError::AlreadyDownloading`], or the download's own error.
    pub async fn run(
        &self,
        name: &str,
        store: &ModelStore,
        assets: &impl ModelFiles,
        mut progress: impl FnMut(SpeechProgress),
    ) -> Result<(), ModelError> {
        match self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(name.to_owned())
        {
            Entry::Occupied(_) => {
                return Err(ModelError::AlreadyDownloading {
                    name: name.to_owned(),
                });
            }
            Entry::Vacant(slot) => slot.insert(SpeechProgress::Verifying),
        };
        let _release = Release {
            running: &self.running,
            name,
        };
        store
            .fetch_speech(assets, |event| {
                self.running
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.to_owned(), event);
                progress(event);
            })
            .await
    }
}

struct Release<'a> {
    running: &'a Mutex<HashMap<String, SpeechProgress>>,
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

/// A file on disk is intact when it has the pinned size. The checksum is checked once, while the
/// download is written.
async fn stored(path: &Path, asset: &Asset<'_>) -> Stored {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() == asset.size => Stored::Intact,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Stored::Missing,
        _ => Stored::Damaged,
    }
}

/// `digest` as lowercase hexadecimal.
pub(crate) fn hex(digest: &[u8]) -> String {
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
        let cached: Vec<(&str, &str, bool, f32)> = models
            .iter()
            .map(|model| (model.name, model.title, model.cached, model.threshold))
            .collect();
        assert_eq!(
            cached,
            [
                ("hey_thalvi", "Hey Thalvi", true, 0.7),
                ("hey_mycroft", "Hey Mycroft", false, 0.5),
                ("hey_jarvis", "Hey Jarvis", false, 0.5),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An HTTP response carrying `body` as the whole file.
    fn response(body: &[u8]) -> Vec<u8> {
        let mut message = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        message.extend_from_slice(body);
        message
    }

    #[tokio::test]
    async fn two_fetches_of_one_file_share_one_download_and_both_get_the_stored_path() {
        use std::net::Ipv4Addr;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::sync::Notify;

        const BODY: &[u8] = b"one pinned wake model";

        let dir = std::env::temp_dir().join(format!("thalvi-shared-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let sha256 = hex(&Sha256::digest(BODY));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a test server");
        let url = format!(
            "http://{}/wake.onnx",
            listener.local_addr().expect("its address")
        );
        let connections = Arc::new(AtomicUsize::new(0));
        let requested = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = tokio::spawn({
            let connections = Arc::clone(&connections);
            let requested = Arc::clone(&requested);
            let release = Arc::clone(&release);
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let first = connections.fetch_add(1, Ordering::SeqCst) == 0;
                    let requested = Arc::clone(&requested);
                    let release = Arc::clone(&release);
                    tokio::spawn(async move {
                        let mut head = [0; 1024];
                        let _ = socket.read(&mut head).await;
                        requested.notify_one();
                        if first {
                            release.notified().await;
                        }
                        let _ = socket.write_all(&response(BODY)).await;
                    });
                }
            }
        });
        let fetch = |store: ModelStore| {
            let url = url.clone();
            let sha256 = sha256.clone();
            async move {
                let asset = Asset {
                    kind: ModelKind::Wake,
                    file_name: "wake.onnx",
                    url: &url,
                    size: u64::try_from(BODY.len()).expect("body size fits"),
                    sha256: &sha256,
                };
                store.fetch(&asset).await
            }
        };
        let first = tokio::spawn(fetch(ModelStore::in_dir(&dir)));
        let second = tokio::spawn(fetch(ModelStore::in_dir(&dir)));
        tokio::time::timeout(Duration::from_secs(10), requested.notified())
            .await
            .expect("the first fetch asked the server for the file");
        release.notify_one();
        let path = dir.join("wake.onnx");
        for fetch in [first, second] {
            let got = tokio::time::timeout(Duration::from_secs(10), fetch)
                .await
                .expect("a fetch returned")
                .expect("the fetch ran");
            assert_eq!(got.expect("the fetch found the file"), path);
        }
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&path).expect("the stored file"), BODY);

        // The file gone, the next fetch downloads it again rather than waiting on anything left over.
        std::fs::remove_file(&path).expect("remove the stored file");
        let asset = Asset {
            kind: ModelKind::Wake,
            file_name: "wake.onnx",
            url: &url,
            size: u64::try_from(BODY.len()).expect("body size fits"),
            sha256: &sha256,
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            ModelStore::in_dir(&dir).fetch(&asset),
        )
        .await
        .expect("the next fetch returned")
        .expect("the next fetch downloaded the file");
        assert_eq!(connections.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read(&path).expect("the stored file"), BODY);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Waits until a second fetch of `path` has subscribed to the attempt in flight.
    async fn follower_joined(path: &Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let joined = FETCHING
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(path)
                    .is_some_and(|in_flight| in_flight.outcome.receiver_count() > 0);
                if joined {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the second fetch joined the download in flight");
    }

    /// Waits until the download of `path` in flight has received `bytes`.
    async fn leader_received(path: &Path, bytes: u64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let received = FETCHING
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(path)
                    .is_some_and(|in_flight| *in_flight.progress.borrow() == bytes);
                if received {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the leading fetch received the first part");
    }

    /// Fetches the wake file of `size` bytes from `url` into `store`, with the byte counts it was told.
    async fn fetch_counting(
        store: ModelStore,
        url: String,
        sha256: String,
        size: usize,
    ) -> (Result<PathBuf, ModelError>, Vec<u64>) {
        let asset = Asset {
            kind: ModelKind::Wake,
            file_name: "wake.onnx",
            url: &url,
            size: u64::try_from(size).expect("body size fits"),
            sha256: &sha256,
        };
        let mut told = Vec::new();
        let result = store
            .fetch_reporting(&asset, |bytes| told.push(bytes))
            .await;
        (result, told)
    }

    #[tokio::test]
    async fn two_fetches_of_one_corrupt_file_share_one_download_and_a_later_retry_fetches_again() {
        use std::net::Ipv4Addr;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::sync::Notify;

        const GOOD: &[u8] = b"one pinned wake model";
        const CORRUPT: &[u8] = b"one pinned wake modeX";

        let dir = std::env::temp_dir().join(format!(
            "thalvi-shared-corrupt-fetch-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let sha256 = hex(&Sha256::digest(GOOD));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a test server");
        let url = format!(
            "http://{}/wake.onnx",
            listener.local_addr().expect("its address")
        );
        let connections = Arc::new(AtomicUsize::new(0));
        let requested = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = tokio::spawn({
            let connections = Arc::clone(&connections);
            let requested = Arc::clone(&requested);
            let release = Arc::clone(&release);
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let first = connections.fetch_add(1, Ordering::SeqCst) == 0;
                    let requested = Arc::clone(&requested);
                    let release = Arc::clone(&release);
                    tokio::spawn(async move {
                        let mut head = [0; 1024];
                        let _ = socket.read(&mut head).await;
                        requested.notify_one();
                        let reply = response(CORRUPT);
                        let (part, rest) = reply.split_at(reply.len() - CORRUPT.len() / 2);
                        let _ = socket.write_all(part).await;
                        if first {
                            release.notified().await;
                        }
                        let _ = socket.write_all(rest).await;
                    });
                }
            }
        });
        let fetch = |store| fetch_counting(store, url.clone(), sha256.clone(), GOOD.len());
        let first = tokio::spawn(fetch(ModelStore::in_dir(&dir)));
        tokio::time::timeout(Duration::from_secs(10), requested.notified())
            .await
            .expect("the first fetch asked the server for the file");
        let path = dir.join("wake.onnx");
        let part = (CORRUPT.len() - CORRUPT.len() / 2) as u64;
        leader_received(&path, part).await;
        let second = tokio::spawn(fetch(ModelStore::in_dir(&dir)));
        follower_joined(&path).await;
        release.notify_one();
        let (second, told) = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("the second fetch returned")
            .expect("the fetch ran");
        assert_eq!(
            told.first(),
            Some(&part),
            "a fetch that joins is told at once how far the download is"
        );
        let (first, _) = tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .expect("the first fetch returned")
            .expect("the fetch ran");
        for result in [first, second] {
            let err = result.expect_err("corrupt file fails checksum");
            assert!(matches!(err, ModelError::Checksum { .. }), "{err}");
        }
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert!(!path.exists());

        // An explicit retry after the failure starts a fresh attempt.
        let err = tokio::time::timeout(Duration::from_secs(10), fetch(ModelStore::in_dir(&dir)))
            .await
            .expect("retry returned")
            .0
            .expect_err("corrupt retry fails checksum");
        assert!(matches!(err, ModelError::Checksum { .. }), "{err}");
        assert_eq!(connections.load(Ordering::SeqCst), 2);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancelling_the_leading_fetch_lets_a_waiting_follower_finish() {
        use std::net::Ipv4Addr;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::sync::Notify;

        const BODY: &[u8] = b"recovered wake model";

        let dir = std::env::temp_dir().join(format!(
            "thalvi-cancel-handoff-fetch-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let sha256 = hex(&Sha256::digest(BODY));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a test server");
        let url = format!(
            "http://{}/wake.onnx",
            listener.local_addr().expect("its address")
        );
        let connections = Arc::new(AtomicUsize::new(0));
        let first_started = Arc::new(Notify::new());
        let server = tokio::spawn({
            let connections = Arc::clone(&connections);
            let first_started = Arc::clone(&first_started);
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let first = connections.fetch_add(1, Ordering::SeqCst) == 0;
                    let first_started = Arc::clone(&first_started);
                    tokio::spawn(async move {
                        let mut head = [0; 1024];
                        let _ = socket.read(&mut head).await;
                        if first {
                            first_started.notify_one();
                            std::future::pending::<()>().await;
                        } else {
                            let _ = socket.write_all(&response(BODY)).await;
                        }
                    });
                }
            }
        });
        let fetch = |store: ModelStore| {
            let url = url.clone();
            let sha256 = sha256.clone();
            async move {
                let asset = Asset {
                    kind: ModelKind::Wake,
                    file_name: "wake.onnx",
                    url: &url,
                    size: u64::try_from(BODY.len()).expect("body size fits"),
                    sha256: &sha256,
                };
                store.fetch(&asset).await
            }
        };
        let leader = tokio::spawn(fetch(ModelStore::in_dir(&dir)));
        tokio::time::timeout(Duration::from_secs(10), first_started.notified())
            .await
            .expect("leader requested file");
        let follower = tokio::spawn(fetch_counting(
            ModelStore::in_dir(&dir),
            url.clone(),
            sha256.clone(),
            BODY.len(),
        ));
        let path = dir.join("wake.onnx");
        follower_joined(&path).await;

        leader.abort();
        assert!(leader.await.expect_err("leader aborted").is_cancelled());
        let (got, told) = tokio::time::timeout(Duration::from_secs(10), follower)
            .await
            .expect("follower completed")
            .expect("follower ran");
        assert_eq!(got.expect("follower downloaded the file"), path);
        assert!(
            !told.contains(&0) && told.last() == Some(&(BODY.len() as u64)),
            "a fetch that joined before any byte arrived is told only of bytes that did: {told:?}"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read(&path).expect("saved"), BODY);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

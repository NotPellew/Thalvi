//! Local wake word scoring: openWakeWord features and two selectable heads on ONNX Runtime.
//!
//! Features follow openWakeWord 0.6.0's streaming path: each 80 ms frame is melspectrogrammed
//! with the 480 samples before it, the newest 76 spectrogram rows become one embedding, and a
//! head scores the newest 16 embeddings. History starts, and restarts on reset, as the embeddings
//! of fixed noise so the first frame already has a full context. Like upstream, a head's first
//! five scores after start or reset are reported as 0.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ort::environment::Environment;
use ort::session::Session;
use ort::value::{Outlet, Tensor, TensorElementType};
use serde::Deserialize;

use crate::frame::Frame;
use crate::models::{self, CatalogHead, ModelError, ModelStore};

const MEL_BINS: usize = 32;
const EMBEDDING_WINDOW: usize = 76;
const EMBEDDING_STEP: usize = 8;
const EMBEDDING_DIM: usize = 96;
/// Embeddings a head scores at once.
const HEAD_CONTEXT: usize = 16;
/// Samples before a frame that its melspectrogram also needs.
const MEL_LEAD_IN: usize = 480;
/// Scores reported as 0 after start or reset.
const WARM_UP_FRAMES: usize = 5;
const SEED_SAMPLES: usize = 64_000;

/// Which head crossed its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeEvent {
    Capture,
    Conversation,
    /// Both heads crossed on the same frame.
    Ambiguous,
}

/// What one frame produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WakeFrame {
    pub capture: f32,
    pub conversation: f32,
    /// Real time spent on this frame, for the opt-in metrics.
    pub inference: Duration,
    pub event: Option<WakeEvent>,
}

/// Why wake config was rejected. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error(
        "Unknown wake model \"{0}\". Use hey_thalvi, hey_mycroft, hey_jarvis, or the path of an .onnx file."
    )]
    UnknownModel(String),
    #[error("A wake threshold must be above 0 and at most 1.")]
    Threshold,
    #[error("The capture and conversation wake models must be different.")]
    SameModel,
}

/// A head chosen by catalog name or by the path of a local `.onnx` file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum HeadModel {
    Catalog(CatalogHead),
    File(PathBuf),
}

impl TryFrom<String> for HeadModel {
    type Error = ConfigError;

    fn try_from(value: String) -> Result<Self, ConfigError> {
        if let Some(head) = CatalogHead::ALL.iter().find(|head| head.name() == value) {
            Ok(Self::Catalog(*head))
        } else if Path::new(&value)
            .extension()
            .is_some_and(|ext| ext == "onnx")
        {
            Ok(Self::File(PathBuf::from(value)))
        } else {
            Err(ConfigError::UnknownModel(value))
        }
    }
}

/// The score at or above which a head wakes Thalvi.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(try_from = "f32")]
pub struct Threshold(f32);

impl TryFrom<f32> for Threshold {
    type Error = ConfigError;

    fn try_from(value: f32) -> Result<Self, ConfigError> {
        if value > 0.0 && value <= 1.0 {
            Ok(Self(value))
        } else {
            Err(ConfigError::Threshold)
        }
    }
}

impl Threshold {
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }
}

/// One head's model and threshold.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadConfig {
    pub model: HeadModel,
    #[serde(default = "default_threshold")]
    pub threshold: Threshold,
}

fn default_threshold() -> Threshold {
    Threshold(0.5)
}

fn default_head(head: CatalogHead) -> HeadConfig {
    HeadConfig {
        model: HeadModel::Catalog(head),
        threshold: default_threshold(),
    }
}

/// Both heads, validated once when read: the two models differ.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "RawWakeConfig")]
pub struct WakeConfig {
    capture: HeadConfig,
    conversation: HeadConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWakeConfig {
    #[serde(default = "default_capture")]
    capture: HeadConfig,
    #[serde(default = "default_conversation")]
    conversation: HeadConfig,
}

fn default_capture() -> HeadConfig {
    default_head(CatalogHead::HeyThalvi)
}

fn default_conversation() -> HeadConfig {
    default_head(CatalogHead::HeyMycroft)
}

impl TryFrom<RawWakeConfig> for WakeConfig {
    type Error = ConfigError;

    fn try_from(raw: RawWakeConfig) -> Result<Self, ConfigError> {
        if raw.capture.model == raw.conversation.model {
            return Err(ConfigError::SameModel);
        }
        Ok(Self {
            capture: raw.capture,
            conversation: raw.conversation,
        })
    }
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            capture: default_capture(),
            conversation: default_conversation(),
        }
    }
}

impl WakeConfig {
    #[must_use]
    pub fn capture(&self) -> &HeadConfig {
        &self.capture
    }

    #[must_use]
    pub fn conversation(&self) -> &HeadConfig {
        &self.conversation
    }
}

/// Why wake could not load or stopped scoring. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum WakeError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("Could not start the wake word engine: {0}")]
    Runtime(String),
    #[error("The wake model {model} could not be loaded: {reason}")]
    Load { model: String, reason: String },
    #[error("The wake model {model} cannot be used as a wake head: {reason}")]
    Head { model: String, reason: &'static str },
    #[error("Wake word detection failed: {0}")]
    Inference(String),
}

struct Head {
    session: Session,
    threshold: f32,
}

/// The wake pipeline: call [`process`](Self::process) with every microphone frame in order.
pub struct WakeEngine {
    mel: Session,
    embedding: Session,
    capture: Head,
    conversation: Head,
    tail: Vec<f32>,
    mel_rows: VecDeque<[f32; MEL_BINS]>,
    features: VecDeque<[f32; EMBEDDING_DIM]>,
    seed: VecDeque<[f32; EMBEDDING_DIM]>,
    frames: usize,
}

impl WakeEngine {
    /// Fetches any missing model files through `store`, then loads and validates them.
    ///
    /// # Errors
    /// Fails when a model cannot be downloaded or loaded, or a head does not take 16 embeddings
    /// and return one score.
    pub async fn load(config: &WakeConfig, store: &ModelStore) -> Result<Self, WakeError> {
        let mel = store.fetch(&models::MELSPECTROGRAM).await?;
        let embedding = store.fetch(&models::EMBEDDING).await?;
        let capture = head_path(config.capture(), store).await?;
        let conversation = head_path(config.conversation(), store).await?;
        let mut engine = Self {
            mel: open(&mel)?,
            embedding: open(&embedding)?,
            capture: open_head(&capture, config.capture().threshold)?,
            conversation: open_head(&conversation, config.conversation().threshold)?,
            tail: Vec::new(),
            mel_rows: VecDeque::new(),
            features: VecDeque::new(),
            seed: VecDeque::new(),
            frames: 0,
        };
        engine.seed = engine.noise_embeddings()?;
        engine.reset();
        Ok(engine)
    }

    /// Forgets the audio so far, as when capture ends and the cue must not wake it again.
    pub fn reset(&mut self) {
        self.tail.clear();
        self.mel_rows = VecDeque::from(vec![[1.0; MEL_BINS]; EMBEDDING_WINDOW]);
        self.features.clone_from(&self.seed);
        self.frames = 0;
    }

    /// Scores one 80 ms frame with both heads.
    ///
    /// # Errors
    /// Fails when a model returns something other than the documented shapes or a score outside
    /// 0 to 1.
    pub fn process(&mut self, frame: &Frame) -> Result<WakeFrame, WakeError> {
        let started = Instant::now();
        let mut audio = std::mem::take(&mut self.tail);
        audio.extend(frame.iter().map(|&sample| f32::from(sample)));
        let rows = melspectrogram(&mut self.mel, &audio)?;
        self.tail = audio.split_off(audio.len() - MEL_LEAD_IN);
        self.mel_rows.extend(rows);
        let excess = self.mel_rows.len().saturating_sub(EMBEDDING_WINDOW);
        self.mel_rows.drain(..excess);

        let window: Vec<f32> = self.mel_rows.iter().flatten().copied().collect();
        let embedded = embed(&mut self.embedding, window, 1)?;
        self.features.extend(embedded);
        let excess = self.features.len().saturating_sub(HEAD_CONTEXT);
        self.features.drain(..excess);

        let (capture, conversation) = if self.frames < WARM_UP_FRAMES {
            (0.0, 0.0)
        } else {
            let context: Vec<f32> = self.features.iter().flatten().copied().collect();
            (
                score(&mut self.capture.session, &context)?,
                score(&mut self.conversation.session, &context)?,
            )
        };
        self.frames = self.frames.saturating_add(1);
        let event = match (
            capture >= self.capture.threshold,
            conversation >= self.conversation.threshold,
        ) {
            (true, true) => Some(WakeEvent::Ambiguous),
            (true, false) => Some(WakeEvent::Capture),
            (false, true) => Some(WakeEvent::Conversation),
            (false, false) => None,
        };
        Ok(WakeFrame {
            capture,
            conversation,
            inference: started.elapsed(),
            event,
        })
    }

    /// The embeddings openWakeWord starts from: those of a fixed 4 s of noise.
    fn noise_embeddings(&mut self) -> Result<VecDeque<[f32; EMBEDDING_DIM]>, WakeError> {
        let rows = melspectrogram(&mut self.mel, &seed_noise())?;
        let starts: Vec<usize> = (0..rows.len())
            .step_by(EMBEDDING_STEP)
            .filter(|start| start + EMBEDDING_WINDOW <= rows.len())
            .collect();
        let windows: Vec<f32> = starts
            .iter()
            .flat_map(|&start| {
                rows[start..start + EMBEDDING_WINDOW]
                    .iter()
                    .flatten()
                    .copied()
            })
            .collect();
        let mut embedded = embed(&mut self.embedding, windows, starts.len())?;
        let excess = embedded.len().saturating_sub(HEAD_CONTEXT);
        embedded.drain(..excess);
        Ok(embedded)
    }
}

/// Fixed pseudo-noise in ±1000 that stands in for upstream's random seed history, so that a
/// start and a reset behave the same every time.
fn seed_noise() -> Vec<f32> {
    let mut state: u32 = 1;
    (0..SEED_SAMPLES)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [high, low, ..] = state.to_be_bytes();
            f32::from(u16::from_be_bytes([high, low]) % 2000) - 1000.0
        })
        .collect()
}

async fn head_path(head: &HeadConfig, store: &ModelStore) -> Result<PathBuf, WakeError> {
    match &head.model {
        HeadModel::Catalog(catalog) => Ok(store.fetch(&catalog.asset()).await?),
        HeadModel::File(path) => Ok(path.clone()),
    }
}

fn environment() -> Result<&'static Environment, WakeError> {
    static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();
    ENVIRONMENT
        .get_or_init(|| ort::init().build().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|reason| WakeError::Runtime(reason.clone()))
}

fn open(path: &Path) -> Result<Session, WakeError> {
    let load = |reason: ort::Error| WakeError::Load {
        model: path.display().to_string(),
        reason: reason.to_string(),
    };
    let environment = environment()?;
    let build = || -> ort::Result<Session> {
        Session::builder(environment)?
            .with_intra_threads(1)?
            .with_inter_threads(1)?
            .commit_from_file(path)
    };
    build().map_err(load)
}

fn open_head(path: &Path, threshold: Threshold) -> Result<Head, WakeError> {
    let session = open(path)?;
    check_head(session.inputs(), session.outputs()).map_err(|reason| WakeError::Head {
        model: path.display().to_string(),
        reason,
    })?;
    Ok(Head {
        session,
        threshold: threshold.get(),
    })
}

/// Checks a head takes `[batch, 16, 96]` floats and returns `[batch, 1]` floats; a batch is 1
/// or any size.
fn check_head(inputs: &[Outlet], outputs: &[Outlet]) -> Result<(), &'static str> {
    let ([input], [output]) = (inputs, outputs) else {
        return Err("it must have exactly one input and one output");
    };
    let float = |outlet: &Outlet| outlet.dtype().tensor_type() == Some(TensorElementType::Float32);
    if !float(input) || !float(output) {
        return Err("its input and output must be 32-bit floats");
    }
    let (Some(input), Some(output)) = (input.dtype().tensor_shape(), output.dtype().tensor_shape())
    else {
        return Err("its input and output must be tensors");
    };
    let batch = |dim: i64| dim == 1 || dim == -1;
    match (&**input, &**output) {
        ([b, 16, 96], [ob, 1]) if batch(*b) && batch(*ob) => Ok(()),
        _ => Err("its input must be [batch, 16, 96] and its output [batch, 1]"),
    }
}

fn run(session: &mut Session, shape: &[usize], data: Vec<f32>) -> Result<Vec<f32>, WakeError> {
    let fail = |error: ort::Error| WakeError::Inference(error.to_string());
    let input = Tensor::from_array((shape.to_vec(), data)).map_err(fail)?;
    let outputs = session.run(ort::inputs![input]).map_err(fail)?;
    let (_, values) = outputs[0].try_extract_tensor::<f32>().map_err(fail)?;
    Ok(values.to_vec())
}

fn melspectrogram(session: &mut Session, audio: &[f32]) -> Result<Vec<[f32; MEL_BINS]>, WakeError> {
    let values = run(session, &[1, audio.len()], audio.to_vec())?;
    if values.is_empty() || values.len() % MEL_BINS != 0 {
        return Err(WakeError::Inference(
            "the spectrogram model returned an unexpected size".to_owned(),
        ));
    }
    // Upstream rescales the log-mel output toward Google's original speech_embedding input.
    Ok(values
        .as_chunks::<MEL_BINS>()
        .0
        .iter()
        .map(|row| row.map(|value| value / 10.0 + 2.0))
        .collect())
}

fn embed(
    session: &mut Session,
    windows: Vec<f32>,
    count: usize,
) -> Result<VecDeque<[f32; EMBEDDING_DIM]>, WakeError> {
    let values = run(session, &[count, EMBEDDING_WINDOW, MEL_BINS, 1], windows)?;
    if values.len() != count * EMBEDDING_DIM {
        return Err(WakeError::Inference(
            "the embedding model returned an unexpected size".to_owned(),
        ));
    }
    Ok(values
        .as_chunks::<EMBEDDING_DIM>()
        .0
        .iter()
        .copied()
        .collect())
}

fn score(session: &mut Session, context: &[f32]) -> Result<f32, WakeError> {
    let values = run(session, &[1, HEAD_CONTEXT, EMBEDDING_DIM], context.to_vec())?;
    match values.as_slice() {
        [score] if (0.0..=1.0).contains(score) => Ok(*score),
        _ => Err(WakeError::Inference(
            "a wake head returned an invalid score".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use ort::value::{Shape, SymbolicDimensions, ValueType};

    use super::*;

    const FLOAT: TensorElementType = TensorElementType::Float32;

    fn outlet(ty: TensorElementType, dims: &[i64]) -> Outlet {
        let rank = dims.len();
        Outlet::new(
            "tensor",
            ValueType::Tensor {
                ty,
                shape: Shape::new(dims.iter().copied()),
                dimension_symbols: SymbolicDimensions::empty(rank),
            },
        )
    }

    /// A dimension of -1 is a symbolic or unnamed one, as ONNX Runtime reports it.
    #[test]
    fn a_head_must_take_16_by_96_floats_and_return_one_float() {
        let double = TensorElementType::Float64;
        let int = TensorElementType::Int64;
        // (inputs, outputs, valid)
        let cases: Vec<(Vec<Outlet>, Vec<Outlet>, bool)> = vec![
            (
                vec![outlet(FLOAT, &[-1, 16, 96])],
                vec![outlet(FLOAT, &[-1, 1])],
                true,
            ),
            (
                vec![outlet(FLOAT, &[1, 16, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                true,
            ),
            (
                vec![outlet(FLOAT, &[1, 0, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 15, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 17, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 120, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 121, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, -1, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[2, 16, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 16, 95])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 16, 96])],
                vec![outlet(FLOAT, &[1, 2])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 16, 96])],
                vec![outlet(FLOAT, &[1, -1])],
                false,
            ),
            (
                vec![outlet(double, &[1, 16, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 16, 96])],
                vec![outlet(int, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[16, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 16, 96])],
                vec![outlet(FLOAT, &[1, 1, 1])],
                false,
            ),
            (
                vec![outlet(FLOAT, &[1, 16, 96]), outlet(FLOAT, &[1, 16, 96])],
                vec![outlet(FLOAT, &[1, 1])],
                false,
            ),
            (vec![outlet(FLOAT, &[1, 16, 96])], vec![], false),
        ];
        for (index, (inputs, outputs, valid)) in cases.iter().enumerate() {
            assert_eq!(
                check_head(inputs, outputs).is_ok(),
                *valid,
                "case {index}: {:?} -> {:?}",
                inputs.iter().map(Outlet::dtype).collect::<Vec<_>>(),
                outputs.iter().map(Outlet::dtype).collect::<Vec<_>>()
            );
        }
    }

    fn read(json: &str) -> Result<WakeConfig, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    #[test]
    fn config_defaults_to_hey_thalvi_and_hey_mycroft_at_one_half() {
        assert_eq!(read("{}").expect("valid"), WakeConfig::default());
        let config = WakeConfig::default();
        assert_eq!(
            config.capture().model,
            HeadModel::Catalog(CatalogHead::HeyThalvi)
        );
        assert_eq!(
            config.conversation().model,
            HeadModel::Catalog(CatalogHead::HeyMycroft)
        );
        assert_eq!(config.capture().threshold.get(), 0.5);
        assert_eq!(config.conversation().threshold.get(), 0.5);
    }

    #[test]
    fn each_head_is_a_catalog_name_or_an_onnx_path_with_its_own_threshold() {
        let config = read(
            r#"{"capture":{"model":"hey_jarvis","threshold":0.8},"conversation":{"model":"/models/mine.onnx"}}"#,
        )
        .expect("valid");
        assert_eq!(
            config.capture().model,
            HeadModel::Catalog(CatalogHead::HeyJarvis)
        );
        assert_eq!(config.capture().threshold.get(), 0.8);
        assert_eq!(
            config.conversation().model,
            HeadModel::File("/models/mine.onnx".into())
        );
        assert_eq!(config.conversation().threshold.get(), 0.5);
    }

    #[test]
    fn config_that_cannot_work_is_refused_in_plain_words() {
        let cases = [
            (
                r#"{"capture":{"model":"hey_siri"}}"#,
                "Unknown wake model \"hey_siri\"",
            ),
            (r#"{"capture":{"model":"model.txt"}}"#, "Unknown wake model"),
            (
                r#"{"capture":{"model":"hey_mycroft"}}"#,
                "must be different",
            ),
            (
                r#"{"capture":{"model":"a.onnx"},"conversation":{"model":"a.onnx"}}"#,
                "must be different",
            ),
            (
                r#"{"capture":{"model":"hey_thalvi","threshold":0}}"#,
                "above 0 and at most 1",
            ),
            (
                r#"{"capture":{"model":"hey_thalvi","threshold":1.5}}"#,
                "above 0 and at most 1",
            ),
            (
                r#"{"capture":{"model":"hey_thalvi","threshold":-0.2}}"#,
                "above 0 and at most 1",
            ),
            (
                r#"{"capture":{"model":"hey_thalvi","treshold":0.5}}"#,
                "unknown field",
            ),
        ];
        for (json, message) in cases {
            let error = read(json).expect_err(json);
            assert!(error.contains(message), "{json}: {error}");
        }
        assert!(read(r#"{"capture":{"model":"hey_thalvi","threshold":1.0}}"#).is_ok());
    }
}

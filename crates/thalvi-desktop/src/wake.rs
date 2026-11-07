//! Local wake word scoring: openWakeWord features and two selectable heads on ONNX Runtime.
//!
//! Features follow openWakeWord 0.6.0's streaming path: each 80 ms frame is melspectrogrammed
//! with the 480 samples before it, the newest 76 spectrogram rows become one embedding, and a
//! head scores the newest 16 embeddings. History starts, and restarts on reset, as the embeddings
//! of fixed noise so the first frame already has a full context. Like upstream, a head's first
//! five scores after start or reset are reported as 0.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ort::session::Session;
use ort::value::{Outlet, Tensor, TensorElementType};
use serde::{Deserialize, Serialize};

use crate::frame::Frame;
use crate::models::{self, CatalogHead, ModelError, ModelStore};
use crate::report::UserError;
use crate::runtime;

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

impl Serialize for HeadModel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Catalog(head) => serializer.serialize_str(head.name()),
            Self::File(path) => path.serialize(serializer),
        }
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

impl WakeConfig {
    /// Builds both heads, refusing the same model for both.
    ///
    /// # Errors
    /// [`ConfigError::SameModel`] when both heads use the same model.
    pub fn new(capture: HeadConfig, conversation: HeadConfig) -> Result<Self, ConfigError> {
        if capture.model == conversation.model {
            return Err(ConfigError::SameModel);
        }
        Ok(Self {
            capture,
            conversation,
        })
    }
}

impl TryFrom<RawWakeConfig> for WakeConfig {
    type Error = ConfigError;

    fn try_from(raw: RawWakeConfig) -> Result<Self, ConfigError> {
        Self::new(raw.capture, raw.conversation)
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

/// Checks two chosen wake models the way listening will use them: each catalog name must be
/// known, each local file must load as a wake head, and the two must differ. Catalog heads are
/// not downloaded here; they download on first use, as before.
///
/// # Errors
/// The plain reason a choice cannot be used: an unknown name, a file that cannot be loaded as
/// a head, or the same model chosen for both.
pub fn validate_wake_models(capture: &str, conversation: &str) -> Result<(), WakeError> {
    let capture = HeadModel::try_from(capture.trim().to_owned())?;
    let conversation = HeadModel::try_from(conversation.trim().to_owned())?;
    for model in [&capture, &conversation] {
        if let HeadModel::File(path) = model {
            open_head(path, default_threshold())?;
        }
    }
    WakeConfig::new(
        HeadConfig {
            model: capture,
            threshold: default_threshold(),
        },
        HeadConfig {
            model: conversation,
            threshold: default_threshold(),
        },
    )?;
    Ok(())
}

/// Why wake could not load or stopped scoring. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum WakeError {
    #[error(transparent)]
    Config(#[from] ConfigError),
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

impl UserError for WakeError {
    fn headline(&self) -> &'static str {
        match self {
            Self::Config(_) => "Wake settings can't be used",
            Self::Model(error) => error.headline(),
            Self::Runtime(_) => "Wake word engine won't start",
            Self::Load { .. } => "Wake model can't be loaded",
            Self::Head { .. } => "Wake model can't be used",
            Self::Inference(_) => "Wake word detection failed",
        }
    }
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
    /// Whether [`load`](Self::load) would download a file: any pinned file it fetches that is
    /// not on disk. A head given as a local path never downloads.
    pub async fn needs_download(config: &WakeConfig, store: &ModelStore) -> bool {
        let heads = [config.capture(), config.conversation()]
            .into_iter()
            .filter_map(head_asset);
        for asset in [models::MELSPECTROGRAM, models::EMBEDDING]
            .into_iter()
            .chain(heads)
        {
            if !store.is_asset_cached(&asset).await {
                return true;
            }
        }
        false
    }

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

/// The download a head needs, if it is a catalog head.
fn head_asset(head: &HeadConfig) -> Option<models::Asset<'static>> {
    match &head.model {
        HeadModel::Catalog(catalog) => Some(catalog.asset()),
        HeadModel::File(_) => None,
    }
}

async fn head_path(head: &HeadConfig, store: &ModelStore) -> Result<PathBuf, WakeError> {
    match &head.model {
        HeadModel::Catalog(catalog) => Ok(store.fetch(&catalog.asset()).await?),
        HeadModel::File(path) => Ok(path.clone()),
    }
}

fn open(path: &Path) -> Result<Session, WakeError> {
    let load = |reason: ort::Error| WakeError::Load {
        model: path.display().to_string(),
        reason: reason.to_string(),
    };
    let environment = runtime::environment().map_err(WakeError::Runtime)?;
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

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = u8::try_from(value & 0x7f).unwrap_or_default();
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn bytes_field(field: u32, bytes: &[u8], out: &mut Vec<u8>) {
        varint(u64::from(field) << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn str_field(field: u32, value: &str, out: &mut Vec<u8>) {
        bytes_field(field, value.as_bytes(), out);
    }

    fn int_field(field: u32, value: i64, out: &mut Vec<u8>) {
        varint(u64::from(field) << 3, out);
        varint(value.cast_unsigned(), out);
    }

    fn value_info(name: &str, dims: &[i64]) -> Vec<u8> {
        let mut shape = Vec::new();
        for dim in dims {
            let mut dimension = Vec::new();
            int_field(1, *dim, &mut dimension);
            bytes_field(1, &dimension, &mut shape);
        }
        let mut tensor = Vec::new();
        int_field(1, 1, &mut tensor);
        bytes_field(2, &shape, &mut tensor);
        let mut ty = Vec::new();
        bytes_field(1, &tensor, &mut ty);
        let mut info = Vec::new();
        str_field(1, name, &mut info);
        bytes_field(2, &ty, &mut info);
        info
    }

    fn attribute_ints(name: &str, values: &[i64]) -> Vec<u8> {
        let mut packed = Vec::new();
        for value in values {
            varint(value.cast_unsigned(), &mut packed);
        }
        let mut attribute = Vec::new();
        str_field(1, name, &mut attribute);
        bytes_field(8, &packed, &mut attribute);
        int_field(20, 7, &mut attribute);
        attribute
    }

    fn attribute_int(name: &str, value: i64) -> Vec<u8> {
        let mut attribute = Vec::new();
        str_field(1, name, &mut attribute);
        int_field(3, value, &mut attribute);
        int_field(20, 2, &mut attribute);
        attribute
    }

    fn node(op: &str, input: &str, output: &str, attributes: &[Vec<u8>]) -> Vec<u8> {
        let mut node = Vec::new();
        str_field(1, input, &mut node);
        str_field(2, output, &mut node);
        str_field(4, op, &mut node);
        for attribute in attributes {
            bytes_field(5, attribute, &mut node);
        }
        node
    }

    /// A head ONNX taking `input` and giving the mean of all embeddings as `[1, 1]`; it loads in
    /// ONNX Runtime without any downloaded model.
    fn head_onnx(input: &[i64]) -> Vec<u8> {
        let reduce = node(
            "ReduceMean",
            "x",
            "r",
            &[
                attribute_ints("axes", &[1, 2]),
                attribute_int("keepdims", 0),
            ],
        );
        let unsqueeze = node("Unsqueeze", "r", "y", &[attribute_ints("axes", &[1])]);
        let mut graph = Vec::new();
        bytes_field(1, &reduce, &mut graph);
        bytes_field(1, &unsqueeze, &mut graph);
        str_field(2, "head", &mut graph);
        bytes_field(11, &value_info("x", input), &mut graph);
        bytes_field(12, &value_info("y", &[1, 1]), &mut graph);
        let mut opset = Vec::new();
        int_field(2, 7, &mut opset);
        let mut model = Vec::new();
        int_field(1, 7, &mut model);
        bytes_field(7, &graph, &mut model);
        bytes_field(8, &opset, &mut model);
        model
    }

    #[test]
    fn validate_wake_models_checks_catalog_files_and_sameness() {
        let dir = std::env::temp_dir().join(format!("thalvi-wake-validate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let valid = dir.join("custom.onnx");
        std::fs::write(&valid, head_onnx(&[1, 16, 96])).expect("valid head");
        let wrong = dir.join("wrong.onnx");
        std::fs::write(&wrong, head_onnx(&[1, 8, 96])).expect("wrong-shape head");
        let not_model = dir.join("not-model.onnx");
        std::fs::write(&not_model, b"not a model").expect("non-model");
        let missing = dir.join("missing.onnx");
        let valid = valid.display().to_string();
        let not_model = not_model.display().to_string();
        let missing = missing.display().to_string();
        let wrong = wrong.display().to_string();
        // (capture, conversation, expected message part; empty means valid)
        let cases: [(&str, &str, &str); 8] = [
            ("hey_thalvi", "hey_mycroft", ""),
            (valid.as_str(), "hey_mycroft", ""),
            (missing.as_str(), "hey_mycroft", "could not be loaded"),
            (not_model.as_str(), "hey_mycroft", "could not be loaded"),
            (wrong.as_str(), "hey_mycroft", "wake head"),
            ("hey_siri", "hey_mycroft", "Unknown wake model"),
            ("hey_thalvi", "hey_thalvi", "must be different"),
            (valid.as_str(), valid.as_str(), "must be different"),
        ];
        for (capture, conversation, expected) in cases {
            let result = validate_wake_models(capture, conversation);
            if expected.is_empty() {
                assert!(result.is_ok(), "{capture} / {conversation}: {result:?}");
            } else {
                let error = result.expect_err("must refuse").to_string();
                assert!(
                    error.contains(expected),
                    "{capture} / {conversation}: {error}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
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

    #[tokio::test]
    async fn loading_downloads_only_when_a_pinned_file_is_missing() {
        let mel = models::MELSPECTROGRAM;
        let embedding = models::EMBEDDING;
        let thalvi = CatalogHead::HeyThalvi.asset();
        let mycroft = CatalogHead::HeyMycroft.asset();
        let jarvis = CatalogHead::HeyJarvis.asset();
        let files =
            r#"{"capture":{"model":"/a/mine.onnx"},"conversation":{"model":"/a/yours.onnx"}}"#;
        // (case, config, files on disk, downloads)
        let cases = [
            ("nothing on disk", "{}", vec![], true),
            (
                "every file",
                "{}",
                vec![mel, embedding, thalvi, mycroft],
                false,
            ),
            (
                "no melspectrogram",
                "{}",
                vec![embedding, thalvi, mycroft],
                true,
            ),
            ("no embedding", "{}", vec![mel, thalvi, mycroft], true),
            ("no capture head", "{}", vec![mel, embedding, mycroft], true),
            (
                "no conversation head",
                "{}",
                vec![mel, embedding, thalvi],
                true,
            ),
            (
                "a head not chosen is not needed",
                r#"{"capture":{"model":"hey_jarvis"}}"#,
                vec![mel, embedding, jarvis, mycroft],
                false,
            ),
            (
                "local heads and the features",
                files,
                vec![mel, embedding],
                false,
            ),
            ("local heads, no features", files, vec![], true),
            (
                "a local head and a missing catalog head",
                r#"{"capture":{"model":"/a/mine.onnx"}}"#,
                vec![mel, embedding],
                true,
            ),
        ];
        for (case, config, on_disk, downloads) in cases {
            let dir = std::env::temp_dir().join(format!(
                "thalvi-wake-download-{}-{}",
                case.replace(' ', "-"),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            for asset in on_disk {
                std::fs::File::create(dir.join(asset.file_name))
                    .and_then(|file| file.set_len(asset.size))
                    .expect("file at pinned size");
            }
            let config = read(config).expect("valid config");
            assert_eq!(
                WakeEngine::needs_download(&config, &ModelStore::in_dir(&dir)).await,
                downloads,
                "{case}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

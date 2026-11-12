//! Owns the `NeMo` transducers (LSTM predictor, joiner and a cache-aware or buffered encoder) on ONNX Runtime, and the model choice.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ort::session::Session;
use ort::session::builder::SessionBuilder;
use ort::value::Tensor;

use crate::boosting::{Joiner, PhraseTree, TreeState, greedy};
use crate::device::{
    self, Adapters, Attempt, Device, Export, Fallback, Gpu, Nodes, Part, Placement, Provider, Step,
};
use crate::fbank::{HOP, LogMel, MEL_BINS, MelFrame, WindowMel};
use crate::frame::Frame;
use crate::local::{ModelSource, SpeechModel, SpeechStream};
use crate::models::{CatalogSpeech, ModelStore, SpeechProgress};
use crate::phrases::WordList;
use crate::recognizer::{RecognizerError, RecognizerEvent};
use crate::runtime;
use crate::segments::{FrameDecoder, Revising, Segments, SnapshotDecoder};
use crate::tdt::{DURATIONS, tdt_greedy};

/// Silence after speech that ends a stretch of it, as a `Final`.
const ENDPOINT_MS: usize = 600;
/// One feature frame is 10 ms.
const FRAME_MS: usize = 10;
/// Silence that the look-ahead appends for a buffered model: its end-of-audio step brings out the rest.
const BUFFERED_LOOKAHEAD: usize = 4_400;
/// Silence after a cache-aware window that lets its last frame's tail through.
const WINDOW_TAIL: usize = 400;

/// The files of one exported model.
#[derive(Clone, Debug)]
pub struct TransducerFiles {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

impl TransducerFiles {
    /// The four files a model folder must hold; the only place their names are written.
    #[must_use]
    pub fn in_folder(dir: &Path) -> Self {
        Self {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        }
    }
}

/// Why a folder cannot serve as a speech model. Messages are shown to the user as written.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FolderError {
    #[error("There is no folder at {}.", .0.display())]
    NotFound(PathBuf),
    #[error("{} is not a folder.", .0.display())]
    NotAFolder(PathBuf),
    #[error("The folder {} cannot be read: {}.", .0.display(), .1)]
    Unreadable(PathBuf, std::io::ErrorKind),
    #[error("The model file {} cannot be read: {}.", .0.display(), .1)]
    UnreadableFile(PathBuf, std::io::ErrorKind),
    #[error("Missing required {}: {}", if .0.len() == 1 { "file" } else { "files" }, .0.join(", "))]
    Missing(Vec<String>),
}

/// Checks that `dir` is a readable folder holding every file of a model.
///
/// # Errors
/// The first problem found, with every missing file named when the folder is readable.
pub fn check_folder(dir: &Path) -> Result<(), FolderError> {
    let unreadable = |error: &std::io::Error| FolderError::Unreadable(dir.to_owned(), error.kind());
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(FolderError::NotAFolder(dir.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(FolderError::NotFound(dir.to_owned()));
        }
        Err(error) => return Err(unreadable(&error)),
    }
    std::fs::read_dir(dir).map_err(|error| unreadable(&error))?;
    let files = TransducerFiles::in_folder(dir);
    let missing: Vec<String> = [&files.encoder, &files.decoder, &files.joiner, &files.tokens]
        .iter()
        .filter(|file| !file.is_file())
        .filter_map(|file| file.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if !missing.is_empty() {
        return Err(FolderError::Missing(missing));
    }
    for file in [files.encoder, files.decoder, files.joiner, files.tokens] {
        std::fs::File::open(&file)
            .map_err(|error| FolderError::UnreadableFile(file, error.kind()))?;
    }
    Ok(())
}

/// Why a model could not be loaded. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum TransducerError {
    #[error("The speech model {file} could not be loaded: {reason}")]
    Load { file: String, reason: String },
    #[error("The speech model {file} cannot be used here: {reason}")]
    Unsupported { file: String, reason: String },
}

fn load_error(path: &Path, reason: &impl ToString) -> TransducerError {
    TransducerError::Load {
        file: path.display().to_string(),
        reason: reason.to_string(),
    }
}

fn unsupported(path: &Path, reason: &impl ToString) -> TransducerError {
    TransducerError::Unsupported {
        file: path.display().to_string(),
        reason: reason.to_string(),
    }
}

/// Sizes every transducer reports about itself.
struct Shape {
    subsampling: usize,
    hidden: usize,
    layers: usize,
    encoder_dim: usize,
    blank: usize,
    tdt: bool,
}

/// Sizes of the cache-aware encoder.
struct CacheShape {
    /// Feature frames per encoder call.
    window: usize,
    /// Feature frames the next call moves on by.
    shift: usize,
    channel: [usize; 3],
    time: [usize; 3],
}

/// Sizes of the buffered encoder, which reads the same window again at every step.
struct BufferedShape {
    mel: WindowMel,
    /// Feature frames per encoder call.
    window: usize,
    /// Samples the window moves on by at each step.
    step: usize,
    /// Samples in a full window.
    capacity: usize,
    /// Encoded frames per call.
    frames: usize,
    /// The first encoded frame decoded at each step: the newest ones wait for their look-ahead.
    begin: usize,
    /// Encoded frames decoded at each step.
    decoded: usize,
    /// Steps of digital silence after which the window no longer holds anything said before.
    flush_steps: usize,
    /// What the encoder makes of a window of digital silence, which fills the frames before the first audio.
    silence: Vec<f32>,
}

enum Layout {
    CacheAware(Arc<CacheShape>),
    /// With the language prompt, which is always the model's own detection.
    Buffered(Arc<BufferedShape>, i64),
    Tdt,
}

/// How an encoder takes its input; told apart by what it declares.
#[derive(Debug, PartialEq, Eq)]
enum EncoderKind {
    CacheAware,
    Buffered,
    Tdt,
}

impl EncoderKind {
    fn of(inputs: &[&str]) -> Option<Self> {
        match inputs {
            [_, _, _, _, _] => Some(Self::CacheAware),
            ["features", "length", "prompt_index"] => Some(Self::Buffered),
            ["audio_signal", "length"] => Some(Self::Tdt),
            _ => None,
        }
    }
}

fn tdt_dimension(
    encoder: Option<&[i64]>,
    joiner: Option<&[i64]>,
    blank: usize,
    subsampling: usize,
) -> Option<usize> {
    let [_, dimension, _] = encoder? else {
        return None;
    };
    let [_, _, _, logits] = joiner? else {
        return None;
    };
    if subsampling != 8 || usize::try_from(*logits).ok()? != blank + 1 + DURATIONS {
        return None;
    }
    usize::try_from(*dimension).ok().filter(|dim| *dim > 0)
}

struct Sessions {
    encoder: Session,
    decoder: Session,
    joiner: Session,
}

/// A loaded model; each recognition shares its weights.
pub struct Transducer {
    sessions: Arc<Mutex<Sessions>>,
    shape: Arc<Shape>,
    layout: Layout,
    tokens: Arc<Vec<String>>,
    phrases: Arc<PhraseTree>,
    device: Device,
    /// Where the GPU put the nodes of each session; absent on the CPU.
    placement: Option<Placement>,
}

fn builder(path: &Path, threads: usize) -> Result<SessionBuilder, TransducerError> {
    let environment = runtime::environment().map_err(|reason| load_error(path, &reason))?;
    let build = || -> ort::Result<SessionBuilder> {
        Ok(Session::builder(environment)?
            .with_intra_threads(threads)?
            .with_inter_threads(1)?)
    };
    build().map_err(|error| load_error(path, &error))
}

fn open_on_cpu(path: &Path, threads: usize) -> Result<Session, TransducerError> {
    builder(path, threads)?
        .commit_from_file(path)
        .map_err(|error| load_error(path, &error))
}

/// Only what `device::open` reports about the GPU session is the GPU's failure; the CPU takes over from it.
fn open_on_gpu(
    path: &Path,
    threads: usize,
    part: Part,
) -> Result<(Session, Nodes), Attempt<TransducerError>> {
    let builder = builder(path, threads)?;
    device::open(builder, path, part).map_err(Attempt::Fallback)
}

fn number(session: &Session, path: &Path, key: &str) -> Result<usize, TransducerError> {
    session
        .metadata()
        .ok()
        .and_then(|metadata| metadata.custom(key))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| unsupported(path, &format!("it does not say its {key}")))
}

impl Transducer {
    /// Loads the model on the CPU; this reads hundreds of megabytes, so call it off the async loop.
    ///
    /// # Errors
    /// Fails when a file cannot be read, is not a streaming `NeMo` transducer, or has features other
    /// than 128 mel bins.
    pub fn load(files: &TransducerFiles, phrases: &WordList) -> Result<Self, TransducerError> {
        Self::load_cpu(files, phrases, Fallback::CpuModel)
    }

    /// Like [`load`](Self::load), for a CPU that runs the model because of `reason`.
    ///
    /// # Errors
    /// As [`load`](Self::load).
    pub fn load_cpu(
        files: &TransducerFiles,
        phrases: &WordList,
        reason: Fallback,
    ) -> Result<Self, TransducerError> {
        let threads = threads();
        let sessions = Sessions {
            encoder: open_on_cpu(&files.encoder, threads)?,
            decoder: open_on_cpu(&files.decoder, 1)?,
            joiner: open_on_cpu(&files.joiner, 1)?,
        };
        Self::assemble(sessions, None, files, phrases, Device::Cpu(reason))
    }

    /// Like [`load`](Self::load), on `gpu`, the files being the fp16 export.
    ///
    /// # Errors
    /// [`Attempt::Fallback`] when a session does not start on the GPU or the GPU does not take the model, else as
    /// [`load`](Self::load).
    pub fn load_gpu(
        files: &TransducerFiles,
        phrases: &WordList,
        gpu: Gpu,
    ) -> Result<Self, Attempt<TransducerError>> {
        let threads = threads();
        let (encoder, encoder_nodes) = open_on_gpu(&files.encoder, threads, Part::Encoder)?;
        let (decoder, decoder_nodes) = open_on_gpu(&files.decoder, 1, Part::Decoder)?;
        let (joiner, joiner_nodes) = open_on_gpu(&files.joiner, 1, Part::Joiner)?;
        let placement = Placement {
            encoder: encoder_nodes,
            decoder: decoder_nodes,
            joiner: joiner_nodes,
        };
        Self::assemble(
            Sessions {
                encoder,
                decoder,
                joiner,
            },
            Some(placement),
            files,
            phrases,
            Device::Gpu(gpu),
        )
        .map_err(Attempt::Failed)
    }

    fn assemble(
        mut sessions: Sessions,
        placement: Option<Placement>,
        files: &TransducerFiles,
        phrases: &WordList,
        device: Device,
    ) -> Result<Self, TransducerError> {
        let path = &files.encoder;
        let count = |sessions: &Sessions, key| number(&sessions.encoder, path, key);
        if count(&sessions, "feat_dim")? != MEL_BINS {
            return Err(unsupported(path, &format!("it needs {MEL_BINS} mel bins")));
        }
        let names: Vec<&str> = sessions
            .encoder
            .inputs()
            .iter()
            .map(ort::value::Outlet::name)
            .collect();
        let kind = EncoderKind::of(&names).filter(|_| {
            sessions.decoder.inputs().len() == 4 && sessions.joiner.inputs().len() == 2
        });
        let Some(kind) = kind else {
            return Err(unsupported(path, &"it is not a streaming NeMo transducer"));
        };
        let tokens: Vec<String> = std::fs::read_to_string(&files.tokens)
            .map_err(|error| load_error(&files.tokens, &error))?
            .lines()
            .map(|line| {
                line.rsplit_once(' ')
                    .map_or(line, |(piece, _)| piece)
                    .to_owned()
            })
            .collect();
        let mut shape = Shape {
            subsampling: count(&sessions, "subsampling_factor")?,
            hidden: count(&sessions, "pred_hidden")?,
            layers: count(&sessions, "pred_rnn_layers")?,
            encoder_dim: 0,
            blank: count(&sessions, "vocab_size")?,
            tdt: kind == EncoderKind::Tdt,
        };
        let mismatch = || unsupported(&files.tokens, &"its tokens do not match the encoder");
        if tokens.len() != shape.blank + 1 || shape.subsampling == 0 {
            return Err(mismatch());
        }
        let layout = match kind {
            EncoderKind::Tdt => {
                shape.encoder_dim = tdt_shape(&sessions, &shape, path)?;
                Layout::Tdt
            }
            EncoderKind::CacheAware => {
                let cache = CacheShape {
                    window: count(&sessions, "window_size")?,
                    shift: count(&sessions, "chunk_shift")?,
                    channel: [
                        count(&sessions, "cache_last_channel_dim1")?,
                        count(&sessions, "cache_last_channel_dim2")?,
                        count(&sessions, "cache_last_channel_dim3")?,
                    ],
                    time: [
                        count(&sessions, "cache_last_time_dim1")?,
                        count(&sessions, "cache_last_time_dim2")?,
                        count(&sessions, "cache_last_time_dim3")?,
                    ],
                };
                if cache.shift > cache.window {
                    return Err(mismatch());
                }
                shape.encoder_dim = cache.channel[2];
                Layout::CacheAware(Arc::new(cache))
            }
            EncoderKind::Buffered => {
                let meta = [
                    count(&sessions, "window_size")?,
                    count(&sessions, "chunk_shift")?,
                    count(&sessions, "right_context")?,
                    count(&sessions, "prompt_auto")?,
                ];
                let (buffered, prompt) = buffered_shape(&mut sessions, &shape, path, meta)?;
                shape.encoder_dim = buffered.silence.len() / buffered.frames;
                Layout::Buffered(Arc::new(buffered), prompt)
            }
        };
        let phrases = PhraseTree::new(phrases, &tokens[..shape.blank]);
        Ok(Self {
            sessions: Arc::new(Mutex::new(sessions)),
            shape: Arc::new(shape),
            layout,
            tokens: Arc::new(tokens),
            phrases: Arc::new(phrases),
            device,
            placement,
        })
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| threads_for(n.get()))
}

/// Intra-op threads on a CPU with `logical` hardware threads: all but two, as 6 of 8 and 14 of 16 gave the lowest
/// lag measured (#380); the cap is the most threads measured.
fn threads_for(logical: usize) -> usize {
    logical.saturating_sub(2).clamp(4, 16).min(logical)
}

fn tdt_shape(sessions: &Sessions, shape: &Shape, path: &Path) -> Result<usize, TransducerError> {
    let encoder = sessions
        .encoder
        .outputs()
        .first()
        .and_then(|out| out.dtype().tensor_shape());
    let joiner = sessions
        .joiner
        .outputs()
        .first()
        .and_then(|out| out.dtype().tensor_shape());
    tdt_dimension(
        encoder.map(|dims| &dims[..]),
        joiner.map(|dims| &dims[..]),
        shape.blank,
        shape.subsampling,
    )
    .ok_or_else(|| {
        unsupported(
            path,
            &"its shapes do not declare an 80 ms TDT model with five durations",
        )
    })
}

/// Reads a buffered encoder's sizes and runs it once on digital silence.
fn buffered_shape(
    sessions: &mut Sessions,
    shape: &Shape,
    path: &Path,
    [window, shift, right, prompt]: [usize; 4],
) -> Result<(BufferedShape, i64), TransducerError> {
    let prompt = i64::try_from(prompt).unwrap_or(0);
    let sub = shape.subsampling;
    if shift == 0 || shift % sub != 0 || right % sub != 0 || window < shift + right {
        return Err(unsupported(path, &"its window does not fit its chunks"));
    }
    let quiet = encode_window(
        sessions,
        window,
        prompt,
        vec![0.0; MEL_BINS * window],
        window,
    )
    .map_err(|error| load_error(path, &error))?;
    let frames = quiet.stride;
    let decoded = shift / sub;
    let behind = decoded + right / sub + 1;
    if frames < behind || quiet.values.len() % frames != 0 {
        return Err(unsupported(path, &"its window does not fit its chunks"));
    }
    Ok((
        BufferedShape {
            mel: WindowMel::new(),
            window,
            step: shift * HOP,
            capacity: window * HOP,
            frames,
            begin: frames - behind,
            decoded,
            flush_steps: (window - shift - right) / shift,
            silence: quiet.values,
        },
        prompt,
    ))
}

impl SpeechModel for Transducer {
    type Stream = Recognition;

    fn device(&self) -> Device {
        self.device.clone()
    }

    fn placement(&self) -> Option<Placement> {
        self.placement
    }

    fn stream(&self) -> Recognition {
        let shape = Arc::clone(&self.shape);
        let endpoint_blanks = ENDPOINT_MS.div_ceil(shape.subsampling * FRAME_MS);
        let sessions = Arc::clone(&self.sessions);
        let phrases = Arc::clone(&self.phrases);
        let (decoder, lookahead) = match &self.layout {
            Layout::Tdt => {
                return Recognition::Revising(Revising::new(
                    Replayed::new(OnnxBackend {
                        sessions,
                        shape,
                        phrases,
                        window: 0,
                        prompt: 0,
                    }),
                    Arc::clone(&self.tokens),
                ));
            }
            Layout::CacheAware(cache) => (
                ModelStream::CacheAware(TransducerStream {
                    sessions,
                    carried: Carried::empty(cache),
                    phrases,
                    shape,
                    cache: Arc::clone(cache),
                    mel: LogMel::new(),
                    features: VecDeque::new(),
                }),
                cache.window * HOP + WINDOW_TAIL,
            ),
            Layout::Buffered(buffered, prompt) => {
                let backend = OnnxBackend {
                    sessions,
                    shape,
                    phrases,
                    window: buffered.window,
                    prompt: *prompt,
                };
                (
                    ModelStream::Buffered(Buffered::new(backend, Arc::clone(buffered))),
                    BUFFERED_LOOKAHEAD,
                )
            }
        };
        Recognition::Frames(Segments::new(
            decoder,
            Arc::clone(&self.tokens),
            endpoint_blanks,
            lookahead,
        ))
    }
}

/// A recognition stream for either append-only frames or revisable offline windows.
pub enum Recognition {
    Frames(Segments<ModelStream>),
    Revising(Revising<Replayed<OnnxBackend>>),
}

impl SpeechStream for Recognition {
    fn accept(&mut self, frame: &Frame) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        match self {
            Self::Frames(stream) => stream.accept(frame),
            Self::Revising(stream) => stream.accept(frame),
        }
    }
}

/// One recognition on a [`Transducer`], of whichever kind its encoder is.
#[derive(Clone)]
pub enum ModelStream {
    CacheAware(TransducerStream),
    Buffered(Buffered<OnnxBackend>),
}

impl FrameDecoder for ModelStream {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        match self {
            Self::CacheAware(stream) => stream.push(samples),
            Self::Buffered(stream) => stream.push(samples),
        }
    }

    fn lookahead(&self, silence: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        match self {
            Self::CacheAware(stream) => stream.lookahead(silence),
            Self::Buffered(stream) => stream.lookahead(silence),
        }
    }

    fn next_segment(&mut self) {
        match self {
            Self::CacheAware(stream) => stream.next_segment(),
            Self::Buffered(stream) => stream.next_segment(),
        }
    }
}

/// What the encoder carries from one chunk to the next.
#[derive(Clone, Default)]
struct EncoderCache {
    channel: Vec<f32>,
    time: Vec<f32>,
    length: i64,
}

impl EncoderCache {
    fn empty(shape: &CacheShape) -> Self {
        Self {
            channel: vec![0.0; shape.channel.iter().product()],
            time: vec![0.0; shape.time.iter().product()],
            length: 0,
        }
    }
}

/// What the model carries from chunk to chunk, and across the end of a stretch of speech.
#[derive(Clone)]
struct Carried {
    encoder: EncoderCache,
    /// `None` until the next decode, then the predictor primed with the blank token.
    predictor: Option<Predictor>,
    /// Where the text so far stands in the word list.
    phrases: TreeState,
}

impl Carried {
    fn empty(shape: &CacheShape) -> Self {
        Self {
            encoder: EncoderCache::empty(shape),
            predictor: None,
            phrases: TreeState::default(),
        }
    }

    /// The text starts afresh but the encoder keeps what it heard: without that context the first
    /// words after a pause are lost.
    fn next_segment(&mut self) {
        self.predictor = None;
        self.phrases = TreeState::default();
    }
}

/// The predictor's output for the last token and its recurrent state.
#[derive(Clone, Default)]
struct Predictor {
    output: Vec<f32>,
    states: [Vec<f32>; 2],
}

/// One recognition on a [`Transducer`].
#[derive(Clone)]
pub struct TransducerStream {
    sessions: Arc<Mutex<Sessions>>,
    shape: Arc<Shape>,
    cache: Arc<CacheShape>,
    phrases: Arc<PhraseTree>,
    mel: LogMel,
    features: VecDeque<MelFrame>,
    carried: Carried,
}

fn failed(reason: impl std::fmt::Display) -> RecognizerError {
    tracing::warn!(%reason, "local speech recognition failed");
    RecognizerError::said("Local speech recognition failed. Nothing was delivered.")
}

fn dims(shape: &[usize]) -> Vec<usize> {
    shape.to_vec()
}

impl TransducerStream {
    fn run_encoder(
        &mut self,
        sessions: &mut Sessions,
    ) -> Result<(Vec<f32>, usize), RecognizerError> {
        let window = self.cache.window;
        let mut audio = vec![0.0_f32; MEL_BINS * window];
        for (t, frame) in self.features.iter().take(window).enumerate() {
            for (m, value) in frame.iter().enumerate() {
                audio[m * window + t] = *value;
            }
        }
        self.features.drain(..self.cache.shift);
        let [c0, c1, c2] = self.cache.channel;
        let [t0, t1, t2] = self.cache.time;
        let inputs = ort::inputs![
            Tensor::from_array((dims(&[1, MEL_BINS, window]), audio)).map_err(failed)?,
            Tensor::from_array((dims(&[1]), vec![i64::try_from(window).unwrap_or(i64::MAX)]))
                .map_err(failed)?,
            Tensor::from_array((
                dims(&[1, c0, c1, c2]),
                std::mem::take(&mut self.carried.encoder.channel)
            ))
            .map_err(failed)?,
            Tensor::from_array((
                dims(&[1, t0, t1, t2]),
                std::mem::take(&mut self.carried.encoder.time)
            ))
            .map_err(failed)?,
            Tensor::from_array((dims(&[1]), vec![self.carried.encoder.length])).map_err(failed)?,
        ];
        let outputs = sessions.encoder.run(inputs).map_err(failed)?;
        let (shape, encoded) = outputs[0].try_extract_tensor::<f32>().map_err(failed)?;
        let frames = shape
            .get(2)
            .and_then(|n| usize::try_from(*n).ok())
            .ok_or_else(|| failed("the encoder returned an unexpected shape"))?;
        let encoded = encoded.to_vec();
        let (_, channel) = outputs[2].try_extract_tensor::<f32>().map_err(failed)?;
        self.carried.encoder.channel = channel.to_vec();
        let (_, time) = outputs[3].try_extract_tensor::<f32>().map_err(failed)?;
        self.carried.encoder.time = time.to_vec();
        let (_, length) = outputs[4].try_extract_tensor::<i64>().map_err(failed)?;
        self.carried.encoder.length = length.first().copied().unwrap_or(0);
        Ok((encoded, frames))
    }

    fn decode(
        &mut self,
        sessions: &mut Sessions,
        encoded: &[f32],
        frames: usize,
    ) -> Result<Vec<Vec<usize>>, RecognizerError> {
        decode(
            sessions,
            &self.shape,
            &self.phrases,
            &mut Decoding {
                predictor: &mut self.carried.predictor,
                phrases: &mut self.carried.phrases,
                skip: &mut 0,
            },
            Encoded {
                values: encoded,
                stride: frames,
            },
            0..frames,
        )
    }
}

/// What the greedy search carries from one call to the next.
pub struct Decoding<'a> {
    /// `None` until the next decode, then the predictor primed with the blank token.
    predictor: &'a mut Option<Predictor>,
    /// Where the text so far stands in the word list.
    phrases: &'a mut TreeState,
    skip: &'a mut usize,
}

/// Encoder output, `stride` frames to a channel.
#[derive(Clone, Copy)]
struct Encoded<'a> {
    values: &'a [f32],
    stride: usize,
}

/// Greedy search over encoder frames `range`, as sherpa-onnx does, favouring the word list.
fn decode(
    sessions: &mut Sessions,
    shape: &Shape,
    tree: &PhraseTree,
    carried: &mut Decoding<'_>,
    encoded: Encoded<'_>,
    range: std::ops::Range<usize>,
) -> Result<Vec<Vec<usize>>, RecognizerError> {
    let predictor = if let Some(predictor) = carried.predictor.take() {
        predictor
    } else {
        let zeros = || vec![0.0; shape.layers * shape.hidden];
        run_predictor(sessions, shape, shape.blank, [zeros(), zeros()])?
    };
    let mut joiner = OnnxJoiner {
        sessions,
        shape,
        encoded,
        first: range.start,
        predictor,
    };
    let emitted = if shape.tdt {
        let (next, emitted) = tdt_greedy(
            joiner,
            tree,
            carried.phrases,
            shape.blank,
            range.len(),
            carried.skip,
        )?;
        joiner = next;
        emitted
    } else {
        greedy(&mut joiner, tree, carried.phrases, range.len())?
    };
    *carried.predictor = Some(joiner.predictor);
    Ok(emitted)
}

fn run_predictor(
    sessions: &mut Sessions,
    shape: &Shape,
    token: usize,
    states: [Vec<f32>; 2],
) -> Result<Predictor, RecognizerError> {
    let (hidden, layers) = (shape.hidden, shape.layers);
    let [first, second] = states;
    let inputs = ort::inputs![
        Tensor::from_array((dims(&[1, 1]), vec![i32::try_from(token).unwrap_or(0)]))
            .map_err(failed)?,
        Tensor::from_array((dims(&[1]), vec![1_i32])).map_err(failed)?,
        Tensor::from_array((dims(&[layers, 1, hidden]), first)).map_err(failed)?,
        Tensor::from_array((dims(&[layers, 1, hidden]), second)).map_err(failed)?,
    ];
    let outputs = sessions.decoder.run(inputs).map_err(failed)?;
    let take = |index: usize| -> Result<Vec<f32>, RecognizerError> {
        let (_, values) = outputs[index].try_extract_tensor::<f32>().map_err(failed)?;
        Ok(values.to_vec())
    };
    Ok(Predictor {
        output: take(0)?,
        states: [take(2)?, take(3)?],
    })
}

/// The ONNX joiner and predictor over one chunk's encoder output.
struct OnnxJoiner<'a> {
    sessions: &'a mut Sessions,
    shape: &'a Shape,
    encoded: Encoded<'a>,
    /// The encoder frame that [`Joiner::logits`] calls frame 0.
    first: usize,
    predictor: Predictor,
}

impl Joiner for OnnxJoiner<'_> {
    fn logits(&mut self, frame: usize) -> Result<Vec<f32>, RecognizerError> {
        let dim = self.shape.encoder_dim;
        let column: Vec<f32> = (0..dim)
            .map(|c| self.encoded.values[c * self.encoded.stride + self.first + frame])
            .collect();
        let inputs = ort::inputs![
            Tensor::from_array((dims(&[1, dim, 1]), column)).map_err(failed)?,
            Tensor::from_array((
                dims(&[1, self.shape.hidden, 1]),
                self.predictor.output.clone()
            ))
            .map_err(failed)?,
        ];
        let outputs = self.sessions.joiner.run(inputs).map_err(failed)?;
        let (_, logits) = outputs[0].try_extract_tensor::<f32>().map_err(failed)?;
        Ok(logits.to_vec())
    }

    fn advance(&mut self, token: usize) -> Result<(), RecognizerError> {
        let states = std::mem::take(&mut self.predictor.states);
        self.predictor = run_predictor(self.sessions, self.shape, token, states)?;
        Ok(())
    }
}

impl FrameDecoder for TransducerStream {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        let mut mel = Vec::new();
        self.mel.push(samples, &mut mel);
        self.features.extend(mel);
        let sessions = Arc::clone(&self.sessions);
        let mut emitted = Vec::new();
        while self.features.len() >= self.cache.window {
            let mut sessions = sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let (encoded, frames) = self.run_encoder(&mut sessions)?;
            emitted.extend(self.decode(&mut sessions, &encoded, frames)?);
        }
        Ok(emitted)
    }

    fn lookahead(&self, silence: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        self.clone().push(silence)
    }

    fn next_segment(&mut self) {
        self.carried.next_segment();
    }
}

/// What a buffered encoder made of one window.
pub struct Encoding {
    values: Vec<f32>,
    /// Frames to a channel in `values`.
    stride: usize,
    /// Frames, at the start of each channel, that come from speech.
    count: usize,
}

/// Runs the buffered encoder on `features` (mel bin major, `window` frames), `valid` of them speech.
fn encode_window(
    sessions: &mut Sessions,
    window: usize,
    prompt: i64,
    features: Vec<f32>,
    valid: usize,
) -> Result<Encoding, RecognizerError> {
    let inputs = ort::inputs![
        Tensor::from_array((dims(&[1, MEL_BINS, window]), features)).map_err(failed)?,
        Tensor::from_array((dims(&[1]), vec![i64::try_from(valid).unwrap_or(i64::MAX)]))
            .map_err(failed)?,
        Tensor::from_array((dims(&[1]), vec![prompt])).map_err(failed)?,
    ];
    let outputs = sessions.encoder.run(inputs).map_err(failed)?;
    let (shape, values) = outputs[0].try_extract_tensor::<f32>().map_err(failed)?;
    let stride = shape
        .get(2)
        .and_then(|n| usize::try_from(*n).ok())
        .ok_or_else(|| failed("the encoder returned an unexpected shape"))?;
    let (_, length) = outputs[1].try_extract_tensor::<i64>().map_err(failed)?;
    let count = length
        .first()
        .and_then(|n| usize::try_from(*n).ok())
        .unwrap_or(0)
        .min(stride);
    Ok(Encoding {
        values: values.to_vec(),
        stride,
        count,
    })
}

/// The two things a buffered stream needs from the model; ONNX Runtime provides them, and tests a script.
pub trait Backend: Clone + Send + 'static {
    /// # Errors
    /// Fails when the model cannot continue.
    fn encode(&mut self, features: Vec<f32>, valid: usize) -> Result<Encoding, RecognizerError>;

    /// The tokens of encoder frames `range` of `encoded`, `stride` frames to a channel.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn decode(
        &mut self,
        encoded: &[f32],
        stride: usize,
        range: std::ops::Range<usize>,
        carried: &mut Decoding<'_>,
    ) -> Result<Vec<Vec<usize>>, RecognizerError>;
}

/// The buffered model on ONNX Runtime.
#[derive(Clone)]
pub struct OnnxBackend {
    sessions: Arc<Mutex<Sessions>>,
    shape: Arc<Shape>,
    phrases: Arc<PhraseTree>,
    window: usize,
    prompt: i64,
}

impl Backend for OnnxBackend {
    fn encode(&mut self, features: Vec<f32>, valid: usize) -> Result<Encoding, RecognizerError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if self.shape.tdt {
            encode_offline(&mut sessions, features, valid)
        } else {
            encode_window(&mut sessions, self.window, self.prompt, features, valid)
        }
    }

    fn decode(
        &mut self,
        encoded: &[f32],
        stride: usize,
        range: std::ops::Range<usize>,
        carried: &mut Decoding<'_>,
    ) -> Result<Vec<Vec<usize>>, RecognizerError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        decode(
            &mut sessions,
            &self.shape,
            &self.phrases,
            carried,
            Encoded {
                values: encoded,
                stride,
            },
            range,
        )
    }
}

fn encode_offline(
    sessions: &mut Sessions,
    features: Vec<f32>,
    window: usize,
) -> Result<Encoding, RecognizerError> {
    let inputs = ort::inputs![
        Tensor::from_array((dims(&[1, MEL_BINS, window]), features)).map_err(failed)?,
        Tensor::from_array((dims(&[1]), vec![i64::try_from(window).unwrap_or(i64::MAX)]))
            .map_err(failed)?,
    ];
    let outputs = sessions.encoder.run(inputs).map_err(failed)?;
    let (shape, values) = outputs[0].try_extract_tensor::<f32>().map_err(failed)?;
    let stride = shape
        .get(2)
        .and_then(|n| usize::try_from(*n).ok())
        .ok_or_else(|| failed("the encoder returned an unexpected shape"))?;
    let (_, length) = outputs[1].try_extract_tensor::<i64>().map_err(failed)?;
    let count = length
        .first()
        .and_then(|n| usize::try_from(*n).ok())
        .unwrap_or(0)
        .min(stride);
    Ok(Encoding {
        values: values.to_vec(),
        stride,
        count,
    })
}

#[derive(Clone, Default)]
struct ReplayState {
    predictor: Option<Predictor>,
    tree: TreeState,
    skip: usize,
}

impl ReplayState {
    fn decoding(&mut self) -> Decoding<'_> {
        Decoding {
            predictor: &mut self.predictor,
            phrases: &mut self.tree,
            skip: &mut self.skip,
        }
    }
}

/// Replays the tentative suffix with left context; a final replays the complete speech stretch.
pub struct Replayed<B> {
    backend: B,
    mel: WindowMel,
    prefix: Vec<Vec<usize>>,
    anchor: ReplayState,
}

impl<B: Backend> Replayed<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            mel: WindowMel::new(),
            prefix: Vec::new(),
            anchor: ReplayState::default(),
        }
    }

    fn hear(&mut self, audio: &[f32]) -> Result<Encoding, RecognizerError> {
        // Quiet speech otherwise falls below NeMo's additive log-energy guard.
        let audio: Vec<f32> = audio.iter().map(|sample| sample * 16.0).collect();
        let mel = self.mel.features(&audio);
        let window = mel.len();
        let mut features = vec![0.0; MEL_BINS * window];
        for (t, frame) in mel.iter().enumerate() {
            for (m, value) in frame.iter().enumerate() {
                features[m * window + t] = *value;
            }
        }
        self.backend.encode(features, window)
    }
}

impl<B: Backend> SnapshotDecoder for Replayed<B> {
    fn snapshot(&mut self, audio: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        const FRAME: usize = 1280;
        const LEFT: usize = 8;
        const WINDOW: usize = 24;
        const REVISE: usize = 16;
        let start = self.prefix.len();
        let offset = start.saturating_sub(LEFT);
        let heard = self.hear(&audio[offset * FRAME..])?;
        let from = start - offset;
        let mut carried = self.anchor.clone();
        let mut all = self.prefix.clone();
        let cut = if heard.count - from >= WINDOW {
            heard.count - REVISE
        } else {
            from
        };
        if cut > from {
            all.extend(self.backend.decode(
                &heard.values,
                heard.stride,
                from..cut,
                &mut carried.decoding(),
            )?);
            if all.iter().any(|tokens| !tokens.is_empty()) {
                self.anchor = carried.clone();
                self.prefix.clone_from(&all);
            }
        }
        all.extend(self.backend.decode(
            &heard.values,
            heard.stride,
            cut..heard.count,
            &mut carried.decoding(),
        )?);
        Ok(all)
    }

    fn finish(&mut self, audio: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        let mut padded = audio.to_vec();
        padded.resize(padded.len() + 12_800, 0.0);
        let heard = self.hear(&padded)?;
        let count = audio.len().div_ceil(1280).min(heard.count);
        self.backend.decode(
            &heard.values,
            heard.stride,
            0..count,
            &mut ReplayState::default().decoding(),
        )
    }

    fn next_segment(&mut self) {
        self.prefix.clear();
        self.anchor = ReplayState::default();
    }
}

/// One recognition on a buffered encoder: every step it hears the last few seconds again and
/// decodes only the frames that now have their look-ahead.
#[derive(Clone)]
pub struct Buffered<B> {
    backend: B,
    shape: Arc<BufferedShape>,
    /// The last window of audio taken in; the newest sample is last.
    audio: Vec<f32>,
    /// Samples received that do not make a whole step yet.
    waiting: Vec<f32>,
    predictor: Option<Predictor>,
    tree: TreeState,
    /// Steps in a row whose new samples were all exactly zero.
    silent_steps: usize,
    /// A stretch of speech ended since the predictor was last made fresh.
    stretch_ended: bool,
}

impl<B: Backend> Buffered<B> {
    fn new(backend: B, shape: Arc<BufferedShape>) -> Self {
        Self {
            backend,
            shape,
            audio: Vec::new(),
            waiting: Vec::new(),
            predictor: None,
            tree: TreeState::default(),
            silent_steps: 0,
            stretch_ended: false,
        }
    }

    /// Takes in `chunk`, hears the window and decodes encoder frames `from` to `to`.
    fn step(
        &mut self,
        chunk: &[f32],
        from: usize,
        to: usize,
    ) -> Result<Vec<Vec<usize>>, RecognizerError> {
        let shape = Arc::clone(&self.shape);
        self.audio.extend_from_slice(chunk);
        let excess = self.audio.len().saturating_sub(shape.capacity);
        self.audio.drain(..excess);
        let mel = shape.mel.features(&self.audio);
        let valid = mel.len().min(shape.window);
        let mut features = vec![0.0; MEL_BINS * shape.window];
        for (t, frame) in mel.iter().take(valid).enumerate() {
            for (m, value) in frame.iter().enumerate() {
                features[m * shape.window + t] = *value;
            }
        }
        let heard = self.backend.encode(features, valid)?;
        let mut full = shape.silence.clone();
        let count = heard.count.min(shape.frames);
        let channels = shape.silence.len() / shape.frames;
        for c in 0..channels {
            let target = c * shape.frames + shape.frames - count;
            let source = c * heard.stride;
            full[target..target + count].copy_from_slice(&heard.values[source..source + count]);
        }
        let emitted = self.backend.decode(
            &full,
            shape.frames,
            from..to,
            &mut Decoding {
                predictor: &mut self.predictor,
                phrases: &mut self.tree,
                skip: &mut 0,
            },
        )?;
        self.silent_steps = if chunk.iter().any(|sample| *sample != 0.0) {
            0
        } else {
            self.silent_steps + 1
        };
        Ok(emitted)
    }

    /// Hears what is left over at the end of the audio, decoding every frame down to the newest.
    fn finish(&mut self) -> Result<Vec<Vec<usize>>, RecognizerError> {
        if self.audio.is_empty() && self.waiting.is_empty() {
            return Ok(Vec::new());
        }
        let chunk = std::mem::take(&mut self.waiting);
        let shape = Arc::clone(&self.shape);
        // Without new samples the last step already decoded its first frames.
        let from = shape.begin + if chunk.is_empty() { shape.decoded } else { 0 };
        self.step(&chunk, from, shape.frames)
    }
}

impl<B: Backend> FrameDecoder for Buffered<B> {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        self.waiting.extend_from_slice(samples);
        let shape = Arc::clone(&self.shape);
        let mut emitted = Vec::new();
        while self.waiting.len() >= shape.step {
            let chunk: Vec<f32> = self.waiting.drain(..shape.step).collect();
            emitted.extend(self.step(&chunk, shape.begin, shape.begin + shape.decoded)?);
            // Long digital silence has pushed everything said before out of the window, and a
            // predictor that still remembers it loses the first words of the next stretch.
            if self.stretch_ended && self.silent_steps >= shape.flush_steps {
                self.predictor = None;
                self.silent_steps = 0;
                self.stretch_ended = false;
            }
        }
        Ok(emitted)
    }

    fn lookahead(&self, silence: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        let mut ahead = self.clone();
        let mut found = ahead.push(silence)?;
        found.extend(ahead.finish()?);
        Ok(found)
    }

    fn next_segment(&mut self) {
        self.tree = TreeState::default();
        self.stretch_ended = true;
    }
}

/// A catalog model, downloaded on first use, or a folder holding the same four files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeechChoice {
    Catalog(CatalogSpeech),
    Folder(PathBuf),
}

impl SpeechChoice {
    /// What metrics and messages call the model.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Catalog(catalog) => catalog.name().to_owned(),
            Self::Folder(path) => path.display().to_string(),
        }
    }
}

/// Fetches the chosen model into the store and loads it.
pub struct TransducerSource {
    store: ModelStore,
    choice: SpeechChoice,
    phrases: WordList,
    adapters: Adapters,
}

impl TransducerSource {
    /// `adapters` takes the result of the adapter check that each load makes.
    #[must_use]
    pub fn new(
        store: ModelStore,
        choice: SpeechChoice,
        phrases: &WordList,
        adapters: Adapters,
    ) -> Self {
        Self {
            store,
            choice,
            phrases: phrases.clone(),
            adapters,
        }
    }

    /// Where the load will put the model, checking the adapter now if the model can use a GPU.
    async fn plan(&self) -> Device {
        let uses_gpu = matches!(&self.choice, SpeechChoice::Catalog(catalog) if catalog.uses_gpu());
        let adapters = self.adapters.clone();
        tokio::task::spawn_blocking(move || Device::plan(uses_gpu, || adapters.check()))
            .await
            .unwrap_or(Device::Cpu(Fallback::NoUsableGpu))
    }
}

/// Runs a blocking load of the model, which reads hundreds of megabytes.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RecognizerError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| RecognizerError::said("The speech model could not be loaded."))
}

impl Provider for TransducerSource {
    type Model = Transducer;
    type Files = TransducerFiles;
    type Error = RecognizerError;

    async fn cached(&self, export: Export) -> bool {
        match &self.choice {
            SpeechChoice::Catalog(catalog) => {
                self.store.is_cached(&catalog.assets_of(export)).await
            }
            SpeechChoice::Folder(_) => true,
        }
    }

    async fn fetch(
        &self,
        export: Export,
        progress: impl FnMut(SpeechProgress) + Send,
    ) -> Result<TransducerFiles, RecognizerError> {
        match &self.choice {
            SpeechChoice::Catalog(catalog) => {
                let assets = catalog.assets_of(export);
                self.store
                    .fetch_speech(&assets, progress)
                    .await
                    .map_err(RecognizerError::from)?;
                Ok(TransducerFiles {
                    encoder: self.store.path(&assets.encoder),
                    decoder: self.store.path(&assets.decoder),
                    joiner: self.store.path(&assets.joiner),
                    tokens: self.store.path(&assets.tokens),
                })
            }
            SpeechChoice::Folder(dir) => Ok(TransducerFiles::in_folder(dir)),
        }
    }

    async fn open_gpu(
        &self,
        gpu: Gpu,
        files: TransducerFiles,
    ) -> Result<Transducer, Attempt<RecognizerError>> {
        let phrases = self.phrases.clone();
        blocking(move || Transducer::load_gpu(&files, &phrases, gpu))
            .await?
            .map_err(|attempt| match attempt {
                Attempt::Fallback(reason) => Attempt::Fallback(reason),
                Attempt::Failed(error) => Attempt::Failed(RecognizerError::said(error.to_string())),
            })
    }

    async fn open_cpu(
        &self,
        reason: Fallback,
        files: TransducerFiles,
    ) -> Result<Transducer, RecognizerError> {
        let phrases = self.phrases.clone();
        blocking(move || Transducer::load_cpu(&files, &phrases, reason))
            .await?
            .map_err(|error| RecognizerError::said(error.to_string()))
    }
}

impl ModelSource for TransducerSource {
    type Model = Transducer;

    async fn load(&self, step: impl Fn(Step) + Send + Sync) -> Result<Transducer, RecognizerError> {
        device::load(self.plan().await, self, step).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thalvi-folder-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn threads_leave_two_logical_threads_free_between_four_and_sixteen() {
        for (logical, threads) in [
            (1, 1),
            (2, 2),
            (4, 4),
            (6, 4),
            (8, 6),
            (16, 14),
            (18, 16),
            (32, 16),
        ] {
            assert_eq!(threads_for(logical), threads, "{logical} logical threads");
        }
    }

    const FILES: [&str; 4] = [
        "encoder.int8.onnx",
        "decoder.int8.onnx",
        "joiner.int8.onnx",
        "tokens.txt",
    ];

    fn folder_with(tag: &str, present: &[&str]) -> PathBuf {
        let dir = scratch(tag);
        for name in present {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        dir
    }

    fn missing(names: &[&str]) -> Result<(), FolderError> {
        Err(FolderError::Missing(
            names.iter().map(|name| (*name).to_owned()).collect(),
        ))
    }

    #[test]
    fn a_folder_is_a_model_only_when_all_four_files_are_in_it() {
        let cases: [(&str, &[&str], Result<(), FolderError>); 7] = [
            ("all", &FILES, Ok(())),
            ("no-encoder", &FILES[1..], missing(&["encoder.int8.onnx"])),
            (
                "no-decoder",
                &[FILES[0], FILES[2], FILES[3]],
                missing(&["decoder.int8.onnx"]),
            ),
            (
                "no-joiner",
                &[FILES[0], FILES[1], FILES[3]],
                missing(&["joiner.int8.onnx"]),
            ),
            ("no-tokens", &FILES[..3], missing(&["tokens.txt"])),
            (
                "two-gone",
                &[FILES[1], FILES[2]],
                missing(&["encoder.int8.onnx", "tokens.txt"]),
            ),
            (
                "empty",
                &[],
                missing(&[FILES[0], FILES[1], FILES[2], FILES[3]]),
            ),
        ];
        for (tag, present, expected) in cases {
            let dir = folder_with(tag, present);
            assert_eq!(check_folder(&dir), expected, "{tag}");
        }
    }

    #[test]
    fn a_missing_file_is_named_in_the_message() {
        let dir = folder_with("message", &FILES[..3]);
        let one = check_folder(&dir).unwrap_err().to_string();
        assert_eq!(one, "Missing required file: tokens.txt");
        let dir = folder_with("messages", &FILES[1..3]);
        let two = check_folder(&dir).unwrap_err().to_string();
        assert_eq!(two, "Missing required files: encoder.int8.onnx, tokens.txt");
    }

    #[test]
    fn a_directory_in_place_of_a_file_counts_as_missing() {
        let dir = folder_with("dir-as-file", &FILES[1..]);
        std::fs::create_dir(dir.join(FILES[0])).unwrap();
        assert_eq!(check_folder(&dir), missing(&[FILES[0]]));
    }

    #[test]
    fn a_path_that_is_not_a_readable_folder_is_refused_with_its_own_reason() {
        let dir = scratch("not-folder");
        let absent = dir.join("absent");
        assert_eq!(
            check_folder(&absent),
            Err(FolderError::NotFound(absent.clone()))
        );
        assert_eq!(
            FolderError::NotFound(absent.clone()).to_string(),
            format!("There is no folder at {}.", absent.display())
        );
        let file = dir.join("model.txt");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            check_folder(&file),
            Err(FolderError::NotAFolder(file.clone()))
        );
        assert_eq!(
            FolderError::NotAFolder(file.clone()).to_string(),
            format!("{} is not a folder.", file.display())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_model_file_without_read_permission_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder_with("locked-file", &FILES);
        let encoder = dir.join("encoder.int8.onnx");
        std::fs::set_permissions(&encoder, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = check_folder(&dir);
        std::fs::set_permissions(&encoder, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            result,
            Err(FolderError::UnreadableFile(
                encoder,
                std::io::ErrorKind::PermissionDenied
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_without_read_permission_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder_with("locked", &FILES);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = check_folder(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            result,
            Err(FolderError::Unreadable(
                dir.clone(),
                std::io::ErrorKind::PermissionDenied
            ))
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            format!(
                "The folder {} cannot be read: permission denied.",
                dir.display()
            )
        );
    }

    #[test]
    fn the_files_a_loaded_folder_names_are_the_ones_checked() {
        let files = TransducerFiles::in_folder(Path::new("/m"));
        let names: Vec<_> = [&files.encoder, &files.decoder, &files.joiner, &files.tokens]
            .iter()
            .map(|path| path.strip_prefix("/m").unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, FILES);
    }

    #[test]
    fn a_new_stretch_of_speech_starts_the_text_afresh_but_keeps_the_encoder_context() {
        let mut carried = Carried {
            encoder: EncoderCache {
                channel: vec![1.0, 2.0],
                time: vec![3.0],
                length: 7,
            },
            predictor: Some(Predictor {
                output: vec![0.5],
                states: [vec![0.1], vec![0.2]],
            }),
            phrases: TreeState::default(),
        };
        carried.next_segment();
        assert!(carried.predictor.is_none());
        assert_eq!(carried.encoder.channel, [1.0, 2.0]);
        assert_eq!(carried.encoder.time, [3.0]);
        assert_eq!(carried.encoder.length, 7);
    }

    /// A buffered encoder that records what it was given and answers like the real one in shape.
    #[derive(Clone, Default)]
    struct Fake {
        seen: Arc<Mutex<Seen>>,
    }

    #[derive(Default)]
    struct Seen {
        /// Per encode: speech frames given, and the first mel bin over the whole window.
        encodes: Vec<(usize, Vec<f32>)>,
        decodes: Vec<Decode>,
    }

    struct Decode {
        range: std::ops::Range<usize>,
        /// The predictor had to start afresh.
        fresh: bool,
        /// The first channel of the 77 encoder frames.
        channel: Vec<f32>,
    }

    const FRAMES: usize = 77;
    const SPEECH: f32 = 7.0;
    const SILENCE: f32 = -1.0;

    impl Backend for Fake {
        fn encode(
            &mut self,
            features: Vec<f32>,
            valid: usize,
        ) -> Result<Encoding, RecognizerError> {
            self.seen
                .lock()
                .unwrap()
                .encodes
                .push((valid, features[..616].to_vec()));
            Ok(Encoding {
                values: vec![SPEECH; 2 * FRAMES],
                stride: FRAMES,
                count: valid.div_ceil(8),
            })
        }

        fn decode(
            &mut self,
            encoded: &[f32],
            stride: usize,
            range: std::ops::Range<usize>,
            carried: &mut Decoding<'_>,
        ) -> Result<Vec<Vec<usize>>, RecognizerError> {
            assert_eq!(stride, FRAMES);
            self.seen.lock().unwrap().decodes.push(Decode {
                range: range.clone(),
                fresh: carried.predictor.is_none(),
                channel: encoded[..FRAMES].to_vec(),
            });
            *carried.predictor = Some(Predictor::default());
            Ok(range.map(|frame| vec![frame]).collect())
        }
    }

    fn buffered() -> (Buffered<Fake>, Arc<Mutex<Seen>>) {
        let shape = BufferedShape {
            mel: WindowMel::new(),
            window: 616,
            step: 2_560,
            capacity: 98_560,
            frames: FRAMES,
            begin: 69,
            decoded: 2,
            flush_steps: 35,
            silence: vec![SILENCE; 2 * FRAMES],
        };
        let fake = Fake::default();
        let seen = Arc::clone(&fake.seen);
        (Buffered::new(fake, Arc::new(shape)), seen)
    }

    /// Sound that is never exactly zero.
    fn sound(len: usize, from: usize) -> Vec<f32> {
        (from..from + len)
            .map(|i| f32::from(u8::try_from(i * 37 % 200).unwrap_or(0) + 1) / 1_000.0)
            .collect()
    }

    const STEP: usize = 2_560;

    #[test]
    fn a_window_not_yet_full_is_heard_on_its_own_and_sits_at_the_end_of_the_silence_before_it() {
        let (mut stream, seen) = buffered();
        let mut emitted = Vec::new();
        for step in 1..=3 {
            emitted.extend(stream.push(&sound(STEP, step * STEP)).unwrap());
        }
        let seen = seen.lock().unwrap();
        assert_eq!(emitted, [[69], [70], [69], [70], [69], [70]]);
        for (step, (encode, decode)) in seen.encodes.iter().zip(&seen.decodes).enumerate() {
            let valid = (step + 1) * 16;
            assert_eq!(encode.0, valid);
            let (speech, quiet) = encode.1.split_at(valid);
            assert!(quiet.iter().all(|value| *value == 0.0), "step {step}");
            let count = |n: usize| f32::from(u16::try_from(n).unwrap());
            let mean = speech.iter().sum::<f32>() / count(valid);
            let spread = speech.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / count(valid - 1);
            assert!(
                mean.abs() < 1e-3 && (spread - 1.0).abs() < 1e-2,
                "step {step}"
            );
            let heard = valid / 8;
            assert_eq!(decode.range, 69..71);
            assert!(
                decode.channel[..FRAMES - heard]
                    .iter()
                    .all(|v| *v == SILENCE)
            );
            assert!(
                decode.channel[FRAMES - heard..]
                    .iter()
                    .all(|v| *v == SPEECH)
            );
        }
    }

    #[test]
    fn a_full_window_holds_the_last_six_seconds_and_moves_on_by_one_step() {
        let (mut stream, seen) = buffered();
        let audio = sound(41 * STEP + 1_280, 0);
        let mut at = 0;
        while at + 1_280 <= audio.len() {
            stream.push(&audio[at..at + 1_280]).unwrap();
            at += 1_280;
        }
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.encodes.len(),
            41,
            "a step for every 2560 samples, none for the 1280 left"
        );
        let (valid, row) = seen.encodes.last().unwrap();
        assert_eq!(*valid, 616);
        let window = &audio[41 * STEP - 98_560..41 * STEP];
        let expected: Vec<f32> = WindowMel::new()
            .features(window)
            .iter()
            .map(|frame| frame[0])
            .collect();
        assert_eq!(row, &expected);
    }

    #[test]
    fn the_end_of_audio_decodes_down_to_the_newest_frame_and_leaves_the_stream_alone() {
        let (mut stream, seen) = buffered();
        stream.push(&sound(STEP + 1_280, 0)).unwrap();
        let ahead = stream.lookahead(&vec![0.0; 4_400]).unwrap();
        // 1280 waiting + 4400 silence: two whole steps, then 560 samples for the end.
        let frames =
            |range: std::ops::Range<usize>| -> Vec<Vec<usize>> { range.map(|f| vec![f]).collect() };
        let expected: Vec<Vec<usize>> = [frames(69..71), frames(69..71), frames(69..77)].concat();
        assert_eq!(ahead, expected);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.encodes.len(), 4);
        }
        stream.push(&sound(1_280, 9)).unwrap();
        let seen = seen.lock().unwrap();
        let (valid, _) = seen.encodes.last().unwrap();
        assert_eq!(*valid, 32, "the look-ahead did not move the stream");
    }

    #[test]
    fn the_end_of_audio_on_a_whole_step_decodes_only_what_that_step_has_not() {
        let (mut stream, _) = buffered();
        stream.push(&sound(STEP, 0)).unwrap();
        assert_eq!(
            stream.lookahead(&[]).unwrap(),
            (71..77).map(|f| vec![f]).collect::<Vec<_>>()
        );
        assert_eq!(
            buffered().0.lookahead(&[]).unwrap(),
            Vec::<Vec<usize>>::new()
        );
    }

    #[test]
    fn the_predictor_survives_stretches_and_starts_afresh_only_after_a_final_and_5_6_s_of_digital_silence()
     {
        // S: a step of sound, `.`: a step of digital silence, `,`: silence but for one sample.
        // The `|` marks a final. The steps whose decode started with a fresh predictor are listed.
        let cases: [(&str, &str, &[usize]); 6] = [
            ("kept across a final", "S|S", &[0]),
            (
                "not before 35 silent steps",
                "S|...................................",
                &[0],
            ),
            (
                "after 35 silent steps",
                "S|....................................",
                &[0, 36],
            ),
            (
                "not without a final",
                ".....................................",
                &[0],
            ),
            (
                "sound restarts the count",
                "S|..................................,..................................",
                &[0],
            ),
            (
                "not after the reset until another final",
                "S|....................................|...",
                &[0, 36],
            ),
        ];
        for (name, script, fresh) in cases {
            let (mut stream, seen) = buffered();
            for (at, mark) in script.chars().enumerate() {
                let mut chunk = vec![0.0; STEP];
                match mark {
                    'S' => chunk = sound(STEP, at),
                    ',' => chunk[7] = 1e-6,
                    '|' => {
                        stream.next_segment();
                        continue;
                    }
                    _ => {}
                }
                stream.push(&chunk).unwrap();
            }
            let seen = seen.lock().unwrap();
            let got: Vec<usize> = seen
                .decodes
                .iter()
                .enumerate()
                .filter_map(|(step, decode)| decode.fresh.then_some(step))
                .collect();
            assert_eq!(got, fresh, "{name}");
        }
    }

    #[test]
    fn an_encoder_is_told_by_what_it_declares() {
        let cases: [(&[&str], Option<EncoderKind>); 5] = [
            (
                &[
                    "audio_signal",
                    "length",
                    "cache_last_channel",
                    "cache_last_time",
                    "cache_last_channel_len",
                ],
                Some(EncoderKind::CacheAware),
            ),
            (
                &["features", "length", "prompt_index"],
                Some(EncoderKind::Buffered),
            ),
            (&["features", "length"], None),
            (&["features", "length", "language"], None),
            (&[], None),
        ];
        for (inputs, kind) in cases {
            assert_eq!(EncoderKind::of(inputs), kind, "{inputs:?}");
        }
    }

    #[test]
    fn a_tdt_encoder_is_recognized_without_a_catalog_name() {
        for (inputs, expected) in [
            (&["audio_signal", "length"][..], Some(EncoderKind::Tdt)),
            (&["features", "length"][..], None),
            (&["audio_signal", "language"][..], None),
        ] {
            assert_eq!(EncoderKind::of(inputs), expected);
        }
        let encoder = [1, 1024, -1];
        let tdt = [-1, -1, -1, 8198];
        let plain = [-1, -1, -1, 8193];
        for (encoder, joiner, subsampling, expected) in [
            (Some(&encoder[..]), Some(&tdt[..]), 8, Some(1024)),
            (Some(&encoder[..]), Some(&plain[..]), 8, None),
            (Some(&encoder[..]), Some(&tdt[..]), 4, None),
            (Some(&[1, 0, -1][..]), Some(&tdt[..]), 8, None),
            (Some(&[1, -1, -1][..]), Some(&tdt[..]), 8, None),
            (None, Some(&tdt[..]), 8, None),
            (Some(&encoder[..]), None, 8, None),
        ] {
            assert_eq!(tdt_dimension(encoder, joiner, 8192, subsampling), expected);
        }
    }

    #[derive(Clone, Default)]
    struct ReplayFake {
        windows: Arc<Mutex<Vec<usize>>>,
        decoded: Arc<Mutex<usize>>,
    }

    impl Backend for ReplayFake {
        fn encode(
            &mut self,
            _features: Vec<f32>,
            valid: usize,
        ) -> Result<Encoding, RecognizerError> {
            let count = valid / 8;
            self.windows.lock().unwrap().push(count);
            Ok(Encoding {
                values: Vec::new(),
                stride: count,
                count,
            })
        }

        fn decode(
            &mut self,
            _encoded: &[f32],
            _stride: usize,
            range: std::ops::Range<usize>,
            carried: &mut Decoding<'_>,
        ) -> Result<Vec<Vec<usize>>, RecognizerError> {
            *self.decoded.lock().unwrap() += range.len();
            let at = carried.predictor.as_ref().map_or(0, |p| p.output.len());
            *carried.predictor = Some(Predictor {
                output: vec![0.0; at + range.len()],
                ..Predictor::default()
            });
            Ok((at..at + range.len()).map(|token| vec![token]).collect())
        }
    }

    #[test]
    fn replayed_windows_keep_each_frame_once_and_the_final_reconsiders_the_whole_stretch() {
        let backend = ReplayFake::default();
        let windows = Arc::clone(&backend.windows);
        let decoded = Arc::clone(&backend.decoded);
        let mut stream = Replayed::new(backend);
        for count in [8, 16, 24, 32, 40, 48, 56, 64] {
            let frames = stream.snapshot(&sound(count * 1280, 0)).unwrap();
            assert_eq!(
                frames.concat(),
                (0..count).collect::<Vec<_>>(),
                "no lost or duplicated frames at {count}"
            );
        }
        assert_eq!(*windows.lock().unwrap(), [8, 16, 24, 32, 32, 32, 32, 32]);
        assert_eq!(
            *decoded.lock().unwrap(),
            168,
            "each tentative frame is decoded once per replay"
        );
        assert_eq!(
            stream.finish(&sound(64 * 1280, 0)).unwrap().concat(),
            (0..64).collect::<Vec<_>>()
        );
        assert_eq!(
            windows.lock().unwrap().last(),
            Some(&74),
            "the final encoder hears 800 ms of look-ahead"
        );
        stream.next_segment();
        assert_eq!(
            stream.snapshot(&sound(8 * 1280, 0)).unwrap().concat(),
            (0..8).collect::<Vec<_>>()
        );
    }

    #[derive(Clone)]
    struct LateWord;

    impl Backend for LateWord {
        fn encode(
            &mut self,
            _features: Vec<f32>,
            valid: usize,
        ) -> Result<Encoding, RecognizerError> {
            let count = valid / 8;
            let mut values = vec![0.0; count];
            if count >= 32 {
                values[0] = SPEECH;
            }
            Ok(Encoding {
                values,
                stride: count,
                count,
            })
        }

        fn decode(
            &mut self,
            encoded: &[f32],
            _stride: usize,
            range: std::ops::Range<usize>,
            carried: &mut Decoding<'_>,
        ) -> Result<Vec<Vec<usize>>, RecognizerError> {
            let predictor = carried.predictor.get_or_insert_with(Predictor::default);
            Ok(range
                .map(|frame| {
                    if predictor.output.is_empty() && encoded[frame] == SPEECH {
                        predictor.output.push(1.0);
                        vec![0]
                    } else {
                        Vec::new()
                    }
                })
                .collect())
        }
    }

    #[test]
    fn an_empty_early_window_does_not_discard_a_word_that_needs_more_context() {
        let mut stream = Replayed::new(LateWord);
        for count in [8, 16, 24] {
            assert_eq!(
                stream.snapshot(&sound(count * 1280, 0)).unwrap().concat(),
                [] as [usize; 0]
            );
        }
        for count in [32, 40] {
            assert_eq!(
                stream.snapshot(&sound(count * 1280, 0)).unwrap().concat(),
                [0],
                "the opening word is still available at frame {count}"
            );
        }
    }

    #[derive(Clone)]
    struct AfterPause;

    impl Backend for AfterPause {
        fn encode(
            &mut self,
            features: Vec<f32>,
            valid: usize,
        ) -> Result<Encoding, RecognizerError> {
            let mut values = vec![0.0; 2 * FRAMES];
            for (frame, row) in features[..valid].chunks(8).enumerate() {
                if row.windows(2).any(|pair| (pair[0] - pair[1]).abs() > 1e-4) {
                    values[frame] = SPEECH;
                }
            }
            Ok(Encoding {
                values,
                stride: FRAMES,
                count: valid.div_ceil(8),
            })
        }

        fn decode(
            &mut self,
            encoded: &[f32],
            _stride: usize,
            range: std::ops::Range<usize>,
            carried: &mut Decoding<'_>,
        ) -> Result<Vec<Vec<usize>>, RecognizerError> {
            let predictor = carried.predictor.get_or_insert_with(Predictor::default);
            Ok(range
                .map(|frame| {
                    if predictor.output.is_empty() && encoded[frame] == SPEECH {
                        predictor.output.push(1.0);
                        vec![0]
                    } else {
                        Vec::new()
                    }
                })
                .collect())
        }
    }

    #[test]
    fn a_buffered_word_after_more_than_five_point_six_seconds_of_silence_is_kept() {
        for seconds in [7, 10] {
            let (fixture, _) = buffered();
            let mut stream = Buffered::new(AfterPause, fixture.shape);
            assert_eq!(stream.push(&sound(8 * STEP, 0)).unwrap().concat(), [0]);
            stream.next_segment();
            assert_eq!(
                stream.push(&vec![0.0; seconds * 16_000]).unwrap().concat(),
                [] as [usize; 0]
            );
            assert_eq!(
                stream.push(&sound(8 * STEP, 1)).unwrap().concat(),
                [0],
                "the word after {seconds} seconds is present"
            );
        }
    }
}

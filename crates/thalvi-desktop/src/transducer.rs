//! Owns the streaming `NeMo` transducer (cache-aware encoder, LSTM predictor, joiner) on ONNX Runtime, as exported by sherpa-onnx.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ort::session::Session;
use ort::value::Tensor;

use crate::fbank::{LogMel, MEL_BINS, MelFrame};
use crate::frame::Frame;
use crate::local::{ModelSource, SpeechModel, SpeechStream};
use crate::models::{CatalogSpeech, ModelStore};
use crate::recognizer::{RecognizerError, RecognizerEvent};
use crate::runtime;

/// Silence after speech that ends a stretch of it, as a `Final`.
const ENDPOINT_MS: usize = 600;
/// One feature frame is 10 ms.
const FRAME_MS: usize = 10;
/// The most tokens one encoder frame may emit.
const MAX_TOKENS_PER_FRAME: usize = 10;
const PCM_SCALE: f32 = 32768.0;

/// The files of one exported model.
#[derive(Clone, Debug)]
pub struct TransducerFiles {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
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

/// Sizes the encoder reports about itself.
struct Shape {
    /// Feature frames per encoder call.
    window: usize,
    /// Feature frames the next call moves on by.
    shift: usize,
    subsampling: usize,
    channel: [usize; 3],
    time: [usize; 3],
    hidden: usize,
    layers: usize,
    encoder_dim: usize,
    blank: usize,
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
    tokens: Arc<Vec<String>>,
}

fn open(path: &Path, threads: usize) -> Result<Session, TransducerError> {
    let environment = runtime::environment().map_err(|reason| load_error(path, &reason))?;
    let build = || -> ort::Result<Session> {
        Session::builder(environment)?
            .with_intra_threads(threads)?
            .with_inter_threads(1)?
            .commit_from_file(path)
    };
    build().map_err(|error| load_error(path, &error))
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
    /// Loads the model; this reads hundreds of megabytes, so call it off the async loop.
    ///
    /// # Errors
    /// Fails when a file cannot be read, is not a streaming `NeMo` transducer, or has features other
    /// than 128 mel bins.
    pub fn load(files: &TransducerFiles) -> Result<Self, TransducerError> {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
        let encoder = open(&files.encoder, threads)?;
        let decoder = open(&files.decoder, 1)?;
        let joiner = open(&files.joiner, 1)?;
        let path = &files.encoder;
        let count = |key| number(&encoder, path, key);
        if count("feat_dim")? != MEL_BINS {
            return Err(unsupported(path, &format!("it needs {MEL_BINS} mel bins")));
        }
        if encoder.inputs().len() != 5 || decoder.inputs().len() != 4 || joiner.inputs().len() != 2
        {
            return Err(unsupported(path, &"it is not a streaming NeMo transducer"));
        }
        let tokens: Vec<String> = std::fs::read_to_string(&files.tokens)
            .map_err(|error| load_error(&files.tokens, &error))?
            .lines()
            .map(|line| {
                line.rsplit_once(' ')
                    .map_or(line, |(piece, _)| piece)
                    .to_owned()
            })
            .collect();
        let shape = Shape {
            window: count("window_size")?,
            shift: count("chunk_shift")?,
            subsampling: count("subsampling_factor")?,
            channel: [
                count("cache_last_channel_dim1")?,
                count("cache_last_channel_dim2")?,
                count("cache_last_channel_dim3")?,
            ],
            time: [
                count("cache_last_time_dim1")?,
                count("cache_last_time_dim2")?,
                count("cache_last_time_dim3")?,
            ],
            hidden: count("pred_hidden")?,
            layers: count("pred_rnn_layers")?,
            encoder_dim: count("cache_last_channel_dim3")?,
            blank: count("vocab_size")?,
        };
        if tokens.len() != shape.blank + 1 || shape.shift > shape.window || shape.subsampling == 0 {
            return Err(unsupported(
                &files.tokens,
                &"its tokens do not match the encoder",
            ));
        }
        Ok(Self {
            sessions: Arc::new(Mutex::new(Sessions {
                encoder,
                decoder,
                joiner,
            })),
            shape: Arc::new(shape),
            tokens: Arc::new(tokens),
        })
    }
}

impl SpeechModel for Transducer {
    type Stream = TransducerStream;

    fn stream(&self) -> TransducerStream {
        let mut stream = TransducerStream {
            sessions: Arc::clone(&self.sessions),
            shape: Arc::clone(&self.shape),
            tokens: Arc::clone(&self.tokens),
            mel: LogMel::new(),
            features: VecDeque::new(),
            encoder: EncoderCache::default(),
            predictor: None,
            spoken: Vec::new(),
            trailing_blanks: 0,
            shown: String::new(),
        };
        stream.encoder = EncoderCache::empty(&stream.shape);
        stream
    }
}

/// What the encoder carries from one chunk to the next.
#[derive(Default)]
struct EncoderCache {
    channel: Vec<f32>,
    time: Vec<f32>,
    length: i64,
}

impl EncoderCache {
    fn empty(shape: &Shape) -> Self {
        Self {
            channel: vec![0.0; shape.channel.iter().product()],
            time: vec![0.0; shape.time.iter().product()],
            length: 0,
        }
    }
}

/// The predictor's output for the last token and its recurrent state.
struct Predictor {
    output: Vec<f32>,
    states: [Vec<f32>; 2],
}

/// One recognition on a [`Transducer`].
pub struct TransducerStream {
    sessions: Arc<Mutex<Sessions>>,
    shape: Arc<Shape>,
    tokens: Arc<Vec<String>>,
    mel: LogMel,
    features: VecDeque<MelFrame>,
    encoder: EncoderCache,
    /// `None` until the next decode, then the predictor primed with the blank token.
    predictor: Option<Predictor>,
    spoken: Vec<usize>,
    trailing_blanks: usize,
    shown: String,
}

fn failed(reason: impl std::fmt::Display) -> RecognizerError {
    tracing::warn!(%reason, "local speech recognition failed");
    RecognizerError::new("Local speech recognition failed. Nothing was delivered.")
}

fn dims(shape: &[usize]) -> Vec<usize> {
    shape.to_vec()
}

impl TransducerStream {
    fn text(&self) -> String {
        self.spoken
            .iter()
            .filter_map(|id| self.tokens.get(*id))
            .flat_map(|piece| piece.chars())
            .map(|c| if c == '\u{2581}' { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_owned()
    }

    fn restart(&mut self) {
        self.encoder = EncoderCache::empty(&self.shape);
        self.predictor = None;
        self.spoken.clear();
        self.trailing_blanks = 0;
        self.shown.clear();
    }

    fn run_encoder(
        &mut self,
        sessions: &mut Sessions,
    ) -> Result<(Vec<f32>, usize), RecognizerError> {
        let window = self.shape.window;
        let mut audio = vec![0.0_f32; MEL_BINS * window];
        for (t, frame) in self.features.iter().take(window).enumerate() {
            for (m, value) in frame.iter().enumerate() {
                audio[m * window + t] = *value;
            }
        }
        self.features.drain(..self.shape.shift);
        let [c0, c1, c2] = self.shape.channel;
        let [t0, t1, t2] = self.shape.time;
        let inputs = ort::inputs![
            Tensor::from_array((dims(&[1, MEL_BINS, window]), audio)).map_err(failed)?,
            Tensor::from_array((dims(&[1]), vec![i64::try_from(window).unwrap_or(i64::MAX)]))
                .map_err(failed)?,
            Tensor::from_array((
                dims(&[1, c0, c1, c2]),
                std::mem::take(&mut self.encoder.channel)
            ))
            .map_err(failed)?,
            Tensor::from_array((
                dims(&[1, t0, t1, t2]),
                std::mem::take(&mut self.encoder.time)
            ))
            .map_err(failed)?,
            Tensor::from_array((dims(&[1]), vec![self.encoder.length])).map_err(failed)?,
        ];
        let outputs = sessions.encoder.run(inputs).map_err(failed)?;
        let (shape, encoded) = outputs[0].try_extract_tensor::<f32>().map_err(failed)?;
        let frames = shape
            .get(2)
            .and_then(|n| usize::try_from(*n).ok())
            .ok_or_else(|| failed("the encoder returned an unexpected shape"))?;
        let encoded = encoded.to_vec();
        let (_, channel) = outputs[2].try_extract_tensor::<f32>().map_err(failed)?;
        self.encoder.channel = channel.to_vec();
        let (_, time) = outputs[3].try_extract_tensor::<f32>().map_err(failed)?;
        self.encoder.time = time.to_vec();
        let (_, length) = outputs[4].try_extract_tensor::<i64>().map_err(failed)?;
        self.encoder.length = length.first().copied().unwrap_or(0);
        Ok((encoded, frames))
    }

    fn run_predictor(
        &self,
        sessions: &mut Sessions,
        token: usize,
        states: [Vec<f32>; 2],
    ) -> Result<Predictor, RecognizerError> {
        let (hidden, layers) = (self.shape.hidden, self.shape.layers);
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

    /// Greedy search over one chunk's encoder frames, as sherpa-onnx does.
    fn decode(
        &mut self,
        sessions: &mut Sessions,
        encoded: &[f32],
        frames: usize,
    ) -> Result<(), RecognizerError> {
        let dim = self.shape.encoder_dim;
        let blank = self.shape.blank;
        let mut predictor = if let Some(predictor) = self.predictor.take() {
            predictor
        } else {
            let zeros = || vec![0.0; self.shape.layers * self.shape.hidden];
            self.run_predictor(sessions, blank, [zeros(), zeros()])?
        };
        for t in 0..frames {
            let frame: Vec<f32> = (0..dim).map(|c| encoded[c * frames + t]).collect();
            for _ in 0..MAX_TOKENS_PER_FRAME {
                let inputs = ort::inputs![
                    Tensor::from_array((dims(&[1, dim, 1]), frame.clone())).map_err(failed)?,
                    Tensor::from_array((
                        dims(&[1, self.shape.hidden, 1]),
                        predictor.output.clone()
                    ))
                    .map_err(failed)?,
                ];
                let best = {
                    let outputs = sessions.joiner.run(inputs).map_err(failed)?;
                    let (_, logits) = outputs[0].try_extract_tensor::<f32>().map_err(failed)?;
                    logits
                        .iter()
                        .enumerate()
                        .fold((0, f32::NEG_INFINITY), |best, (id, value)| {
                            if *value > best.1 { (id, *value) } else { best }
                        })
                        .0
                };
                if best == blank {
                    self.trailing_blanks += 1;
                    break;
                }
                self.spoken.push(best);
                self.trailing_blanks = 0;
                predictor = self.run_predictor(sessions, best, predictor.states)?;
            }
        }
        self.predictor = Some(predictor);
        Ok(())
    }

    fn silent_long_enough(&self) -> bool {
        self.trailing_blanks * self.shape.subsampling * FRAME_MS >= ENDPOINT_MS
    }
}

impl SpeechStream for TransducerStream {
    fn accept(&mut self, frame: &Frame) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        let samples: Vec<f32> = frame.iter().map(|s| f32::from(*s) / PCM_SCALE).collect();
        let mut mel = Vec::new();
        self.mel.push(&samples, &mut mel);
        self.features.extend(mel);
        let sessions = Arc::clone(&self.sessions);
        let mut events = Vec::new();
        while self.features.len() >= self.shape.window {
            let mut sessions = sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let (encoded, frames) = self.run_encoder(&mut sessions)?;
            self.decode(&mut sessions, &encoded, frames)?;
            drop(sessions);
            let text = self.text();
            if !self.spoken.is_empty() && self.silent_long_enough() {
                events.push(RecognizerEvent::Final(text));
                self.restart();
            } else if text != self.shown {
                events.push(RecognizerEvent::Partial(text.clone()));
                self.shown = text;
            }
        }
        Ok(events)
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
}

impl TransducerSource {
    #[must_use]
    pub fn new(store: ModelStore, choice: SpeechChoice) -> Self {
        Self { store, choice }
    }

    async fn files(&self) -> Result<TransducerFiles, RecognizerError> {
        let plain = |error: &dyn std::fmt::Display| RecognizerError::new(error.to_string());
        match &self.choice {
            SpeechChoice::Catalog(catalog) => {
                let assets = catalog.assets();
                Ok(TransducerFiles {
                    encoder: self
                        .store
                        .fetch(&assets.encoder)
                        .await
                        .map_err(|e| plain(&e))?,
                    decoder: self
                        .store
                        .fetch(&assets.decoder)
                        .await
                        .map_err(|e| plain(&e))?,
                    joiner: self
                        .store
                        .fetch(&assets.joiner)
                        .await
                        .map_err(|e| plain(&e))?,
                    tokens: self
                        .store
                        .fetch(&assets.tokens)
                        .await
                        .map_err(|e| plain(&e))?,
                })
            }
            SpeechChoice::Folder(dir) => Ok(TransducerFiles {
                encoder: dir.join("encoder.int8.onnx"),
                decoder: dir.join("decoder.int8.onnx"),
                joiner: dir.join("joiner.int8.onnx"),
                tokens: dir.join("tokens.txt"),
            }),
        }
    }
}

impl ModelSource for TransducerSource {
    type Model = Transducer;

    async fn load(&self) -> Result<Transducer, RecognizerError> {
        let files = self.files().await?;
        tokio::task::spawn_blocking(move || Transducer::load(&files))
            .await
            .map_err(|_| RecognizerError::new("The speech model could not be loaded."))?
            .map_err(|error| RecognizerError::new(error.to_string()))
    }
}

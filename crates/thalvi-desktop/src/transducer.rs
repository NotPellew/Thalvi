//! Owns the streaming `NeMo` transducer (cache-aware encoder, LSTM predictor, joiner) on ONNX Runtime, as exported by sherpa-onnx.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ort::session::Session;
use ort::value::Tensor;

use crate::fbank::{LogMel, MEL_BINS, MelFrame};
use crate::local::{ModelSource, SpeechModel};
use crate::models::{CatalogSpeech, ModelStore};
use crate::recognizer::RecognizerError;
use crate::runtime;
use crate::segments::{FrameDecoder, Segments};

/// Silence after speech that ends a stretch of it, as a `Final`.
const ENDPOINT_MS: usize = 600;
/// One feature frame is 10 ms.
const FRAME_MS: usize = 10;
/// The most tokens one encoder frame may emit.
const MAX_TOKENS_PER_FRAME: usize = 10;

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
    type Stream = Segments<TransducerStream>;

    fn stream(&self) -> Segments<TransducerStream> {
        let shape = Arc::clone(&self.shape);
        let endpoint_blanks = ENDPOINT_MS.div_ceil(shape.subsampling * FRAME_MS);
        let decoder = TransducerStream {
            sessions: Arc::clone(&self.sessions),
            carried: Carried::empty(&shape),
            shape,
            mel: LogMel::new(),
            features: VecDeque::new(),
        };
        Segments::new(decoder, Arc::clone(&self.tokens), endpoint_blanks)
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
    fn empty(shape: &Shape) -> Self {
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
}

impl Carried {
    fn empty(shape: &Shape) -> Self {
        Self {
            encoder: EncoderCache::empty(shape),
            predictor: None,
        }
    }

    /// The text starts afresh but the encoder keeps what it heard: without that context the first
    /// words after a pause are lost.
    fn next_segment(&mut self) {
        self.predictor = None;
    }
}

/// The predictor's output for the last token and its recurrent state.
#[derive(Clone)]
struct Predictor {
    output: Vec<f32>,
    states: [Vec<f32>; 2],
}

/// One recognition on a [`Transducer`].
#[derive(Clone)]
pub struct TransducerStream {
    sessions: Arc<Mutex<Sessions>>,
    shape: Arc<Shape>,
    mel: LogMel,
    features: VecDeque<MelFrame>,
    carried: Carried,
}

fn failed(reason: impl std::fmt::Display) -> RecognizerError {
    tracing::warn!(%reason, "local speech recognition failed");
    RecognizerError::new("Local speech recognition failed. Nothing was delivered.")
}

fn dims(shape: &[usize]) -> Vec<usize> {
    shape.to_vec()
}

impl TransducerStream {
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
    ) -> Result<Vec<Vec<usize>>, RecognizerError> {
        let dim = self.shape.encoder_dim;
        let blank = self.shape.blank;
        let mut predictor = if let Some(predictor) = self.carried.predictor.take() {
            predictor
        } else {
            let zeros = || vec![0.0; self.shape.layers * self.shape.hidden];
            self.run_predictor(sessions, blank, [zeros(), zeros()])?
        };
        let mut emitted = Vec::with_capacity(frames);
        for t in 0..frames {
            let mut tokens = Vec::new();
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
                    break;
                }
                tokens.push(best);
                predictor = self.run_predictor(sessions, best, predictor.states)?;
            }
            emitted.push(tokens);
        }
        self.carried.predictor = Some(predictor);
        Ok(emitted)
    }
}

impl FrameDecoder for TransducerStream {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        let mut mel = Vec::new();
        self.mel.push(samples, &mut mel);
        self.features.extend(mel);
        let sessions = Arc::clone(&self.sessions);
        let mut emitted = Vec::new();
        while self.features.len() >= self.shape.window {
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
            SpeechChoice::Folder(dir) => Ok(TransducerFiles::in_folder(dir)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thalvi-folder-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        };
        carried.next_segment();
        assert!(carried.predictor.is_none());
        assert_eq!(carried.encoder.channel, [1.0, 2.0]);
        assert_eq!(carried.encoder.time, [3.0]);
        assert_eq!(carried.encoder.length, 7);
    }
}

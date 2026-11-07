//! Owns spotting of the configured ending phrases in the microphone stream: sherpa-onnx's streaming
//! Zipformer keyword model on ONNX Runtime with its context-graph beam search.
//!
//! The decoding follows sherpa-onnx `keyword-spotter-transducer-impl.h`,
//! `transducer-keyword-decoder.cc` and `context-graph.cc` at commit
//! `3e437b331fa273ae0819bc19659eb0f53efc03d3`, with its default boost 1.0, acoustic threshold 0.25,
//! four active paths and more than one trailing blank. Phrases are tokenized with the model's own
//! unigram `SentencePiece` pieces, so any phrase of plain letters works; others are refused at load.

use std::collections::{BTreeMap, HashMap};

use ort::session::{Session, SessionInputValue};
use ort::value::{DynValue, Outlet, Tensor, TensorElementType};

use crate::capture::{Ending, Endings};
use crate::fbank::{KALDI_BINS, KaldiFbank, KaldiFrame};
use crate::frame::Frame;
use crate::models::{self, ModelError, ModelStore};
use crate::report::UserError;
use crate::runtime;

/// Mean token probability a phrase must reach. sherpa-onnx's default; not yet calibrated for Thalvi.
const ACOUSTIC_THRESHOLD: f32 = 0.25;
/// Log-score added for each token that follows a phrase.
const BOOST: f32 = 1.0;
const ACTIVE_PATHS: usize = 4;
/// A hit needs more blank frames than this after the phrase.
const TRAILING_BLANKS: u32 = 1;
/// Blank frames after which the search starts over, 1.5 s of silence at 40 ms each.
const SILENCE_BLANKS: u32 = 38;
/// Samples per encoder output frame: 4 feature frames of 160 samples.
const ENCODER_FRAME_SAMPLES: u64 = 640;
const SUBSAMPLING: usize = 4;
const CONTEXT: usize = 2;
const BLANK: usize = 0;
const WORD_START: char = '\u{2581}';

/// A phrase heard in the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SpotterHit {
    /// The configured phrase, as in [`Endings`].
    pub phrase: String,
    pub ending: Ending,
    /// Where the phrase's first token was emitted, in 16 kHz samples since the last reset. The
    /// model emits a token near where it is heard, not at an exact word boundary.
    pub start_sample: u64,
    /// The end of the frame the phrase's last token was emitted in, counted like `start_sample`.
    pub end_sample: u64,
    /// The mean probability of the phrase's tokens, from 0 to 1.
    pub confidence: f32,
}

/// Hears the ending phrases in successive microphone frames.
pub trait KeywordSpotter {
    /// Forgets the audio so far; positions of later hits count from here.
    fn reset(&mut self);

    /// Takes the next frame, in order, and tells whether a phrase has just been heard.
    ///
    /// # Errors
    /// Fails when the model returns something other than the documented shapes or probabilities.
    fn push(&mut self, frame: &Frame) -> Result<Option<SpotterHit>, SpotterError>;
}

/// An ending phrase the model cannot be asked to listen for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unspeakable {
    pub phrase: String,
    /// The characters of the phrase the model has no sound for.
    pub characters: String,
}

fn describe(phrases: &[Unspeakable]) -> String {
    phrases
        .iter()
        .map(|bad| format!("\"{}\" (the characters {})", bad.phrase, bad.characters))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why the spotter could not start or run. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum SpotterError {
    #[error(
        "Spoken endings can only use plain letters, apostrophes and hyphens, but these cannot be heard: {}. Change them in Settings.",
        describe(.0)
    )]
    Phrases(Vec<Unspeakable>),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("ONNX Runtime could not start for the ending phrase detector: {0}")]
    Runtime(String),
    #[error("The ending phrase model {file} could not be loaded: {reason}")]
    Load { file: String, reason: String },
    #[error("The ending phrase model {file} cannot be used here: {reason}")]
    Unsupported { file: String, reason: String },
    #[error("Ending phrase detection failed: {0}")]
    Inference(String),
}

impl UserError for SpotterError {
    fn headline(&self) -> &'static str {
        match self {
            Self::Phrases(_) => "Ending phrases can't be heard",
            Self::Model(error) => error.headline(),
            Self::Runtime(_) => "Phrase detector won't start",
            Self::Load { .. } => "Phrase model can't be loaded",
            Self::Unsupported { .. } => "Phrase model can't be used",
            Self::Inference(_) => "Phrase detection failed",
        }
    }
}

fn load_error(file: &std::path::Path, reason: &impl ToString) -> SpotterError {
    SpotterError::Load {
        file: file.display().to_string(),
        reason: reason.to_string(),
    }
}

fn unsupported(file: &std::path::Path, reason: &impl ToString) -> SpotterError {
    SpotterError::Unsupported {
        file: file.display().to_string(),
        reason: reason.to_string(),
    }
}

fn failed(reason: impl std::fmt::Display) -> SpotterError {
    SpotterError::Inference(reason.to_string())
}

/// The model's `SentencePiece` pieces: their scores segment a phrase, their numbers are tokens.
struct Pieces {
    /// Normal pieces by text, with their number and score.
    by_text: HashMap<String, (usize, f32)>,
    longest: usize,
    count: usize,
    unknown: Option<usize>,
}

/// Reads the pieces of a unigram `SentencePiece` model file (a protobuf).
fn read_pieces(bytes: &[u8]) -> Result<Pieces, &'static str> {
    /// A field number with its varint value or its bytes.
    type Field<'a> = (u64, Result<u64, &'a [u8]>);
    const BAD: &str = "it is not a SentencePiece model";
    fn varint(bytes: &mut &[u8]) -> Result<u64, &'static str> {
        let mut value = 0_u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = bytes.split_first().ok_or(BAD)?;
            *bytes = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(BAD)
    }
    /// The fields of one message as (number, varint value or bytes).
    fn fields(mut bytes: &[u8]) -> Result<Vec<Field<'_>>, &'static str> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let key = varint(&mut bytes)?;
            let value = match key & 7 {
                0 => Ok(varint(&mut bytes)?),
                1 => Err(take(&mut bytes, 8)?),
                2 => {
                    let len = usize::try_from(varint(&mut bytes)?).map_err(|_| BAD)?;
                    Err(take(&mut bytes, len)?)
                }
                5 => Err(take(&mut bytes, 4)?),
                _ => return Err(BAD),
            };
            out.push((key >> 3, value));
        }
        Ok(out)
    }
    fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], &'static str> {
        let (head, rest) = bytes.split_at_checked(len).ok_or(BAD)?;
        *bytes = rest;
        Ok(head)
    }

    const NORMAL: u64 = 1;
    const UNKNOWN: u64 = 2;
    const UNIGRAM: u64 = 1;
    let mut pieces = Pieces {
        by_text: HashMap::new(),
        longest: 0,
        count: 0,
        unknown: None,
    };
    let mut unigram = false;
    for (number, value) in fields(bytes)? {
        match (number, value) {
            (1, Err(piece)) => {
                let (mut text, mut score, mut kind) = (None, 0.0_f32, NORMAL);
                for (number, value) in fields(piece)? {
                    match (number, value) {
                        (1, Err(raw)) => {
                            text = Some(String::from_utf8(raw.to_vec()).map_err(|_| BAD)?);
                        }
                        (2, Err(raw)) => {
                            score = f32::from_le_bytes(raw.try_into().map_err(|_| BAD)?);
                        }
                        (3, Ok(value)) => kind = value,
                        _ => {}
                    }
                }
                let text = text.ok_or(BAD)?;
                if kind == NORMAL {
                    pieces.longest = pieces.longest.max(text.chars().count());
                    pieces.by_text.insert(text, (pieces.count, score));
                } else if kind == UNKNOWN {
                    pieces.unknown = Some(pieces.count);
                }
                pieces.count += 1;
            }
            (2, Err(trainer)) => {
                unigram = fields(trainer)?
                    .into_iter()
                    .any(|field| matches!(field, (3, Ok(UNIGRAM))));
            }
            _ => {}
        }
    }
    if !unigram {
        return Err("it is not a unigram SentencePiece model");
    }
    Ok(pieces)
}

impl Pieces {
    /// The tokens of `phrase` (lowercase, words single-spaced) by the highest-scoring segmentation
    /// into pieces, or the characters that have no piece of their own.
    fn encode(&self, phrase: &str) -> Result<Vec<usize>, String> {
        let text: Vec<char> = std::iter::once(WORD_START)
            .chain(
                phrase
                    .to_uppercase()
                    .chars()
                    .map(|c| if c == ' ' { WORD_START } else { c }),
            )
            .collect();
        let unknown: String = text
            .iter()
            .filter(|c| !self.by_text.contains_key(c.to_string().as_str()))
            .collect();
        if !unknown.is_empty() {
            return Err(unknown);
        }
        // Best (score, previous cut, piece) to reach each cut; the earliest start wins a tie.
        let mut best: Vec<Option<(f32, usize, usize)>> = vec![None; text.len() + 1];
        best[0] = Some((0.0, 0, 0));
        for end in 1..=text.len() {
            for start in end.saturating_sub(self.longest)..end {
                let Some((before, ..)) = best[start] else {
                    continue;
                };
                let piece: String = text[start..end].iter().collect();
                if let Some(&(id, score)) = self.by_text.get(&piece)
                    && best[end].is_none_or(|(known, ..)| before + score > known)
                {
                    best[end] = Some((before + score, start, id));
                }
            }
        }
        let mut tokens = Vec::new();
        let mut at = text.len();
        while at > 0 {
            let Some((_, start, id)) = best[at] else {
                return Err(String::new());
            };
            tokens.push(id);
            at = start;
        }
        tokens.reverse();
        Ok(tokens)
    }
}

/// A state of the phrase automaton: the tokens of some phrase prefix.
struct Node {
    score: f32,
    output_score: f32,
    level: usize,
    ends: Option<usize>,
    next: BTreeMap<usize, usize>,
    fail: usize,
    output: Option<usize>,
}

/// sherpa-onnx's context graph: an Aho-Corasick automaton over the phrases' tokens whose arcs
/// carry the boost.
struct Graph {
    nodes: Vec<Node>,
}

const ROOT: usize = 0;

impl Graph {
    fn new(phrases: &[Vec<usize>]) -> Self {
        let node = |score, level| Node {
            score,
            output_score: 0.0,
            level,
            ends: None,
            next: BTreeMap::new(),
            fail: ROOT,
            output: None,
        };
        let mut nodes = vec![node(0.0, 0)];
        for (phrase, tokens) in phrases.iter().enumerate() {
            let mut at = ROOT;
            for (index, &token) in tokens.iter().enumerate() {
                at = if let Some(&known) = nodes[at].next.get(&token) {
                    known
                } else {
                    let created = nodes.len();
                    let score = nodes[at].score + BOOST;
                    nodes.push(node(score, index + 1));
                    nodes[at].next.insert(token, created);
                    created
                };
            }
            nodes[at].ends = Some(phrase);
            nodes[at].output_score = nodes[at].score;
        }
        let mut graph = Self { nodes };
        graph.link();
        graph
    }

    /// Fills each node's failure arc and the nearest phrase end behind it.
    fn link(&mut self) {
        let mut queue: std::collections::VecDeque<usize> =
            self.nodes[ROOT].next.values().copied().collect();
        while let Some(current) = queue.pop_front() {
            let arcs: Vec<(usize, usize)> = self.nodes[current]
                .next
                .iter()
                .map(|(&token, &child)| (token, child))
                .collect();
            for (token, child) in arcs {
                let mut fail = self.nodes[current].fail;
                if let Some(&next) = self.nodes[fail].next.get(&token) {
                    fail = next;
                } else {
                    fail = self.nodes[fail].fail;
                    while !self.nodes[fail].next.contains_key(&token) {
                        fail = self.nodes[fail].fail;
                        if fail == ROOT {
                            break;
                        }
                    }
                    if let Some(&next) = self.nodes[fail].next.get(&token) {
                        fail = next;
                    }
                }
                self.nodes[child].fail = fail;
                let mut output = Some(fail);
                while let Some(candidate) = output {
                    if self.nodes[candidate].ends.is_some() {
                        break;
                    }
                    let behind = self.nodes[candidate].fail;
                    output = (behind != ROOT).then_some(behind);
                }
                self.nodes[child].output = output;
                if let Some(output) = output {
                    self.nodes[child].output_score += self.nodes[output].output_score;
                }
                queue.push_back(child);
            }
        }
    }

    /// The score gained by reading `token` in `state`, and the state after it.
    fn step(&self, state: usize, token: usize) -> (f32, usize) {
        let node = if let Some(&next) = self.nodes[state].next.get(&token) {
            next
        } else {
            let mut node = self.nodes[state].fail;
            while !self.nodes[node].next.contains_key(&token) {
                node = self.nodes[node].fail;
                if node == ROOT {
                    break;
                }
            }
            self.nodes[node].next.get(&token).copied().unwrap_or(node)
        };
        let score = if self.nodes[state].next.contains_key(&token) {
            BOOST
        } else {
            self.nodes[node].score - self.nodes[state].score
        };
        (score + self.nodes[node].output_score, node)
    }

    /// The phrase end that `state` is or contains, and how many tokens it has.
    fn matched(&self, state: usize) -> Option<(usize, usize)> {
        let end = if self.nodes[state].ends.is_some() {
            state
        } else {
            self.nodes[state].output?
        };
        Some((self.nodes[end].ends?, self.nodes[end].level))
    }
}

/// One path of the beam search.
#[derive(Clone)]
struct Path {
    /// The blank-padded token history the predictor reads its last two tokens from.
    tokens: Vec<i64>,
    /// The encoder frame, in the stream, each token since the last restart was emitted in.
    frames: Vec<u64>,
    probabilities: Vec<f32>,
    log_prob: f64,
    trailing_blanks: u32,
    state: usize,
}

impl Path {
    fn start() -> Self {
        Self {
            tokens: vec![-1, 0],
            frames: Vec::new(),
            probabilities: Vec::new(),
            log_prob: 0.0,
            trailing_blanks: 0,
            state: ROOT,
        }
    }
}

fn log_add(x: f64, y: f64) -> f64 {
    let (high, low) = if x < y { (y, x) } else { (x, y) };
    high + (low - high).exp().ln_1p()
}

/// One cached tensor of the encoder, zero at the start.
enum Cache {
    Floats(Vec<f32>),
    Ints(Vec<i64>),
}

struct Layout {
    shape: Vec<usize>,
    ints: bool,
}

impl Layout {
    fn zeros(&self) -> Cache {
        let len = self.shape.iter().product();
        if self.ints {
            Cache::Ints(vec![0; len])
        } else {
            Cache::Floats(vec![0.0; len])
        }
    }
}

/// The ONNX sessions and what the encoder expects of them.
struct Network {
    encoder: Session,
    decoder: Session,
    joiner: Session,
    layouts: Vec<Layout>,
    /// Feature frames per encoder call.
    window: usize,
    /// Feature frames the next call moves on by.
    shift: usize,
    dim: usize,
    vocab: usize,
}

fn tensor_shape(outlet: &Outlet) -> Option<Vec<i64>> {
    outlet.dtype().tensor_shape().map(|shape| shape.to_vec())
}

fn number(session: &Session, path: &std::path::Path, key: &str) -> Result<usize, SpotterError> {
    session
        .metadata()
        .ok()
        .and_then(|metadata| metadata.custom(key))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| unsupported(path, &format!("it does not say its {key}")))
}

fn open(path: &std::path::Path) -> Result<Session, SpotterError> {
    let environment = runtime::environment().map_err(SpotterError::Runtime)?;
    let build = || -> ort::Result<Session> {
        Session::builder(environment)?
            .with_intra_threads(1)?
            .with_inter_threads(1)?
            .commit_from_file(path)
    };
    build().map_err(|error| load_error(path, &error))
}

impl Network {
    fn load(files: &Files, vocab: usize) -> Result<Self, SpotterError> {
        let (encoder, decoder, joiner) = (
            open(&files.encoder)?,
            open(&files.decoder)?,
            open(&files.joiner)?,
        );
        let path = &files.encoder;
        let mut inputs = encoder.inputs().iter();
        let window = inputs
            .next()
            .and_then(tensor_shape)
            .and_then(|shape| match shape[..] {
                [_, window, bins] if usize::try_from(bins) == Ok(KALDI_BINS) => {
                    usize::try_from(window).ok()
                }
                _ => None,
            })
            .ok_or_else(|| {
                unsupported(
                    path,
                    &format!("it needs {KALDI_BINS} mel bins in fixed windows"),
                )
            })?;
        let shift = number(&encoder, path, "decode_chunk_len")?;
        if shift == 0 || shift % SUBSAMPLING != 0 || shift > window {
            return Err(unsupported(
                path,
                &"its chunk length does not fit its window",
            ));
        }
        let layouts = inputs
            .map(|outlet| {
                let kind = outlet.dtype().tensor_type();
                let shape = tensor_shape(outlet)?;
                let shape = shape
                    .iter()
                    .map(|&dim| {
                        if dim == -1 {
                            Some(1)
                        } else {
                            usize::try_from(dim).ok()
                        }
                    })
                    .collect::<Option<Vec<usize>>>()?;
                match kind {
                    Some(TensorElementType::Float32) => Some(Layout { shape, ints: false }),
                    Some(TensorElementType::Int64) => Some(Layout { shape, ints: true }),
                    _ => None,
                }
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| unsupported(path, &"its cached states are not tensors it knows"))?;
        if layouts.is_empty() || encoder.outputs().len() != layouts.len() + 1 {
            return Err(unsupported(
                path,
                &"its cached states do not match its outputs",
            ));
        }
        let dim = number(&joiner, &files.joiner, "joiner_dim")?;
        if number(&decoder, &files.decoder, "context_size")? != CONTEXT
            || number(&decoder, &files.decoder, "vocab_size")? != vocab
        {
            return Err(unsupported(
                &files.decoder,
                &"its tokens do not match the pieces",
            ));
        }
        if decoder.inputs().len() != 1 || joiner.inputs().len() != 2 {
            return Err(unsupported(
                &files.joiner,
                &"it is not a transducer predictor and joiner",
            ));
        }
        let mut network = Self {
            encoder,
            decoder,
            joiner,
            layouts,
            window,
            shift,
            dim,
            vocab,
        };
        network.check(path)?;
        Ok(network)
    }
}

impl Network {
    /// Runs the encoder on one window of `window` feature frames, flattened, with the caches the
    /// last call returned; gives the encoder frames, flattened, and the caches to carry on with.
    fn encode(
        &mut self,
        window: Vec<f32>,
        caches: Vec<Cache>,
    ) -> Result<(Vec<f32>, Vec<Cache>), SpotterError> {
        let mut inputs: Vec<(String, SessionInputValue<'static>)> = vec![(
            self.encoder.inputs()[0].name().to_owned(),
            Tensor::from_array((vec![1, self.window, KALDI_BINS], window))
                .map_err(failed)?
                .into_dyn()
                .into(),
        )];
        for ((outlet, layout), cache) in self
            .encoder
            .inputs()
            .iter()
            .skip(1)
            .zip(&self.layouts)
            .zip(caches)
        {
            let value: DynValue = match cache {
                Cache::Floats(values) => Tensor::from_array((layout.shape.clone(), values))
                    .map_err(failed)?
                    .into_dyn(),
                Cache::Ints(values) => Tensor::from_array((layout.shape.clone(), values))
                    .map_err(failed)?
                    .into_dyn(),
            };
            inputs.push((outlet.name().to_owned(), value.into()));
        }
        let outputs = self.encoder.run(inputs).map_err(failed)?;
        let (shape, encoded) = floats(&outputs, 0)?;
        if shape != [1, signed(self.shift / SUBSAMPLING)?, signed(self.dim)?] {
            return Err(failed("the encoder returned an unexpected shape"));
        }
        let mut caches = Vec::with_capacity(self.layouts.len());
        for (index, layout) in self.layouts.iter().enumerate() {
            let want = layout
                .shape
                .iter()
                .map(|&dim| signed(dim))
                .collect::<Result<Vec<_>, _>>()?;
            let (shape, cache) = if layout.ints {
                let (shape, values) = outputs[index + 1]
                    .try_extract_tensor::<i64>()
                    .map_err(failed)?;
                (shape.to_vec(), Cache::Ints(values.to_vec()))
            } else {
                let (shape, values) = floats(&outputs, index + 1)?;
                (shape, Cache::Floats(values))
            };
            if shape != want {
                return Err(failed(
                    "the encoder returned a cached state of an unexpected shape",
                ));
            }
            caches.push(cache);
        }
        Ok((encoded, caches))
    }

    /// The log probability of every token after each of `contexts` (two tokens per path) given one
    /// encoder frame, one row per path.
    fn log_probabilities(
        &mut self,
        encoded: &[f32],
        contexts: Vec<i64>,
    ) -> Result<Vec<Vec<f32>>, SpotterError> {
        let count = contexts.len() / CONTEXT;
        let outputs = self
            .decoder
            .run(ort::inputs![
                Tensor::from_array((vec![count, CONTEXT], contexts)).map_err(failed)?
            ])
            .map_err(failed)?;
        let (shape, predicted) = floats(&outputs, 0)?;
        if shape != [signed(count)?, signed(self.dim)?] {
            return Err(failed("the predictor returned an unexpected shape"));
        }
        let repeated: Vec<f32> = (0..count).flat_map(|_| encoded.iter().copied()).collect();
        let outputs = self
            .joiner
            .run(ort::inputs![
                Tensor::from_array((vec![count, self.dim], repeated)).map_err(failed)?,
                Tensor::from_array((vec![count, self.dim], predicted)).map_err(failed)?
            ])
            .map_err(failed)?;
        let (shape, logits) = floats(&outputs, 0)?;
        if shape != [signed(count)?, signed(self.vocab)?] {
            return Err(failed("the joiner returned an unexpected shape"));
        }
        if logits.iter().any(|logit| !logit.is_finite()) {
            return Err(failed("the joiner returned a score that is not a number"));
        }
        Ok(logits
            .chunks(self.vocab)
            .map(|row| {
                let high = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let total = row
                    .iter()
                    .map(|logit| (logit - high).exp())
                    .sum::<f32>()
                    .ln();
                row.iter().map(|logit| logit - high - total).collect()
            })
            .collect())
    }

    /// Runs each session once on silence, so a model that does not behave as documented is
    /// refused when it loads and not in the middle of a conversation.
    fn check(&mut self, file: &std::path::Path) -> Result<(), SpotterError> {
        let caches = self.layouts.iter().map(Layout::zeros).collect();
        let checked = self
            .encode(vec![0.0; self.window * KALDI_BINS], caches)
            .and_then(|(encoded, _)| {
                let contexts = [-1, 0].repeat(ACTIVE_PATHS);
                self.log_probabilities(&encoded[..self.dim], contexts)
            });
        checked.map(drop).map_err(|error| unsupported(file, &error))
    }
}

fn signed(value: usize) -> Result<i64, SpotterError> {
    i64::try_from(value).map_err(failed)
}

/// The locations of the model files.
struct Files {
    encoder: std::path::PathBuf,
    decoder: std::path::PathBuf,
    joiner: std::path::PathBuf,
}

fn floats(
    session_outputs: &ort::session::SessionOutputs<'_>,
    index: usize,
) -> Result<(Vec<i64>, Vec<f32>), SpotterError> {
    let (shape, values) = session_outputs[index]
        .try_extract_tensor::<f32>()
        .map_err(failed)?;
    Ok((shape.to_vec(), values.to_vec()))
}

/// The ending phrase spotter on the real model: call [`push`](KeywordSpotter::push) with every
/// microphone frame in order, outside hold capture.
pub struct OnnxSpotter {
    network: Network,
    graph: Graph,
    phrases: Vec<(String, Ending)>,
    unknown: Option<usize>,
    fbank: KaldiFbank,
    features: std::collections::VecDeque<KaldiFrame>,
    caches: Vec<Cache>,
    paths: Vec<Path>,
    trailing_blanks: u32,
    /// Encoder frames decoded since the last reset.
    decoded: u64,
}

impl OnnxSpotter {
    /// Fetches any missing model files through `store`, checks that every phrase of `endings` can be
    /// spoken to the model, then loads and validates the model.
    ///
    /// # Errors
    /// Fails when a file cannot be downloaded or loaded, is not the model it must be, or a phrase has
    /// characters the model cannot hear; the error names each such phrase.
    pub async fn load(endings: &Endings, store: &ModelStore) -> Result<Self, SpotterError> {
        let pieces_path = store.fetch(&models::SPOTTER.pieces).await?;
        let bytes =
            std::fs::read(&pieces_path).map_err(|error| load_error(&pieces_path, &error))?;
        let pieces = read_pieces(&bytes).map_err(|reason| unsupported(&pieces_path, &reason))?;
        let mut tokens = Vec::new();
        let mut phrases = Vec::new();
        let mut refused = Vec::new();
        for (phrase, ending) in endings.entries() {
            match pieces.encode(phrase) {
                Ok(encoded) => {
                    tokens.push(encoded);
                    phrases.push((phrase.to_owned(), ending));
                }
                Err(characters) => refused.push(Unspeakable {
                    phrase: phrase.to_owned(),
                    characters,
                }),
            }
        }
        if !refused.is_empty() {
            return Err(SpotterError::Phrases(refused));
        }
        let files = Files {
            encoder: store.fetch(&models::SPOTTER.encoder).await?,
            decoder: store.fetch(&models::SPOTTER.decoder).await?,
            joiner: store.fetch(&models::SPOTTER.joiner).await?,
        };
        let network = Network::load(&files, pieces.count)?;
        let mut spotter = Self {
            caches: network.layouts.iter().map(Layout::zeros).collect(),
            network,
            graph: Graph::new(&tokens),
            phrases,
            unknown: pieces.unknown,
            fbank: KaldiFbank::new(),
            features: std::collections::VecDeque::new(),
            paths: vec![Path::start()],
            trailing_blanks: 0,
            decoded: 0,
        };
        spotter.reset();
        Ok(spotter)
    }

    fn restart_search(&mut self) {
        self.caches = self.network.layouts.iter().map(Layout::zeros).collect();
        self.paths = vec![Path::start()];
        self.trailing_blanks = 0;
    }

    /// Advances the beam by one encoder frame and tells which phrase ended on it.
    fn advance(&mut self, encoded: &[f32], frame: u64) -> Result<Option<SpotterHit>, SpotterError> {
        let vocab = self.network.vocab;
        let contexts = self
            .paths
            .iter()
            .flat_map(|path| path.tokens[path.tokens.len() - CONTEXT..].to_vec())
            .collect();
        let rows = self.network.log_probabilities(encoded, contexts)?;
        let mut scored: Vec<f32> = Vec::with_capacity(rows.len() * vocab);
        for (path, row) in self.paths.iter().zip(&rows) {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "sherpa-onnx adds in double and stores a float"
            )]
            scored.extend(
                row.iter()
                    .map(|log_prob| (f64::from(*log_prob) + path.log_prob) as f32),
            );
        }
        let mut order: Vec<usize> = (0..scored.len()).collect();
        order.sort_by(|&a, &b| scored[b].total_cmp(&scored[a]).then(a.cmp(&b)));
        let mut next: Vec<Path> = Vec::new();
        for &pick in order.iter().take(ACTIVE_PATHS) {
            let (from, token) = (pick / vocab, pick % vocab);
            let mut path = self.paths[from].clone();
            let mut bonus = 0.0;
            if token != BLANK && Some(token) != self.unknown {
                path.tokens.push(i64::try_from(token).map_err(failed)?);
                path.frames.push(frame);
                path.probabilities.push(rows[from][token].exp());
                path.trailing_blanks = 0;
                let (gained, state) = self.graph.step(path.state, token);
                bonus = gained;
                path.state = state;
                if state == ROOT {
                    path = Path {
                        log_prob: path.log_prob,
                        ..Path::start()
                    };
                }
            } else {
                path.trailing_blanks += 1;
            }
            path.log_prob = f64::from(scored[pick] + bonus);
            if let Some(known) = next.iter_mut().find(|known| known.tokens == path.tokens) {
                known.log_prob = log_add(known.log_prob, path.log_prob);
            } else {
                next.push(path);
            }
        }
        let best = next
            .iter()
            .reduce(|best, path| {
                if best.log_prob < path.log_prob {
                    path
                } else {
                    best
                }
            })
            .cloned();
        self.paths = next;
        let Some(best) = best else {
            return Ok(None);
        };
        let Some((phrase, level)) = self.graph.matched(best.state) else {
            return Ok(None);
        };
        let Some(heard) = best.probabilities.get(..level) else {
            return Ok(None);
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a phrase has a handful of tokens"
        )]
        let confidence = heard.iter().sum::<f32>() / level as f32;
        if best.trailing_blanks <= TRAILING_BLANKS || confidence < ACOUSTIC_THRESHOLD {
            return Ok(None);
        }
        let frames = &best.frames[best.frames.len() - level..];
        let (Some(&first), Some(&last)) = (frames.first(), frames.last()) else {
            return Ok(None);
        };
        self.paths = vec![Path::start()];
        let (phrase, ending) = self.phrases[phrase].clone();
        Ok(Some(SpotterHit {
            phrase,
            ending,
            start_sample: first * ENCODER_FRAME_SAMPLES,
            end_sample: (last + 1) * ENCODER_FRAME_SAMPLES,
            confidence: confidence.clamp(0.0, 1.0),
        }))
    }

    fn decode_chunk(&mut self) -> Result<Option<SpotterHit>, SpotterError> {
        if self.trailing_blanks >= SILENCE_BLANKS {
            self.restart_search();
        }
        let window = self
            .features
            .iter()
            .take(self.network.window)
            .flatten()
            .copied()
            .collect();
        self.features.drain(..self.network.shift);
        let (encoded, caches) = self
            .network
            .encode(window, std::mem::take(&mut self.caches))?;
        self.caches = caches;
        let mut hit = None;
        for row in encoded.chunks(self.network.dim) {
            if let Some(found) = self.advance(row, self.decoded)? {
                hit = Some(found);
            }
            self.decoded += 1;
        }
        self.trailing_blanks = self
            .paths
            .iter()
            .reduce(|best, path| {
                if best.log_prob < path.log_prob {
                    path
                } else {
                    best
                }
            })
            .map_or(0, |best| best.trailing_blanks);
        Ok(hit)
    }
}

impl KeywordSpotter for OnnxSpotter {
    fn reset(&mut self) {
        self.fbank = KaldiFbank::new();
        self.features.clear();
        self.decoded = 0;
        self.restart_search();
    }

    fn push(&mut self, frame: &Frame) -> Result<Option<SpotterHit>, SpotterError> {
        let samples: Vec<f32> = frame
            .iter()
            .map(|&sample| f32::from(sample) / 32_768.0)
            .collect();
        let mut mel = Vec::new();
        self.fbank.push(&samples, &mut mel);
        self.features.extend(mel);
        // sherpa-onnx starts a chunk only when more than a window of frames is ready.
        while self.features.len() > self.network.window {
            if let Some(hit) = self.decode_chunk()? {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_machine_boosts_phrases_and_recognises_them_through_shared_prefixes() {
        // Tokens 1 2 3 and 1 2 4 share a prefix; 9 is a phrase by itself.
        let graph = Graph::new(&[vec![1, 2, 3], vec![1, 2, 4], vec![9]]);
        let walk = |tokens: &[usize]| {
            let mut state = ROOT;
            let mut total = 0.0;
            for &token in tokens {
                let (score, next) = graph.step(state, token);
                total += score;
                state = next;
            }
            (total, graph.matched(state))
        };
        // Each token earns the boost, and finishing a phrase also pays out what it earned.
        assert_eq!(walk(&[1, 2, 3]), (6.0, Some((0, 3))));
        assert_eq!(walk(&[1, 2, 4]), (6.0, Some((1, 3))));
        assert_eq!(walk(&[9]), (2.0, Some((2, 1))));
        // A wrong turn takes back the boost the prefix earned, and the phrase is not matched.
        assert_eq!(walk(&[1, 2, 7]), (0.0, None));
        // A phrase that starts inside a failed one is still found.
        assert_eq!(walk(&[1, 1, 2, 3]), (6.0, Some((0, 3))));
    }

    fn varint(mut value: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let low = u8::try_from(value & 0x7f).expect("seven bits");
            value >>= 7;
            if value == 0 {
                out.push(low);
                return out;
            }
            out.push(low | 0x80);
        }
    }

    /// A length-delimited protobuf field.
    fn field(number: usize, payload: &[u8]) -> Vec<u8> {
        let mut out = varint(number << 3 | 2);
        out.extend(varint(payload.len()));
        out.extend_from_slice(payload);
        out
    }

    /// A model file with these (text, score, type) pieces and the given model type.
    fn model(pieces: &[(&str, f32, usize)], model_type: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for (text, score, kind) in pieces {
            let mut piece = field(1, text.as_bytes());
            piece.extend(varint(2 << 3 | 5));
            piece.extend(score.to_le_bytes());
            piece.extend(varint(3 << 3));
            piece.extend(varint(*kind));
            out.extend(field(1, &piece));
        }
        let mut trainer = varint(3 << 3);
        trainer.extend(varint(model_type));
        out.extend(field(2, &trainer));
        out
    }

    #[test]
    fn phrases_are_cut_into_the_pieces_with_the_best_total_score() {
        let bytes = model(
            &[
                ("<unk>", 0.0, 2),
                ("\u{2581}", -2.0, 1),
                ("A", -5.0, 1),
                ("B", -5.0, 1),
                ("AB", -10.0, 1),
                ("\u{2581}A", -6.0, 1),
                ("'", -3.0, 1),
            ],
            1,
        );
        let pieces = read_pieces(&bytes).expect("a unigram model");
        assert_eq!(pieces.unknown, Some(0));
        // "▁AB": ▁A + B = -11 beats ▁ + AB = -12 and ▁ + A + B = -12.
        assert_eq!(pieces.encode("ab"), Ok(vec![5, 3]));
        // Words are separated by the word-start piece; apostrophes are pieces of their own.
        assert_eq!(pieces.encode("a b"), Ok(vec![5, 1, 3]));
        assert_eq!(pieces.encode("a'b"), Ok(vec![5, 6, 3]));
        // Characters without a piece, after upper-casing, are all named.
        assert_eq!(pieces.encode("a2c-"), Err("2C-".to_owned()));
        assert!(read_pieces(&model(&[("A", -1.0, 1)], 2)).is_err());
    }
}

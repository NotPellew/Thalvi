//! Owns the measurement run: prompt cases, setups, and per-reply timings.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::{Config, TtsApi};
use crate::judge::Grade;
use crate::model::{ChatMessage, ChatModel, OpenAiChatModel};
use crate::sentence::SentenceSplitter;
use crate::tts::{AudioStream, OpenAiTts, Qwen3Tts, Tts, TtsError};

/// How long one reply may take, end to end, before it counts as failed.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// Bytes per second of the speech clients' audio (24 kHz, 16-bit, mono).
const BYTES_PER_SECOND: f64 = 48_000.0;

/// One prompt: the conversation so far and the user message to answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Case {
    /// Name used by `--only` and in results.
    pub id: String,
    /// Alternating user and assistant messages, ending with the user message
    /// to answer; earlier turns are sent as history like a session does.
    pub turns: Vec<String>,
}

impl Case {
    /// The messages sent to the model.
    #[must_use]
    pub fn history(&self) -> Vec<ChatMessage> {
        self.turns
            .iter()
            .enumerate()
            .map(|(index, text)| {
                if index % 2 == 0 {
                    ChatMessage::user(text.clone())
                } else {
                    ChatMessage::assistant(text.clone())
                }
            })
            .collect()
    }
}

/// Parses and validates a prompt set.
///
/// # Errors
///
/// Returns a plain message when the JSON is malformed, ids repeat, or a case
/// does not alternate user and assistant turns ending on a user turn.
pub fn parse_cases(json: &str) -> Result<Vec<Case>, String> {
    let cases: Vec<Case> =
        serde_json::from_str(json).map_err(|error| format!("invalid cases: {error}"))?;
    if cases.is_empty() {
        return Err("the cases file has no cases".to_string());
    }
    let mut seen = HashSet::new();
    for case in &cases {
        if !seen.insert(case.id.as_str()) {
            return Err(format!("case id {:?} appears twice", case.id));
        }
        if case.turns.len() % 2 == 0 || case.turns.iter().any(|turn| turn.trim().is_empty()) {
            return Err(format!(
                "case {:?} needs non-empty turns alternating user and assistant, ending on a user turn",
                case.id
            ));
        }
    }
    Ok(cases)
}

/// Speech client for whichever protocol the configuration selects.
#[derive(Debug, Clone)]
pub enum ConfiguredTts {
    /// `OpenAI` `/audio/speech`.
    OpenAi(OpenAiTts),
    /// Qwen3-TTS framed PCM stream.
    Qwen3(Qwen3Tts),
}

impl Tts for ConfiguredTts {
    async fn synthesize(&self, text: &str) -> Result<AudioStream, TtsError> {
        match self {
            Self::OpenAi(tts) => tts.synthesize(text).await,
            Self::Qwen3(tts) => tts.synthesize(text).await,
        }
    }
}

/// A named model and speech configuration to measure.
#[derive(Debug, Clone)]
pub struct Setup<M, T> {
    /// Short name shown in the table.
    pub name: String,
    /// What differs from other setups, for the table.
    pub label: String,
    /// The chat model.
    pub model: M,
    /// The speech client.
    pub tts: T,
}

/// Builds a setup from the normal environment (`base`) plus `overrides`, the
/// text of an env file whose lines win over it.
///
/// # Errors
///
/// Returns a plain message when the override file is malformed or the merged
/// configuration is invalid.
pub fn load_setup(
    name: &str,
    overrides: &str,
    base: impl Fn(&str) -> Option<String>,
) -> Result<Setup<OpenAiChatModel, ConfiguredTts>, String> {
    let mut overridden = HashMap::new();
    for line in dotenvy::from_read_iter(overrides.as_bytes()) {
        let (key, value) = line.map_err(|error| format!("invalid override file: {error}"))?;
        overridden.insert(key, value);
    }
    let config = Config::from_lookup(|key| overridden.get(key).cloned().or_else(|| base(key)))?;
    let voice = config.tts_voice.clone();
    let (tts, tts_name) = match config.tts_api {
        TtsApi::OpenAi { model, api_key } => (
            ConfiguredTts::OpenAi(OpenAiTts::new(
                config.tts_base_url,
                model.clone(),
                voice,
                api_key,
            )),
            model,
        ),
        TtsApi::Qwen3 => (
            ConfiguredTts::Qwen3(Qwen3Tts::new(config.tts_base_url, voice)),
            "qwen3-tts".to_string(),
        ),
    };
    let effort = config.llm_reasoning_effort.as_deref().unwrap_or("default");
    let label = format!("{} (effort {effort}), {tts_name}", config.llm_model);
    Ok(Setup {
        name: name.to_string(),
        label,
        model: OpenAiChatModel::new(
            config.llm_base_url,
            config.llm_model,
            config.llm_api_key,
            config.llm_reasoning_effort,
        ),
        tts,
    })
}

/// What one reply measured, in seconds from the request start.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Measured {
    /// First content token from the model.
    pub first_token_s: Option<f64>,
    /// First complete sentence from the sentence splitter.
    pub first_sentence_s: Option<f64>,
    /// First audio byte of the first sentence.
    pub first_audio_s: Option<f64>,
    /// Last audio byte of the whole reply; `None` unless it finished cleanly.
    pub end_to_end_s: Option<f64>,
    /// Sentences the splitter produced.
    pub sentences: usize,
    /// Seconds of audio synthesized.
    pub audio_s: f64,
    /// The model's reply text.
    pub reply: String,
    /// Why the reply failed, in plain words.
    pub error: Option<String>,
}

impl Measured {
    /// A reply that failed before measuring anything.
    #[must_use]
    pub fn failed(error: String) -> Self {
        Self {
            first_token_s: None,
            first_sentence_s: None,
            first_audio_s: None,
            end_to_end_s: None,
            sentences: 0,
            audio_s: 0.0,
            reply: String::new(),
            error: Some(error),
        }
    }
}

struct Produced {
    first_token: Option<f64>,
    first_sentence: Option<f64>,
    sentences: usize,
    reply: String,
    error: Option<String>,
}

struct Spoken {
    first_audio: Option<f64>,
    last_audio: Option<f64>,
    audio_bytes: usize,
    error: Option<String>,
}

/// Streams a reply through the sentence splitter into speech the way a session
/// does, timing each event from the moment the request starts.
pub async fn measure_reply(
    model: &impl ChatModel,
    tts: &impl Tts,
    history: &[ChatMessage],
) -> Measured {
    let start = Instant::now();
    let (sentence_tx, sentence_rx) = mpsc::unbounded_channel();
    let (produced, spoken) = tokio::join!(
        produce(model, history, start, sentence_tx),
        speak(tts, start, sentence_rx)
    );
    let mut error = produced.error.or(spoken.error);
    if error.is_none() && produced.sentences == 0 {
        error = Some("the model replied with no text".to_string());
    }
    let end_to_end_s = if error.is_none() {
        spoken.last_audio
    } else {
        None
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "audio byte counts are far below f64's exact integer range"
    )]
    let audio_s = spoken.audio_bytes as f64 / BYTES_PER_SECOND;
    Measured {
        first_token_s: produced.first_token,
        first_sentence_s: produced.first_sentence,
        first_audio_s: spoken.first_audio,
        end_to_end_s,
        sentences: produced.sentences,
        audio_s,
        reply: produced.reply,
        error,
    }
}

async fn produce(
    model: &impl ChatModel,
    history: &[ChatMessage],
    start: Instant,
    sentences: mpsc::UnboundedSender<String>,
) -> Produced {
    let mut produced = Produced {
        first_token: None,
        first_sentence: None,
        sentences: 0,
        reply: String::new(),
        error: None,
    };
    let mut deltas = match model.stream_reply(history).await {
        Ok(deltas) => deltas,
        Err(error) => {
            produced.error = Some(format!("The language model could not reply: {error}"));
            return produced;
        }
    };
    let mut splitter = SentenceSplitter::new();
    let emit = |produced: &mut Produced, sentence: String| {
        produced
            .first_sentence
            .get_or_insert_with(|| start.elapsed().as_secs_f64());
        produced.sentences += 1;
        // The speech side stops listening after its own failure.
        let _ = sentences.send(sentence);
    };
    while let Some(delta) = deltas.next().await {
        let delta = match delta {
            Ok(delta) => delta,
            Err(error) => {
                produced.error = Some(format!("The language model could not reply: {error}"));
                return produced;
            }
        };
        produced
            .first_token
            .get_or_insert_with(|| start.elapsed().as_secs_f64());
        produced.reply.push_str(&delta);
        for sentence in splitter.push(&delta) {
            emit(&mut produced, sentence);
        }
    }
    if let Some(tail) = splitter.finish() {
        emit(&mut produced, tail);
    }
    produced
}

async fn speak(
    tts: &impl Tts,
    start: Instant,
    mut sentences: mpsc::UnboundedReceiver<String>,
) -> Spoken {
    let mut spoken = Spoken {
        first_audio: None,
        last_audio: None,
        audio_bytes: 0,
        error: None,
    };
    while let Some(sentence) = sentences.recv().await {
        let mut audio = match tts.synthesize(&sentence).await {
            Ok(audio) => audio,
            Err(error) => {
                spoken.error = Some(format!("Speech synthesis failed: {error}"));
                return spoken;
            }
        };
        while let Some(chunk) = audio.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    spoken.error = Some(format!("Speech synthesis failed: {error}"));
                    return spoken;
                }
            };
            if chunk.is_empty() {
                continue;
            }
            let now = start.elapsed().as_secs_f64();
            spoken.first_audio.get_or_insert(now);
            spoken.last_audio = Some(now);
            spoken.audio_bytes += chunk.len();
        }
    }
    spoken
}

/// One measured reply for one setup, case and run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    /// Name of the setup.
    pub setup: String,
    /// Id of the case.
    pub case: String,
    /// Zero-based run number for this setup and case.
    pub run: usize,
    /// The timings and reply.
    #[serde(flatten)]
    pub measured: Measured,
    /// The judge's grade; `None` until judged or when judging is off.
    pub grade: Option<Grade>,
    /// How long the judge call took.
    pub judge_s: Option<f64>,
}

/// Measures every case `runs` times per setup, interleaved: for each case and
/// run, every setup in turn, so all setups see the same network noise.
/// `on_sample` sees each sample as it completes.
pub async fn run_all<M: ChatModel, T: Tts>(
    setups: &[Setup<M, T>],
    cases: &[Case],
    runs: usize,
    mut on_sample: impl FnMut(&Sample),
) -> Vec<Sample> {
    let mut samples = Vec::new();
    for case in cases {
        let history = case.history();
        for run in 0..runs {
            for setup in setups {
                let measured = tokio::time::timeout(
                    REPLY_TIMEOUT,
                    measure_reply(&setup.model, &setup.tts, &history),
                )
                .await
                .unwrap_or_else(|_| {
                    Measured::failed(format!(
                        "The reply took longer than {} seconds",
                        REPLY_TIMEOUT.as_secs()
                    ))
                });
                let sample = Sample {
                    setup: setup.name.clone(),
                    case: case.id.clone(),
                    run,
                    measured,
                    grade: None,
                    judge_s: None,
                };
                on_sample(&sample);
                samples.push(sample);
            }
        }
    }
    samples
}

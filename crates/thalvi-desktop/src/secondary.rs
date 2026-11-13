//! Owns the optional secondary transcription of a finished capture: its validated settings and the `OpenAI` or Meta WAV upload.

use std::{fmt, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{frame::Frame, recognizer::Upsampler};

const TIMEOUT: Duration = Duration::from_secs(10);
const SAMPLE_RATE: u32 = 24_000;
const BOUNDARY: &str = "thalvi-capture-boundary-7d1f0c2a9b3e";

/// `url` as an endpoint's base address without a trailing slash, when it is an `http` or `https`
/// URL with a host and no credentials, query or fragment.
pub(crate) fn base_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    (matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none())
    .then(|| parsed.as_str().trim_end_matches('/').to_string())
}

/// Which transcription API the secondary endpoint speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SecondaryApi {
    #[default]
    OpenAi,
    Meta,
}

/// Why secondary settings were refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecondarySttError {
    #[error(
        "stt.secondary.url must be an http or https URL with a host, no credentials, query or fragment."
    )]
    Url,
    #[error("stt.secondary.model must be nonempty text.")]
    Model,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecondaryStt {
    #[serde(default)]
    enabled: bool,
    url: String,
    model: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    api: SecondaryApi,
}

/// Settings of the secondary transcription endpoint; it runs only when `enabled`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawSecondaryStt")]
pub struct SecondaryStt {
    enabled: bool,
    url: String,
    model: String,
    api_key: String,
    api: SecondaryApi,
}

impl fmt::Debug for SecondaryStt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondaryStt")
            .field("enabled", &self.enabled)
            .field("url", &self.url)
            .field("model", &self.model)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl SecondaryStt {
    /// # Errors
    /// Fails when `enabled` and `url` is not an `http` or `https` base URL without credentials, query or fragment, or `model` is blank. When not `enabled`, the endpoint is kept as written without validation.
    pub fn new(
        url: &str,
        model: &str,
        api_key: String,
        api: SecondaryApi,
        enabled: bool,
    ) -> Result<Self, SecondarySttError> {
        if !enabled {
            return Ok(Self {
                enabled,
                url: url.trim().trim_end_matches('/').to_string(),
                model: model.trim().to_string(),
                api_key,
                api,
            });
        }
        let url = base_url(url).ok_or(SecondarySttError::Url)?;
        let model = model.trim();
        if model.is_empty() {
            return Err(SecondarySttError::Model);
        }
        Ok(Self {
            enabled,
            url,
            model: model.to_string(),
            api_key,
            api,
        })
    }

    /// Whether the secondary transcription runs.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The same settings, run or not.
    #[must_use]
    pub fn with_enabled(self, enabled: bool) -> Self {
        Self { enabled, ..self }
    }

    /// The endpoint's base address.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.url
    }

    /// The key sent to the endpoint; empty when it needs none.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The model named in each request.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Which request format the endpoint speaks.
    #[must_use]
    pub fn api(&self) -> SecondaryApi {
        self.api
    }
}

impl TryFrom<RawSecondaryStt> for SecondaryStt {
    type Error = SecondarySttError;

    fn try_from(raw: RawSecondaryStt) -> Result<Self, Self::Error> {
        Self::new(&raw.url, &raw.model, raw.api_key, raw.api, raw.enabled)
    }
}

/// A two-letter ISO 639-1 language code, kept lower-case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Language([char; 2]);

/// Why a language was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the language must be a two-letter ISO 639-1 code such as de, or empty.")]
pub struct LanguageError;

impl Language {
    /// `text` as a language code, or none when it is blank.
    ///
    /// # Errors
    /// Fails unless `text` is blank or exactly two ASCII letters.
    pub fn parse(text: &str) -> Result<Option<Self>, LanguageError> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let mut letters = text.chars();
        match (letters.next(), letters.next(), letters.next()) {
            (Some(a), Some(b), None) if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() => {
                Ok(Some(Self([a.to_ascii_lowercase(), b.to_ascii_lowercase()])))
            }
            _ => Err(LanguageError),
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.0[0], self.0[1])
    }
}

/// How a live OpenAI-compatible entry sends the capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveRequest {
    /// A streaming multipart upload to `/audio/transcriptions`.
    #[default]
    Transcriptions,
    /// One JSON request to `/chat/completions` with the WAV as `input_audio`.
    Chat,
}

impl LiveRequest {
    /// The name the file and the metrics use.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Transcriptions => "transcriptions",
            Self::Chat => "chat",
        }
    }
}

/// A live OpenAI-compatible entry: where it listens, how it asks and in which language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAiLive {
    pub settings: SecondaryStt,
    pub request: LiveRequest,
    pub language: Option<Language>,
}

/// Why the secondary transcription produced no text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecondaryError {
    #[error("The second transcription took too long.")]
    Timeout,
    #[error("The second transcription could not be reached.")]
    Unreachable,
    #[error("The second transcription failed with status {0}.")]
    Status(u16),
    #[error("The second transcription returned an unexpected answer.")]
    BadResponse,
}

/// 24 kHz mono 16-bit WAV of the captured frames, upsampled exactly as the live stream was.
fn wav(frames: &[Frame]) -> Vec<u8> {
    let mut upsampler = Upsampler::default();
    let pcm: Vec<u8> = frames
        .iter()
        .flat_map(|frame| upsampler.push(frame))
        .flat_map(i16::to_le_bytes)
        .collect();
    let length = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&length.saturating_add(36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&pcm);
    out
}

/// Sends the capture to the secondary endpoint and returns its raw transcript.
///
/// # Errors
/// Fails with [`SecondaryError`] on timeout, an unreachable or failing endpoint, or an unexpected answer.
pub async fn transcribe(
    client: &reqwest::Client,
    settings: &SecondaryStt,
    frames: &[Frame],
) -> Result<String, SecondaryError> {
    let (endpoint, header, file_field, text_field) = match settings.api {
        SecondaryApi::OpenAi => (
            "/audio/transcriptions",
            format!(
                "Content-Disposition: form-data; name=\"model\"\r\n\r\n{}",
                settings.model
            ),
            "file",
            "text",
        ),
        SecondaryApi::Meta => (
            "/asr/transcribe",
            format!(
                "Content-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n{}",
                json!({"mode": "PUSH_TO_TALK", "model": settings.model, "audioEncoding": "WAV"})
            ),
            "audio",
            "transcript",
        ),
    };
    let mut body = format!(
        "--{BOUNDARY}\r\n{header}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"capture.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(&wav(frames));
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let mut request = client
        .post(format!("{}{endpoint}", settings.url))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("Accept", "application/json")
        .body(body);
    if !settings.api_key.is_empty() {
        request = request.bearer_auth(&settings.api_key);
    }
    let answer = async {
        let response = request
            .send()
            .await
            .map_err(|_| SecondaryError::Unreachable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SecondaryError::Status(status.as_u16()));
        }
        response
            .json::<Value>()
            .await
            .map_err(|_| SecondaryError::BadResponse)
    };
    let answer = tokio::time::timeout(TIMEOUT, answer)
        .await
        .map_err(|_| SecondaryError::Timeout)??;
    match answer.get(text_field) {
        Some(Value::String(text)) => Ok(text.clone()),
        _ => Err(SecondaryError::BadResponse),
    }
}

/// Sends the capture as the entry's request kind. A transcriptions request streams: it emits
/// cumulative text as SSE arrives. A chat request emits nothing before its one answer.
///
/// # Errors
///
/// Returns an error when the endpoint is unreachable, rejects the request, times out, or returns
/// an unsupported response.
pub async fn transcribe_events(
    client: &reqwest::Client,
    entry: &OpenAiLive,
    frames: &[Frame],
    emit: impl FnMut(crate::recognizer::RecognizerEvent),
) -> Result<String, SecondaryError> {
    match entry.request {
        LiveRequest::Transcriptions => transcribe_stream(client, entry, frames, emit).await,
        LiveRequest::Chat => transcribe_chat(client, entry, frames).await,
    }
}

async fn transcribe_stream(
    client: &reqwest::Client,
    entry: &OpenAiLive,
    frames: &[Frame],
    mut emit: impl FnMut(crate::recognizer::RecognizerEvent),
) -> Result<String, SecondaryError> {
    let settings = &entry.settings;
    let language = entry.language.map_or_else(String::new, |language| {
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\n{language}\r\n")
    });
    let mut body = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"stream\"\r\n\r\ntrue\r\n{language}--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"capture.wav\"\r\nContent-Type: audio/wav\r\n\r\n", settings.model).into_bytes();
    body.extend_from_slice(&wav(frames));
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let mut request = client
        .post(format!("{}/audio/transcriptions", settings.url))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("Accept", "text/event-stream, application/json")
        .body(body);
    if !settings.api_key.is_empty() {
        request = request.bearer_auth(settings.api_key());
    }
    let upload = async {
        let response = request
            .send()
            .await
            .map_err(|_| SecondaryError::Unreachable)?;
        if !response.status().is_success() {
            return Err(SecondaryError::Status(response.status().as_u16()));
        }
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"))
        {
            let answer: Value = response
                .json()
                .await
                .map_err(|_| SecondaryError::BadResponse)?;
            let text = answer
                .get("text")
                .and_then(Value::as_str)
                .ok_or(SecondaryError::BadResponse)?
                .to_owned();
            return Ok(text);
        }
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut transcript = String::new();
        let mut final_text = None;
        while let Some(chunk) = stream.next().await {
            pending.extend_from_slice(&chunk.map_err(|_| SecondaryError::Unreachable)?);
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = pending.drain(..=newline).collect();
                let line = String::from_utf8_lossy(&line);
                if let Some(data) = line.trim().strip_prefix("data: ") {
                    if data == "[DONE]" {
                        break;
                    }
                    let value: Value =
                        serde_json::from_str(data).map_err(|_| SecondaryError::BadResponse)?;
                    match value.get("type").and_then(Value::as_str) {
                        Some("transcript.text.delta") => {
                            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                                transcript.push_str(delta);
                                emit(crate::recognizer::RecognizerEvent::Partial(
                                    transcript.clone(),
                                ));
                            }
                        }
                        Some("transcript.text.done") => {
                            if let Some(text) = value.get("text").and_then(Value::as_str) {
                                final_text = Some(text.to_owned());
                            }
                        }
                        None => {
                            if let Some(text) = value.get("text").and_then(Value::as_str) {
                                if !transcript.is_empty()
                                    && !transcript.chars().last().is_some_and(char::is_whitespace)
                                    && !text.chars().next().is_some_and(char::is_whitespace)
                                {
                                    transcript.push(' ');
                                }
                                transcript.push_str(text);
                                emit(crate::recognizer::RecognizerEvent::Partial(
                                    transcript.clone(),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(final_text.unwrap_or(transcript))
    };
    tokio::time::timeout(TIMEOUT, upload)
        .await
        .map_err(|_| SecondaryError::Timeout)?
}

/// One non-streaming chat completion that carries the WAV as `input_audio`.
async fn transcribe_chat(
    client: &reqwest::Client,
    entry: &OpenAiLive,
    frames: &[Frame],
) -> Result<String, SecondaryError> {
    let settings = &entry.settings;
    let mut body = json!({
        "model": settings.model,
        "messages": [{"role": "user", "content": [{
            "type": "input_audio",
            "input_audio": {"data": STANDARD.encode(wav(frames)), "format": "wav"},
        }]}],
    });
    if let (Some(language), Some(object)) = (entry.language, body.as_object_mut()) {
        object.insert("language".to_owned(), json!(language.to_string()));
    }
    let mut request = client
        .post(format!("{}/chat/completions", settings.url))
        .header("Accept", "application/json")
        .json(&body);
    if !settings.api_key.is_empty() {
        request = request.bearer_auth(&settings.api_key);
    }
    let exchange = async {
        let response = request
            .send()
            .await
            .map_err(|_| SecondaryError::Unreachable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SecondaryError::Status(status.as_u16()));
        }
        let answer: Value = response
            .json()
            .await
            .map_err(|_| SecondaryError::BadResponse)?;
        answer
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(SecondaryError::BadResponse)
    };
    tokio::time::timeout(TIMEOUT, exchange)
        .await
        .map_err(|_| SecondaryError::Timeout)?
}

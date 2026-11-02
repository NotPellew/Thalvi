//! Owns the optional secondary transcription of a finished capture: its validated settings and the `OpenAI` or Meta WAV upload.

use std::{fmt, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{frame::Frame, recognizer::Upsampler};

const TIMEOUT: Duration = Duration::from_secs(10);
const SAMPLE_RATE: u32 = 24_000;
const BOUNDARY: &str = "thalvi-capture-boundary-7d1f0c2a9b3e";

/// Which transcription API the secondary endpoint speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    url: String,
    model: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    api: SecondaryApi,
}

/// Validated settings of the secondary transcription endpoint.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawSecondaryStt")]
pub struct SecondaryStt {
    url: String,
    model: String,
    api_key: String,
    api: SecondaryApi,
}

impl fmt::Debug for SecondaryStt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondaryStt")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl SecondaryStt {
    /// # Errors
    /// Fails when `url` is not an `http` or `https` base URL without credentials, query or fragment, or `model` is blank.
    pub fn new(
        url: &str,
        model: &str,
        api_key: String,
        api: SecondaryApi,
    ) -> Result<Self, SecondarySttError> {
        let parsed = reqwest::Url::parse(url.trim()).map_err(|_| SecondarySttError::Url)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(SecondarySttError::Url);
        }
        let model = model.trim();
        if model.is_empty() {
            return Err(SecondarySttError::Model);
        }
        Ok(Self {
            url: parsed.as_str().trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key,
            api,
        })
    }
}

impl TryFrom<RawSecondaryStt> for SecondaryStt {
    type Error = SecondarySttError;

    fn try_from(raw: RawSecondaryStt) -> Result<Self, Self::Error> {
        Self::new(&raw.url, &raw.model, raw.api_key, raw.api)
    }
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

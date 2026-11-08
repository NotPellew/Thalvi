//! Owns the optional secondary transcription of a finished capture: its validated settings and the `OpenAI` or Meta WAV upload.

use std::{fmt, time::Duration};

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
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
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

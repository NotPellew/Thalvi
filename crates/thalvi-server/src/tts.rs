//! Owns the text-to-speech trait and the streamed `OpenAI` speech client.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

/// A speech synthesis failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsError(pub String);

impl std::fmt::Display for TtsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TtsError {}

/// A stream of reply audio chunks (24 kHz s16le mono PCM).
pub type AudioStream = Pin<Box<dyn Stream<Item = Result<Bytes, TtsError>> + Send>>;

/// Text to speech.
pub trait Tts: Send + Sync {
    /// Starts synthesizing `text`, returning streamed PCM audio.
    fn synthesize(
        &self,
        text: &str,
    ) -> impl std::future::Future<Output = Result<AudioStream, TtsError>> + Send;
}

/// Speech client backed by an OpenAI-compatible `/audio/speech` endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiTts {
    client: reqwest::Client,
    base_url: String,
    model: String,
    voice: String,
    api_key: String,
}

impl OpenAiTts {
    /// Creates a speech client from validated configuration values.
    #[must_use]
    pub fn new(base_url: String, model: String, voice: String, api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            voice,
            api_key,
        }
    }

    /// Builds the JSON request body for one sentence.
    #[must_use]
    pub fn request_body(&self, text: &str) -> Value {
        json!({
            "model": self.model,
            "voice": self.voice,
            "input": text,
            "response_format": "pcm",
            "stream": true,
        })
    }
}

impl Tts for OpenAiTts {
    async fn synthesize(&self, text: &str) -> Result<AudioStream, TtsError> {
        let url = format!("{}/audio/speech", self.base_url);
        let mut request = self.client.post(&url).json(&self.request_body(text));
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request
            .send()
            .await
            .map_err(|error| TtsError(format!("speech synthesis could not be reached: {error}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let _ = response.text().await;
            return Err(TtsError(format!(
                "speech synthesis returned an error ({status})"
            )));
        }
        let stream = response.bytes_stream().map(|item| {
            item.map_err(|error| TtsError(format!("speech synthesis stream failed: {error}")))
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAiTts;

    #[test]
    fn request_body_matches_contract() {
        let tts = OpenAiTts::new(
            "http://x/v1".to_string(),
            "kokoro".to_string(),
            "af_heart".to_string(),
            String::new(),
        );
        let body = tts.request_body("Hello there.");
        assert_eq!(body["model"], "kokoro");
        assert_eq!(body["voice"], "af_heart");
        assert_eq!(body["input"], "Hello there.");
        assert_eq!(body["response_format"], "pcm");
        assert_eq!(body["stream"], true);
    }
}

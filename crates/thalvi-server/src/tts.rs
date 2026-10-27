//! Owns the text-to-speech trait and the streamed `OpenAI` and Qwen3-TTS speech clients.

use std::pin::Pin;

use bytes::{Buf, Bytes, BytesMut};
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
            "stream_format": "audio",
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
        let stream = send(request).await?.bytes_stream().map(|item| {
            item.map_err(|error| TtsError(format!("speech synthesis stream failed: {error}")))
        });
        Ok(Box::pin(stream))
    }
}

/// Speech client backed by a Qwen3-TTS server's framed `/audio/speech/pcm-stream`.
///
/// Yields each frame's PCM as it arrives; the stream fails if the body ends
/// before the zero-length end frame.
#[derive(Debug, Clone)]
pub struct Qwen3Tts {
    client: reqwest::Client,
    base_url: String,
    voice: String,
}

impl Qwen3Tts {
    /// Creates a speech client from validated configuration values.
    #[must_use]
    pub fn new(base_url: String, voice: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            voice,
        }
    }
}

impl Tts for Qwen3Tts {
    async fn synthesize(&self, text: &str) -> Result<AudioStream, TtsError> {
        let url = format!("{}/audio/speech/pcm-stream", self.base_url);
        let request = self
            .client
            .post(&url)
            .json(&json!({"input": text, "voice": self.voice}));
        let body = Box::pin(send(request).await?.bytes_stream());
        let frames =
            futures_util::stream::unfold(Some((body, BytesMut::new())), |state| async move {
                let (mut body, mut buffer) = state?;
                loop {
                    if let Some(&header) = buffer.first_chunk::<4>() {
                        let len = u32::from_be_bytes(header) as usize;
                        if buffer.len() >= 4 + len {
                            buffer.advance(4);
                            if len == 0 {
                                return None;
                            }
                            let frame = buffer.split_to(len).freeze();
                            return Some((Ok(frame), Some((body, buffer))));
                        }
                    }
                    let error = match body.next().await {
                        Some(Ok(chunk)) => {
                            buffer.extend_from_slice(&chunk);
                            continue;
                        }
                        Some(Err(error)) => format!("speech synthesis stream failed: {error}"),
                        None => {
                            "speech synthesis stopped before the sentence was finished".to_string()
                        }
                    };
                    return Some((Err(TtsError(error)), None));
                }
            });
        Ok(Box::pin(frames))
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, TtsError> {
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
    Ok(response)
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
        assert_eq!(body["stream_format"], "audio");
    }
}

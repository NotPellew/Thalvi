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

    /// Like [`Tts::synthesize`], in `voice` instead of the configured one.
    ///
    /// The default ignores `voice`, for engines with a single voice.
    fn synthesize_with_voice(
        &self,
        text: &str,
        _voice: &str,
    ) -> impl std::future::Future<Output = Result<AudioStream, TtsError>> + Send {
        self.synthesize(text)
    }
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

    /// Builds the JSON request body for one sentence in the configured voice.
    #[must_use]
    pub fn request_body(&self, text: &str) -> Value {
        self.request_body_with_voice(text, &self.voice)
    }

    /// Builds the JSON request body for one sentence in `voice`.
    #[must_use]
    pub fn request_body_with_voice(&self, text: &str, voice: &str) -> Value {
        json!({
            "model": self.model,
            "voice": voice,
            "input": text,
            "response_format": "pcm",
            "stream_format": "audio",
        })
    }
}

impl Tts for OpenAiTts {
    async fn synthesize(&self, text: &str) -> Result<AudioStream, TtsError> {
        self.synthesize_with_voice(text, &self.voice).await
    }

    async fn synthesize_with_voice(
        &self,
        text: &str,
        voice: &str,
    ) -> Result<AudioStream, TtsError> {
        let url = format!("{}/audio/speech", self.base_url);
        let mut request = self
            .client
            .post(&url)
            .json(&self.request_body_with_voice(text, voice));
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
        self.synthesize_with_voice(text, &self.voice).await
    }

    async fn synthesize_with_voice(
        &self,
        text: &str,
        voice: &str,
    ) -> Result<AudioStream, TtsError> {
        let url = format!("{}/audio/speech/pcm-stream", self.base_url);
        let request = self
            .client
            .post(&url)
            .json(&json!({"input": text, "voice": voice}));
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
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::routing::post;
    use futures_util::StreamExt;
    use serde_json::Value;

    use super::{OpenAiTts, Qwen3Tts, Tts};

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

    type Bodies = Arc<Mutex<Vec<Value>>>;

    async fn record(State(bodies): State<Bodies>, axum::Json(body): axum::Json<Value>) -> Body {
        bodies.lock().expect("bodies lock").push(body);
        // A zero-length end frame, which Qwen3Tts needs and OpenAiTts treats as bytes.
        Body::from(vec![0, 0, 0, 0])
    }

    async fn serve(path: &str) -> (String, Bodies) {
        let bodies = Bodies::default();
        let app = Router::new()
            .route(path, post(record))
            .with_state(Arc::clone(&bodies));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}"), bodies)
    }

    #[tokio::test]
    async fn requested_voice_reaches_each_engine_and_plain_synthesis_keeps_the_configured_one() {
        let (url, openai_bodies) = serve("/audio/speech").await;
        let openai = OpenAiTts::new(
            url,
            "kokoro".to_string(),
            "af_heart".to_string(),
            String::new(),
        );
        let (url, qwen_bodies) = serve("/audio/speech/pcm-stream").await;
        let qwen = Qwen3Tts::new(url, "af_heart".to_string());

        for stream in [
            openai.synthesize_with_voice("Hi.", "sohee").await,
            openai.synthesize("Hi.").await,
            qwen.synthesize_with_voice("Hi.", "sohee").await,
            qwen.synthesize("Hi.").await,
        ] {
            let mut stream = stream.expect("request accepted");
            while stream.next().await.is_some() {}
        }

        for bodies in [openai_bodies, qwen_bodies] {
            let voices: Vec<Value> = bodies
                .lock()
                .expect("bodies lock")
                .iter()
                .map(|body| body["voice"].clone())
                .collect();
            assert_eq!(voices, ["sohee", "af_heart"]);
        }
    }
}

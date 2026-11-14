//! Owns the text-to-speech trait and the streamed `OpenAI` and Qwen3-TTS speech clients.

use std::pin::Pin;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde_json::{Value, json};

use crate::model::stall_client;

/// How long the speech endpoint may go without sending a byte before the call fails.
const STALL_DEADLINE: Duration = Duration::from_secs(10);

/// A speech synthesis failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsError {
    /// No byte arrived within the stall deadline.
    Stalled,
    /// The call failed for the reason given, in words fit for the user.
    Failed(String),
}

impl std::fmt::Display for TtsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stalled => f.write_str("the speech engine stopped responding"),
            Self::Failed(reason) => f.write_str(reason),
        }
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
            client: stall_client(STALL_DEADLINE),
            base_url,
            model,
            voice,
            api_key,
        }
    }

    #[cfg(test)]
    fn with_stall_deadline(mut self, deadline: Duration) -> Self {
        self.client = stall_client(deadline);
        self
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
        let stream = send(request).await?.bytes_stream().map_err(stream_error);
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
            client: stall_client(STALL_DEADLINE),
            base_url,
            voice,
        }
    }

    #[cfg(test)]
    fn with_stall_deadline(mut self, deadline: Duration) -> Self {
        self.client = stall_client(deadline);
        self
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
        let body = Box::pin(send(request).await?.bytes_stream().map_err(stream_error));
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
                        Some(Err(error)) => error,
                        None => TtsError::Failed(
                            "speech synthesis stopped before the sentence was finished".to_string(),
                        ),
                    };
                    return Some((Err(error), None));
                }
            });
        Ok(Box::pin(frames))
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, TtsError> {
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            return TtsError::Stalled;
        }
        TtsError::Failed(format!(
            "speech synthesis could not be reached: {}",
            error.without_url()
        ))
    })?;
    if !response.status().is_success() {
        let status = response.status();
        let _ = response.text().await;
        return Err(TtsError::Failed(format!(
            "speech synthesis returned an error ({status})"
        )));
    }
    Ok(response)
}

fn stream_error(error: reqwest::Error) -> TtsError {
    if error.is_timeout() {
        return TtsError::Stalled;
    }
    TtsError::Failed(format!(
        "speech synthesis stream failed: {}",
        error.without_url()
    ))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use serde_json::Value;

    use super::{OpenAiTts, Qwen3Tts, Tts, TtsError};

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

    #[tokio::test]
    async fn synthesis_transport_errors_do_not_include_the_endpoint_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let port = listener.local_addr().expect("listener address").port();
        drop(listener);

        let tts = OpenAiTts::new(
            format!("http://127.0.0.1:{port}/v1"),
            "kokoro".to_string(),
            "af_heart".to_string(),
            String::new(),
        );
        let Err(error) = tts.synthesize("Hello.").await else {
            panic!("closed local port should fail");
        };
        let message = error.to_string();
        assert!(message.contains("could not be reached"), "{message}");
        assert!(!message.contains("127.0.0.1"), "{message}");
        assert!(!message.contains(&port.to_string()), "{message}");
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
    #[derive(Clone, Copy)]
    enum Endpoint {
        Silent,
        HeadersThenSilent,
        AudioThenSilent,
        SlowButSteady,
    }

    const DEADLINE: Duration = Duration::from_millis(250);
    const GAP: Duration = Duration::from_millis(80);

    /// The bytes one chunk of audio takes on the wire: raw, or in a Qwen3-TTS frame.
    fn wire(framed: bool, pcm: &[u8]) -> Bytes {
        let mut bytes = Vec::new();
        if framed {
            bytes.extend_from_slice(&u32::try_from(pcm.len()).expect("small").to_be_bytes());
        }
        bytes.extend_from_slice(pcm);
        Bytes::from(bytes)
    }

    async fn respond(endpoint: Endpoint, framed: bool) -> Response {
        match endpoint {
            Endpoint::Silent => std::future::pending().await,
            Endpoint::HeadersThenSilent => {
                Body::from_stream(futures_util::stream::pending::<Result<Bytes, Infallible>>())
                    .into_response()
            }
            Endpoint::AudioThenSilent => Body::from_stream(
                futures_util::stream::iter(
                    [wire(framed, &[1, 2]), wire(framed, &[3, 4])].map(Ok::<_, Infallible>),
                )
                .chain(futures_util::stream::pending()),
            )
            .into_response(),
            Endpoint::SlowButSteady => {
                Body::from_stream(futures_util::stream::unfold(0, move |sent| async move {
                    tokio::time::sleep(GAP).await;
                    match sent {
                        0..8 => Some((Ok::<_, Infallible>(wire(framed, &[5, 6])), sent + 1)),
                        8 if framed => Some((Ok(wire(true, &[])), sent + 1)),
                        _ => None,
                    }
                }))
                .into_response()
            }
        }
    }

    /// Whether the headers arrived, the audio delivered, and how the stream ended.
    async fn synthesize_from<T: Tts>(tts: &T) -> (bool, Vec<u8>, Option<TtsError>) {
        let asked = tokio::time::timeout(Duration::from_secs(10), async {
            let mut stream = match tts.synthesize("Hello.").await {
                Ok(stream) => stream,
                Err(error) => return (false, Vec::new(), Some(error)),
            };
            let mut audio = Vec::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => audio.extend_from_slice(&chunk),
                    Err(error) => return (true, audio, Some(error)),
                }
            }
            (true, audio, None)
        })
        .await;
        asked.expect("the call must end under the stall deadline, not hang")
    }

    async fn serve_endpoint(path: &'static str, endpoint: Endpoint, framed: bool) -> String {
        let app = Router::new().route(path, post(move || respond(endpoint, framed)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    #[tokio::test]
    async fn a_quiet_speech_endpoint_stalls_the_call_but_a_steady_stream_never_does() {
        let steady = [5, 6].repeat(8);
        let cases = [
            ("never answers", Endpoint::Silent, false, vec![], true),
            (
                "headers, then silence",
                Endpoint::HeadersThenSilent,
                true,
                vec![],
                true,
            ),
            (
                "audio, then silence",
                Endpoint::AudioThenSilent,
                true,
                vec![1, 2, 3, 4],
                true,
            ),
            (
                "slow but steady",
                Endpoint::SlowButSteady,
                true,
                steady,
                false,
            ),
        ];
        for (name, endpoint, headers, audio, stalls) in cases {
            let openai = OpenAiTts::new(
                serve_endpoint("/audio/speech", endpoint, false).await,
                "kokoro".to_string(),
                "af_heart".to_string(),
                String::new(),
            )
            .with_stall_deadline(DEADLINE);
            let qwen = Qwen3Tts::new(
                serve_endpoint("/audio/speech/pcm-stream", endpoint, true).await,
                "af_heart".to_string(),
            )
            .with_stall_deadline(DEADLINE);
            for (engine, (got_headers, got_audio, error)) in [
                ("openai", synthesize_from(&openai).await),
                ("qwen3", synthesize_from(&qwen).await),
            ] {
                assert_eq!(got_headers, headers, "{engine} {name}");
                assert_eq!(got_audio, audio, "{engine} {name}");
                assert_eq!(
                    error,
                    stalls.then_some(TtsError::Stalled),
                    "{engine} {name}"
                );
            }
        }
    }
}

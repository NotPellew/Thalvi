//! Owns the chat model trait and the streamed Chat Completions client.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};

/// System prompt sent with every model request.
pub const SYSTEM_PROMPT: &str = "You are Thalvi, a voice assistant for when the user's hands and eyes are busy. Reply in short spoken sentences. Do not use markdown, lists, or emoji.";

/// One conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    /// Either `user` or `assistant`.
    pub role: &'static str,
    /// The message text.
    pub content: String,
}

impl ChatMessage {
    /// Creates a user message.
    #[must_use]
    pub fn user(content: String) -> Self {
        Self {
            role: "user",
            content,
        }
    }

    /// Creates an assistant message.
    #[must_use]
    pub fn assistant(content: String) -> Self {
        Self {
            role: "assistant",
            content,
        }
    }
}

/// A model call failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}

/// A stream of model text deltas.
pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<String, ModelError>> + Send>>;

struct SseState {
    bytes: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    pending: std::collections::VecDeque<Result<String, ModelError>>,
    done: bool,
}

/// The language model.
pub trait ChatModel: Send + Sync {
    /// Starts a streamed reply for `history`, returning its text deltas.
    fn stream_reply(
        &self,
        history: &[ChatMessage],
    ) -> impl std::future::Future<Output = Result<DeltaStream, ModelError>> + Send;
}

/// Chat model backed by an OpenAI-compatible Chat Completions endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiChatModel {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    reasoning_effort: Option<String>,
}

impl OpenAiChatModel {
    /// Creates a model client from validated configuration values.
    #[must_use]
    pub fn new(
        base_url: String,
        model: String,
        api_key: String,
        reasoning_effort: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            api_key,
            reasoning_effort,
        }
    }

    /// Builds the JSON request body for a streamed completion.
    #[must_use]
    pub fn request_body(&self, history: &[ChatMessage]) -> Value {
        let mut messages = vec![json!({"role": "system", "content": SYSTEM_PROMPT})];
        for message in history {
            messages.push(json!({"role": message.role, "content": message.content}));
        }
        let mut body = json!({
            "model": self.model,
            "stream": true,
            "messages": messages,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }
}

impl ChatModel for OpenAiChatModel {
    async fn stream_reply(&self, history: &[ChatMessage]) -> Result<DeltaStream, ModelError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self.client.post(&url).json(&self.request_body(history));
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request.send().await.map_err(|error| {
            ModelError(format!("the language model could not be reached: {error}"))
        })?;
        if !response.status().is_success() {
            let status = response.status();
            let _ = response.text().await;
            return Err(ModelError(format!(
                "the language model returned an error ({status})"
            )));
        }
        let state = SseState {
            bytes: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            pending: std::collections::VecDeque::new(),
            done: false,
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((item, state));
                }
                if state.done {
                    return None;
                }
                match state.bytes.next().await {
                    Some(Ok(chunk)) => {
                        state.buffer.extend_from_slice(&chunk);
                        while let Some(newline) =
                            state.buffer.iter().position(|&byte| byte == b'\n')
                        {
                            let line: Vec<u8> = state.buffer.drain(..=newline).collect();
                            match parse_sse_line(String::from_utf8_lossy(&line).trim()) {
                                SseLine::Delta(text) => state.pending.push_back(Ok(text)),
                                SseLine::Done => state.done = true,
                                SseLine::Ignore => {}
                            }
                        }
                    }
                    Some(Err(error)) => {
                        state.done = true;
                        return Some((
                            Err(ModelError(format!(
                                "the language model stream failed: {error}"
                            ))),
                            state,
                        ));
                    }
                    None => {
                        let tail = std::mem::take(&mut state.buffer);
                        if let SseLine::Delta(text) =
                            parse_sse_line(String::from_utf8_lossy(&tail).trim())
                        {
                            state.pending.push_back(Ok(text));
                        }
                        state.done = true;
                    }
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

/// What one SSE line from the model stream means.
#[derive(Debug, PartialEq, Eq)]
pub enum SseLine {
    /// A text delta.
    Delta(String),
    /// The `[DONE]` terminator.
    Done,
    /// A line that carries no delta.
    Ignore,
}

/// Parses one SSE line from a Chat Completions stream.
#[must_use]
pub fn parse_sse_line(line: &str) -> SseLine {
    let Some(data) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return SseLine::Done;
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return SseLine::Ignore;
    };
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .map_or(SseLine::Ignore, |content| {
            SseLine::Delta(content.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::{ChatMessage, OpenAiChatModel, SseLine, parse_sse_line};

    #[test]
    fn parses_sse_deltas_done_and_noise() {
        let cases: Vec<(&str, SseLine)> = vec![
            (
                r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#,
                SseLine::Delta("Hello".to_string()),
            ),
            (r"data: [DONE]", SseLine::Done),
            (": keep-alive", SseLine::Ignore),
            (
                r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
                SseLine::Ignore,
            ),
            (
                r#"data: {"choices":[{"delta":{"content":""}}]}"#,
                SseLine::Ignore,
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(parse_sse_line(line), expected, "line: {line}");
        }
    }

    #[test]
    fn request_body_is_streamed_with_system_prompt_and_history() {
        let model = OpenAiChatModel::new(
            "http://x/v1".to_string(),
            "m".to_string(),
            String::new(),
            None,
        );
        let body = model.request_body(&[
            ChatMessage::user("hi".to_string()),
            ChatMessage::assistant("hello".to_string()),
        ]);
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "m");
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][1],
            serde_json::json!({"role":"user","content":"hi"})
        );
        assert_eq!(
            body["messages"][2],
            serde_json::json!({"role":"assistant","content":"hello"})
        );
    }
}

//! Owns background conversation titles: the model call, cleaning the answer, and its metrics.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::json;
use tokio::time::Instant;
use tracing::Instrument;

use crate::model::{ChatEvent, ChatMessage, ChatModel};
use crate::recorder::Recorder;
use crate::store::{MAX_TITLE_CHARS, SessionStore};

/// System prompt for the title model.
pub const TITLE_SYSTEM_PROMPT: &str = "You write short titles for conversations. Reply with only a title of three to six words: no quotes, no trailing punctuation.";
const TITLE_TIMEOUT: Duration = Duration::from_secs(60);
const EXCERPT_CHARS: usize = 1000;

/// The background model that writes titles, with the name its metrics carry.
pub struct Titler<M> {
    /// The model to call.
    pub model: M,
    /// Model name recorded in metrics.
    pub name: String,
}

/// Writes the title for a first exchange off the reply path; failures keep the fallback title.
pub fn spawn_title<M: ChatModel + 'static>(
    titler: Arc<Titler<M>>,
    store: Arc<SessionStore>,
    id: String,
    user: &str,
    reply: &str,
    recorder: Recorder,
) {
    let prompt = format!("User: {}\nAssistant: {}", excerpt(user), excerpt(reply));
    tokio::spawn(
        async move {
            let started = Instant::now();
            let result = tokio::time::timeout(TITLE_TIMEOUT, ask(&titler.model, prompt)).await;
            let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (outcome, error) = match result {
                Ok(Ok(Some(title))) => {
                    store.set_model_title(&id, &title);
                    ("ok", None)
                }
                Ok(Ok(None)) => ("empty", None),
                Ok(Err(error)) => ("failed", Some(error)),
                Err(_) => ("timeout", None),
            };
            tracing::info!(model = %titler.name, latency_ms, outcome, "title generated");
            recorder.emit(
                "title",
                json!({
                    "model": titler.name,
                    "latency_ms": latency_ms,
                    "outcome": outcome,
                    "error": error,
                }),
            );
        }
        .in_current_span(),
    );
}

async fn ask<M: ChatModel>(model: &M, prompt: String) -> Result<Option<String>, String> {
    let mut deltas = model
        .stream_reply(&[ChatMessage::user(prompt)], None)
        .await
        .map_err(|error| error.to_string())?;
    let mut answer = String::new();
    while let Some(delta) = deltas.next().await {
        if let ChatEvent::Text(text) = delta.map_err(|error| error.to_string())? {
            answer.push_str(&text);
        }
    }
    Ok(clean_title(&answer))
}

fn excerpt(text: &str) -> String {
    text.chars().take(EXCERPT_CHARS).collect()
}

/// Reduces a model answer to a plain title: first non-empty line, no quotes or end punctuation.
#[must_use]
pub fn clean_title(answer: &str) -> Option<String> {
    let line = answer
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line = line.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’' | '*' | '#')
    });
    let line = line.trim_end_matches(['.', ',', ':', ';', '!', '?', '…']);
    let title: String = line.chars().take(MAX_TITLE_CHARS).collect();
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::clean_title;

    #[test]
    fn answers_are_reduced_to_a_plain_capped_title() {
        let long = "w".repeat(200);
        let cases = [
            (
                "\"Twelve times seven\"",
                Some("Twelve times seven".to_string()),
            ),
            (
                "\n  **Recipe help for dinner.**  \nExtra line",
                Some("Recipe help for dinner".to_string()),
            ),
            (
                "“Road trip packing list!”",
                Some("Road trip packing list".to_string()),
            ),
            (long.as_str(), Some("w".repeat(80))),
            ("  \n ", None),
            ("\"\"", None),
        ];
        for (answer, expected) in cases {
            assert_eq!(clean_title(answer), expected, "answer: {answer:?}");
        }
    }
}

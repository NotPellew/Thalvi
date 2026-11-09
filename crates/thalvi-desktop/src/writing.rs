//! Owns the writing model: the settings of its OpenAI-compatible chat endpoint and the streamed call that turns dictated words into proper text.

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::report::UserError;
use crate::secondary::base_url;

/// How long writing may take in all before nothing is pasted.
pub const LIMIT: Duration = Duration::from_secs(15);

/// The system message of a writing call until the user sets their own.
pub const DEFAULT_PROMPT: &str = "You write text for the user from words they dictated. Their message is what they said. If it asks for a kind of text, such as an email or a message, or for a tone or wording, write that. Otherwise write what they meant as clear, correct text, keeping their own wording and meaning. Write in the language they spoke. Use only what they said: keep every fact, name and number, and add no sentences, offers, promises or commitments and no names or details they did not give. Never write placeholders such as [Name]: leave out a greeting or sign-off name you do not have. Reply with the finished text only, as one paragraph without line breaks, because it is typed where the cursor is and a line break would press Enter: no quotes, no notes, no comment on the request.";

/// Why the writing settings were refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WritingSettingsError {
    #[error(
        "writing.url must be an http or https URL with a host, no credentials, query or fragment."
    )]
    Url,
    #[error("writing.model must be nonempty text.")]
    Model,
    #[error("writing.prompt must not be blank. Restore the default to start over.")]
    Prompt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWriting {
    url: String,
    model: String,
    #[serde(default)]
    api_key: String,
    prompt: Option<String>,
}

/// The chat endpoint that writes dictation out: its base address, model, API key and the prompt
/// it is given. The default prompt is not stored, so a better default reaches everyone who never
/// changed it.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawWriting")]
pub struct Writing {
    url: String,
    model: String,
    api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

impl fmt::Debug for Writing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writing")
            .field("url", &self.url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Writing {
    /// # Errors
    /// Fails when `url` is not an `http` or `https` base URL without credentials, query or
    /// fragment, `model` is blank, or `prompt` is given and blank. `None` and the default text
    /// both mean the default prompt.
    pub fn new(
        url: &str,
        model: &str,
        api_key: String,
        prompt: Option<&str>,
    ) -> Result<Self, WritingSettingsError> {
        let url = base_url(url).ok_or(WritingSettingsError::Url)?;
        let model = model.trim();
        if model.is_empty() {
            return Err(WritingSettingsError::Model);
        }
        let prompt = match prompt.map(str::trim) {
            Some("") => return Err(WritingSettingsError::Prompt),
            Some(prompt) if prompt != DEFAULT_PROMPT => Some(prompt.to_owned()),
            _ => None,
        };
        Ok(Self {
            url,
            model: model.to_owned(),
            api_key,
            prompt,
        })
    }

    /// The system message of each request: the user's own prompt, else [`DEFAULT_PROMPT`].
    #[must_use]
    pub fn prompt(&self) -> &str {
        self.prompt.as_deref().unwrap_or(DEFAULT_PROMPT)
    }

    /// The model named in each request.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The key kept in the config when a settings form leaves it unchanged.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.api_key
    }

    /// Asks the endpoint to write `words` out, with nothing else about the user: the reply is
    /// read piece by piece through [`Reply::next`].
    ///
    /// # Errors
    /// [`WritingError::Unreachable`] or [`WritingError::Status`] when no answer starts.
    pub async fn start(
        &self,
        client: &reqwest::Client,
        words: &str,
    ) -> Result<Reply, WritingError> {
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": self.prompt()},
                {"role": "user", "content": words},
            ],
            "stream": true,
        });
        let mut request = client
            .post(format!("{}/chat/completions", self.url))
            .header("Accept", "text/event-stream")
            .json(&body);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request
            .send()
            .await
            .map_err(|_| WritingError::Unreachable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(WritingError::Status(status.as_u16()));
        }
        Ok(Reply {
            chunks: Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec())),
            ),
            pending: Vec::new(),
            over: false,
        })
    }
}

impl TryFrom<RawWriting> for Writing {
    type Error = WritingSettingsError;

    fn try_from(raw: RawWriting) -> Result<Self, Self::Error> {
        Self::new(&raw.url, &raw.model, raw.api_key, raw.prompt.as_deref())
    }
}

/// Why writing produced no text. Messages are shown to the user as written, and the engine adds
/// that nothing was typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WritingError {
    #[error("No writing model is set up. Add one under Settings → Speech → Writing.")]
    NotSetUp,
    #[error("The writing model took longer than 15 seconds.")]
    Timeout,
    #[error("The writing model could not be reached.")]
    Unreachable,
    #[error("The writing model failed with status {0}.")]
    Status(u16),
    #[error("The writing model sent an answer Thalvi could not read.")]
    BadResponse,
    #[error("The writing model returned no text.")]
    Empty,
}

impl WritingError {
    /// A short name for the metrics, without any text.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::NotSetUp => "not_set_up",
            Self::Timeout => "timeout",
            Self::Unreachable => "unreachable",
            Self::Status(_) => "status",
            Self::BadResponse => "bad_response",
            Self::Empty => "empty",
        }
    }
}

impl UserError for WritingError {
    fn headline(&self) -> &'static str {
        match self {
            Self::NotSetUp => "Writing not set up — Settings",
            Self::Timeout => "Writing took too long",
            Self::Unreachable => "Writing model unreachable",
            Self::Status(_) => "Writing model failed",
            Self::BadResponse => "Writing reply unreadable",
            Self::Empty => "Writing returned no text",
        }
    }
}

/// One streamed answer, read a piece of text at a time.
pub struct Reply {
    chunks: Pin<Box<dyn Stream<Item = reqwest::Result<Vec<u8>>> + Send>>,
    /// Bytes received that do not yet end a line.
    pending: Vec<u8>,
    over: bool,
}

/// What one line of the stream says.
enum Line {
    Text(String),
    Done,
    Nothing,
}

impl Reply {
    /// The next piece of text, or `None` once the answer is complete. Safe to drop and call again.
    ///
    /// # Errors
    /// [`WritingError::Unreachable`] when the connection breaks, or
    /// [`WritingError::BadResponse`] when a piece is not the chat completions format.
    pub async fn next(&mut self) -> Result<Option<String>, WritingError> {
        loop {
            if let Some(line) = self.take_line() {
                match read_line(&line)? {
                    Line::Text(text) => return Ok(Some(text)),
                    Line::Done => {
                        self.over = true;
                        return Ok(None);
                    }
                    Line::Nothing => continue,
                }
            }
            if self.over {
                return Ok(None);
            }
            match self.chunks.next().await {
                Some(Ok(bytes)) => self.pending.extend_from_slice(&bytes),
                Some(Err(_)) => return Err(WritingError::Unreachable),
                None => {
                    self.over = true;
                    // An answer that ends without a final newline still ends in a line.
                    if !self.pending.is_empty() {
                        self.pending.push(b'\n');
                    }
                }
            }
        }
    }

    fn take_line(&mut self) -> Option<Vec<u8>> {
        let end = self.pending.iter().position(|byte| *byte == b'\n')?;
        let mut line: Vec<u8> = self.pending.drain(..=end).collect();
        line.pop();
        Some(line)
    }
}

fn read_line(line: &[u8]) -> Result<Line, WritingError> {
    let line = std::str::from_utf8(line).map_err(|_| WritingError::BadResponse)?;
    let Some(data) = line.trim().strip_prefix("data:") else {
        return Ok(Line::Nothing);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(Line::Done);
    }
    let piece: Value = serde_json::from_str(data).map_err(|_| WritingError::BadResponse)?;
    if piece.get("error").is_some() {
        return Err(WritingError::BadResponse);
    }
    Ok(piece
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map_or(Line::Nothing, |text| Line::Text(text.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_are_checked_where_they_are_loaded_and_the_key_stays_out_of_debug_output() {
        let cases = [
            (
                "http://127.0.0.1:8080/v1/",
                "m",
                Ok("http://127.0.0.1:8080/v1"),
            ),
            ("  https://host/v1 ", " m ", Ok("https://host/v1")),
            ("ftp://host", "m", Err(WritingSettingsError::Url)),
            ("host/v1", "m", Err(WritingSettingsError::Url)),
            (
                "http://user:pw@host/v1",
                "m",
                Err(WritingSettingsError::Url),
            ),
            ("http://host/v1?key=1", "m", Err(WritingSettingsError::Url)),
            ("http://host/v1#top", "m", Err(WritingSettingsError::Url)),
            ("http://host/v1", "  ", Err(WritingSettingsError::Model)),
        ];
        for (url, model, expected) in cases {
            let result =
                Writing::new(url, model, "secret".to_owned(), None).map(|writing| writing.url);
            assert_eq!(result, expected.map(str::to_owned), "{url:?} {model:?}");
        }
        let writing = Writing::new("http://host/v1", "m", "secret".to_owned(), None).unwrap();
        assert!(!format!("{writing:?}").contains("secret"));
    }

    #[test]
    fn the_prompt_in_effect_is_the_users_unless_it_is_blank_or_the_default() {
        let prompt = |given: Option<&str>| {
            Writing::new("http://host/v1", "m", String::new(), given)
                .map(|writing| (writing.prompt().to_owned(), writing.prompt.is_some()))
        };
        let default = (DEFAULT_PROMPT.to_owned(), false);
        assert_eq!(prompt(None), Ok(default.clone()));
        assert_eq!(prompt(Some(DEFAULT_PROMPT)), Ok(default.clone()));
        assert_eq!(prompt(Some(&format!("{DEFAULT_PROMPT}\n"))), Ok(default));
        assert_eq!(
            prompt(Some(" Be brief. ")),
            Ok(("Be brief.".to_owned(), true))
        );
        assert_eq!(prompt(Some("")), Err(WritingSettingsError::Prompt));
        assert_eq!(prompt(Some(" \n")), Err(WritingSettingsError::Prompt));
    }

    fn reply_of(parts: &[&[u8]]) -> Reply {
        let parts: Vec<reqwest::Result<Vec<u8>>> =
            parts.iter().map(|part| Ok(part.to_vec())).collect();
        Reply {
            chunks: Box::pin(futures_util::stream::iter(parts)),
            pending: Vec::new(),
            over: false,
        }
    }

    async fn pieces(mut reply: Reply) -> (Vec<String>, Result<(), WritingError>) {
        let mut out = Vec::new();
        loop {
            match reply.next().await {
                Ok(Some(piece)) => out.push(piece),
                Ok(None) => return (out, Ok(())),
                Err(error) => return (out, Err(error)),
            }
        }
    }

    #[tokio::test]
    async fn a_streamed_answer_is_read_across_chunk_boundaries_and_stops_at_done() {
        let hello = "data: {\"choices\":[{\"delta\":{\"content\":\"Héllo\"}}]}\n\n";
        let split = hello.find('é').unwrap() + 1;
        let (before, after) = hello.as_bytes().split_at(split);
        let reply = reply_of(&[
            b": keep-alive\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\r\n\r\n",
            before,
            after,
            b"data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\ndata: [DONE]\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"after done\"}}]}\n\n",
        ]);
        assert_eq!(
            pieces(reply).await,
            (vec!["Héllo".to_owned(), " there".to_owned()], Ok(()))
        );
    }

    #[tokio::test]
    async fn an_answer_that_ends_without_done_or_a_final_newline_still_ends() {
        let reply = reply_of(&[b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}"]);
        assert_eq!(pieces(reply).await, (vec!["Hi".to_owned()], Ok(())));
    }

    #[tokio::test]
    async fn an_answer_that_is_not_chat_completions_is_unreadable() {
        for body in [
            &b"data: not json\n"[..],
            b"data: {\"error\":{\"message\":\"overloaded\"}}\n",
            b"data: \xff\n",
        ] {
            let (_, result) = pieces(reply_of(&[body])).await;
            assert_eq!(result, Err(WritingError::BadResponse), "{body:?}");
        }
    }
}

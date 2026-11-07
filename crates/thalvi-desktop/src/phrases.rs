//! Owns the word list: the spellings recognition favours, and its limits.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::metrics::{Event, Metrics};
use crate::recognizer::ServerUrl;

/// How long a word list request to the server may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The most entries a word list may hold; the server and the self-hosted recognizer agree.
pub const MAX_PHRASES: usize = 100;
/// The longest entry, in characters.
pub const MAX_PHRASE_CHARS: usize = 40;

/// Why a word list was refused. Messages are shown to the user as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WordListError {
    #[error("The word list can hold at most {MAX_PHRASES} entries.")]
    TooMany,
    #[error("Each word list entry must be 1 to {MAX_PHRASE_CHARS} characters.")]
    Length,
}

/// Names and terms whose spelling recognition should favour, each trimmed and within the limits.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct WordList(Vec<String>);

impl WordList {
    /// # Errors
    /// Fails for too many entries or an entry that is blank or too long.
    pub fn new(phrases: Vec<String>) -> Result<Self, WordListError> {
        if phrases.len() > MAX_PHRASES {
            return Err(WordListError::TooMany);
        }
        phrases
            .into_iter()
            .map(|phrase| {
                let phrase = phrase.trim();
                if phrase.is_empty() || phrase.chars().count() > MAX_PHRASE_CHARS {
                    Err(WordListError::Length)
                } else {
                    Ok(phrase.to_owned())
                }
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    #[must_use]
    pub fn phrases(&self) -> &[String] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Vec<String>> for WordList {
    type Error = WordListError;

    fn try_from(phrases: Vec<String>) -> Result<Self, WordListError> {
        Self::new(phrases)
    }
}

impl From<WordList> for Vec<String> {
    fn from(list: WordList) -> Self {
        list.0
    }
}

#[derive(Deserialize)]
struct ServerList {
    boosting: Option<bool>,
}

#[derive(Deserialize)]
struct ServerError {
    error: String,
}

fn unsupported(server: &ServerUrl) -> String {
    format!("The server at {server} doesn't support a word list yet; it may need updating.")
}

/// Whether the server's recognizer boosts a word list; `Ok(false)` covers Meta's recognizer.
///
/// # Errors
/// A plain message when the server can't be reached or can't tell.
pub async fn server_boosts(
    client: &reqwest::Client,
    server: &ServerUrl,
    metrics: &Metrics,
) -> Result<bool, String> {
    let started = Instant::now();
    let result = async {
        let response = client
            .get(server.http("/phrases"))
            .timeout(TIMEOUT)
            .send()
            .await?;
        response.error_for_status()?.json::<ServerList>().await
    }
    .await;
    let result = match result {
        Ok(ServerList {
            boosting: Some(boosting),
        }) => Ok(boosting),
        Ok(ServerList { boosting: None }) => Err(format!(
            "The server at {server} could not reach its speech recognizer to ask about a word list."
        )),
        Err(error) => Err(match error.status() {
            Some(reqwest::StatusCode::NOT_FOUND) => unsupported(server),
            Some(status) => format!(
                "The server at {server} answered HTTP {} when Thalvi asked about a word list.",
                status.as_u16()
            ),
            None if error.is_decode() => {
                format!("The server at {server} sent a word list response Thalvi could not read.")
            }
            None => {
                format!("Thalvi could not reach the server at {server} to ask about a word list.")
            }
        }),
    };
    metrics.metric(
        Event::WordList,
        if result.is_ok() { "ok" } else { "error" },
        Some(started.elapsed()),
        json!({"call": "check"}),
    );
    result
}

/// Saves `list` on the server, which gives it to its recognizer for dictation and conversations.
///
/// # Errors
/// A plain message when the server can't be reached or refuses the list.
pub async fn share(
    client: &reqwest::Client,
    server: &ServerUrl,
    list: &WordList,
    metrics: &Metrics,
) -> Result<(), String> {
    let started = Instant::now();
    let result = client
        .put(server.http("/phrases"))
        .json(&json!({"phrases": list.phrases()}))
        .timeout(TIMEOUT)
        .send()
        .await;
    let refused = match result {
        Ok(response) if response.status().is_success() => None,
        Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
            Some(unsupported(server))
        }
        Ok(response) => {
            let status = response.status();
            Some(response.json::<ServerError>().await.map_or_else(
                |_| {
                    format!(
                        "The server at {server} answered HTTP {} when Thalvi tried to save the word list.",
                        status.as_u16()
                    )
                },
                |body| body.error,
            ))
        }
        Err(_) => Some(format!(
            "The word list is saved here, but Thalvi could not reach the server at {server} to use it there."
        )),
    };
    metrics.metric(
        Event::WordList,
        if refused.is_none() { "ok" } else { "error" },
        Some(started.elapsed()),
        json!({"call": "share"}),
    );
    refused.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Case<'a> = (Vec<&'a str>, Result<Vec<&'a str>, WordListError>);

    #[test]
    fn a_list_is_trimmed_or_refused_in_plain_words() {
        let too_long = "x".repeat(MAX_PHRASE_CHARS + 1);
        let cases: [Case; 4] = [
            (vec![" Cybrex2 ", "Thalvi"], Ok(vec!["Cybrex2", "Thalvi"])),
            (vec!["x"; MAX_PHRASES + 1], Err(WordListError::TooMany)),
            (vec!["  "], Err(WordListError::Length)),
            (vec![too_long.as_str()], Err(WordListError::Length)),
        ];
        for (phrases, expected) in cases {
            let got = WordList::new(phrases.iter().map(ToString::to_string).collect());
            assert_eq!(
                got.map(Vec::from),
                expected.map(|ok| ok.into_iter().map(ToString::to_string).collect())
            );
        }
    }
}

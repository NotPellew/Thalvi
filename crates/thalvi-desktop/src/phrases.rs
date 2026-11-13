//! Owns the local word list and checks direct recognizers for phrase boosting.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::entries::Realtime;
use crate::metrics::{Event, Metrics};

/// How long a direct recognizer check may take.
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

/// Whether a recognizer reached directly announces phrase boosting in its handshake; Meta's does not.
///
/// # Errors
/// A plain message when the recognizer can't be reached or doesn't accept the session.
pub async fn realtime_boosts(realtime: &Realtime, metrics: &Metrics) -> Result<bool, String> {
    let started = Instant::now();
    let address = realtime.address();
    let opened = tokio::time::timeout(
        TIMEOUT,
        thalvi_asr::Connection::open(thalvi_asr::Handshake {
            url: address,
            model: realtime.model(),
            api_key: realtime.api_key(),
            phrases: &[],
        }),
    )
    .await;
    let result = match opened {
        Ok(Ok(mut connection)) => {
            let boosts = connection.boosts_phrases();
            connection.close().await;
            Ok(boosts)
        }
        Ok(Err(thalvi_asr::Error::Refused(message))) => Err(format!(
            "The recognizer at {address} refused the session when Thalvi asked about a word list: {message}"
        )),
        Ok(Err(thalvi_asr::Error::Connect(_))) | Err(_) => Err(format!(
            "Thalvi could not reach the recognizer at {address} to ask about a word list."
        )),
        Ok(Err(_)) => Err(format!(
            "The recognizer at {address} answered something Thalvi did not understand when asked about a word list."
        )),
    };
    metrics.metric(
        Event::WordList,
        if result.is_ok() { "ok" } else { "error" },
        Some(started.elapsed()),
        json!({"call": "check"}),
    );
    result
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

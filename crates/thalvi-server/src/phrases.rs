//! Owns the word list recognizers that boost phrases are given, kept in one file in the data directory.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use crate::learning::write_atomic;

/// The most entries a word list may hold; the self-hosted recognizer refuses more.
pub const MAX_PHRASES: usize = 100;
/// The longest entry, in characters.
pub const MAX_PHRASE_CHARS: usize = 40;

/// Why a word list was refused. Messages are shown to the user as written.
#[derive(Debug, PartialEq, Eq)]
pub enum PhraseError {
    TooMany,
    Length,
    /// The file could not be written; nothing changed.
    Disk(String),
}

impl fmt::Display for PhraseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooMany => write!(f, "The word list can hold at most {MAX_PHRASES} entries."),
            Self::Length => write!(
                f,
                "Each word list entry must be 1 to {MAX_PHRASE_CHARS} characters."
            ),
            Self::Disk(error) => write!(f, "The word list could not be saved: {error}"),
        }
    }
}

impl std::error::Error for PhraseError {}

#[derive(Default, Serialize, Deserialize)]
struct File {
    phrases: Vec<String>,
}

/// The word list, read once at start and replaced whole by the desktop.
pub struct PhraseStore {
    path: PathBuf,
    phrases: RwLock<Arc<[String]>>,
}

/// The entries trimmed, or why they can't be used.
///
/// # Errors
///
/// Fails for too many entries or an entry that is blank or too long.
pub fn valid(phrases: &[String]) -> Result<Vec<String>, PhraseError> {
    if phrases.len() > MAX_PHRASES {
        return Err(PhraseError::TooMany);
    }
    phrases
        .iter()
        .map(|phrase| {
            let phrase = phrase.trim();
            if phrase.is_empty() || phrase.chars().count() > MAX_PHRASE_CHARS {
                Err(PhraseError::Length)
            } else {
                Ok(phrase.to_owned())
            }
        })
        .collect()
}

impl PhraseStore {
    /// Opens the list at `path`; a missing file is an empty list.
    ///
    /// # Errors
    ///
    /// Returns a plain message when the file cannot be read, understood or used.
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<File>(&bytes)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let phrases =
            valid(&file.phrases).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            phrases: RwLock::new(phrases.into()),
        })
    }

    /// The current list.
    pub fn list(&self) -> Arc<[String]> {
        Arc::clone(&self.phrases.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replaces the list, on disk first.
    ///
    /// # Errors
    ///
    /// Fails without changing anything when an entry is refused or the file cannot be written.
    pub fn replace(&self, phrases: &[String]) -> Result<(), PhraseError> {
        let phrases = valid(phrases)?;
        let json = serde_json::to_vec_pretty(&File {
            phrases: phrases.clone(),
        })
        .map_err(|error| PhraseError::Disk(error.to_string()))?;
        write_atomic(&self.path, &json).map_err(|error| PhraseError::Disk(error.to_string()))?;
        *self.phrases.write().unwrap_or_else(PoisonError::into_inner) = phrases.into();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (PathBuf, PhraseStore) {
        let dir =
            std::env::temp_dir().join(format!("thalvi-phrases-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = PhraseStore::open(&dir.join("phrases.json")).unwrap_or_else(|e| panic!("{e}"));
        (dir, store)
    }

    #[test]
    fn a_saved_list_is_trimmed_and_survives_a_restart() {
        let (dir, store) = store("restart");
        assert!(store.list().is_empty());
        store
            .replace(&[" Cybrex2 ".to_owned(), "Thalvi".to_owned()])
            .unwrap_or_else(|e| panic!("{e}"));
        let reopened =
            PhraseStore::open(&dir.join("phrases.json")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(&*reopened.list(), ["Cybrex2", "Thalvi"]);
    }

    #[test]
    fn a_refused_list_leaves_the_saved_one_in_place() {
        let (_dir, store) = store("refused");
        store
            .replace(&["Cybrex2".to_owned()])
            .unwrap_or_else(|e| panic!("{e}"));
        let cases = [
            (vec!["x".to_owned(); MAX_PHRASES + 1], PhraseError::TooMany),
            (vec!["  ".to_owned()], PhraseError::Length),
            (vec!["x".repeat(MAX_PHRASE_CHARS + 1)], PhraseError::Length),
        ];
        for (phrases, error) in cases {
            assert_eq!(store.replace(&phrases), Err(error));
            assert_eq!(&*store.list(), ["Cybrex2"]);
        }
    }
}

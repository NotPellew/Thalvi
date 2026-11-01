//! Owns the durable list of things Thalvi has learned about the user, kept in one JSON file.

use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Longest stored learning, in characters.
pub const MAX_TEXT_CHARS: usize = 1000;

/// One thing learned about the user, as written to disk and sent to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Learning {
    /// Stable id that survives edits.
    pub id: String,
    /// What was learned, in words the user can read and correct.
    pub text: String,
    /// The retired session it was extracted from; `None` when the user wrote it.
    pub source_session: Option<String>,
    /// When it was last added or edited, in milliseconds since the Unix epoch.
    pub updated_ms: u64,
}

/// Why a learning could not be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearningError {
    /// The text is empty once trimmed.
    Blank,
    /// The text is longer than [`MAX_TEXT_CHARS`].
    TooLong,
    /// No learning has that id.
    NotFound,
    /// The file could not be written; nothing changed.
    Disk(String),
}

impl fmt::Display for LearningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => f.write_str("The text can't be empty."),
            Self::TooLong => write!(
                f,
                "The text is too long (at most {MAX_TEXT_CHARS} characters)."
            ),
            Self::NotFound => f.write_str("That learning doesn't exist."),
            Self::Disk(error) => write!(f, "Could not save the learning: {error}"),
        }
    }
}

impl std::error::Error for LearningError {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    next_id: u64,
    learnings: Vec<Learning>,
}

/// Learnings kept in one file until the user edits them.
pub struct LearningStore {
    path: PathBuf,
    state: Mutex<State>,
}

impl LearningStore {
    /// Opens the store at `path`; a missing file is an empty store.
    ///
    /// # Errors
    ///
    /// Returns a plain message when the file cannot be read or understood.
    pub fn open(path: &Path) -> Result<Self, String> {
        let state = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every learning, oldest first.
    pub fn list(&self) -> Vec<Learning> {
        self.lock().learnings.clone()
    }

    /// Stores what a retired session taught, all or nothing, and returns the new rows.
    ///
    /// # Errors
    ///
    /// Fails without storing anything when a text is blank or too long, or the file cannot be
    /// written.
    pub fn append(
        &self,
        source_session: &str,
        texts: &[String],
    ) -> Result<Vec<Learning>, LearningError> {
        let texts = texts
            .iter()
            .map(|text| valid(text))
            .collect::<Result<Vec<_>, _>>()?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut guard = self.lock();
        let mut next = guard.clone();
        let updated_ms = now_ms();
        let mut added = Vec::new();
        for text in texts {
            if next.learnings.iter().any(|learning| {
                learning.source_session.as_deref() == Some(source_session) && learning.text == text
            }) {
                continue;
            }
            next.next_id += 1;
            let learning = Learning {
                id: format!("l{}", next.next_id),
                text,
                source_session: Some(source_session.to_string()),
                updated_ms,
            };
            next.learnings.push(learning.clone());
            added.push(learning);
        }
        if added.is_empty() {
            return Ok(added);
        }
        self.commit(&mut guard, next)?;
        Ok(added)
    }

    /// Removes newly extracted rows when their retirement is rejected, all or nothing.
    ///
    /// # Errors
    ///
    /// Fails without changing the live list when the file cannot be written.
    pub fn remove(&self, ids: &[String]) -> Result<(), LearningError> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut guard = self.lock();
        let mut next = guard.clone();
        next.learnings
            .retain(|learning| !ids.contains(&learning.id));
        if next.learnings.len() == guard.learnings.len() {
            return Ok(());
        }
        self.commit(&mut guard, next)
    }

    /// Replaces the text of learning `id`.
    ///
    /// # Errors
    ///
    /// Fails without changing anything when the text is blank or too long, no learning has that
    /// id, or the file cannot be written.
    pub fn edit(&self, id: &str, text: &str) -> Result<(), LearningError> {
        let text = valid(text)?;
        let mut guard = self.lock();
        let mut next = guard.clone();
        let learning = next
            .learnings
            .iter_mut()
            .find(|learning| learning.id == id)
            .ok_or(LearningError::NotFound)?;
        learning.text = text;
        learning.updated_ms = now_ms();
        self.commit(&mut guard, next)
    }

    /// Writes `next` and only then makes it the live state.
    fn commit(&self, live: &mut State, next: State) -> Result<(), LearningError> {
        let json =
            serde_json::to_vec(&next).map_err(|error| LearningError::Disk(error.to_string()))?;
        write_atomic(&self.path, &json).map_err(|error| LearningError::Disk(error.to_string()))?;
        *live = next;
        Ok(())
    }
}

fn valid(text: &str) -> Result<String, LearningError> {
    let text = text.trim();
    if text.is_empty() {
        Err(LearningError::Blank)
    } else if text.chars().count() > MAX_TEXT_CHARS {
        Err(LearningError::TooLong)
    } else {
        Ok(text.to_string())
    }
}

fn write_atomic(path: &Path, json: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = File::create(&temp)?;
    file.write_all(json)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::{LearningError, LearningStore, MAX_TEXT_CHARS};

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("thalvi-learning-test-{tag}-{}", std::process::id()))
    }

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_missing_file_is_empty_and_appended_learnings_survive_a_reopen() {
        let dir = scratch("reopen");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("learnings.json");
        let store = LearningStore::open(&path).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(store.list(), Vec::new());

        let added = store
            .append("s1", &texts(&["  Likes tea  ", "Has two cats"]))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(added.len(), 2);
        assert_eq!(added[0].text, "Likes tea");
        assert_eq!(added[0].source_session.as_deref(), Some("s1"));
        assert_ne!(added[0].id, added[1].id);

        let reopened = LearningStore::open(&path).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(reopened.list(), added);
        let retry = reopened
            .append("s1", &texts(&["Likes tea", "Prefers quiet mornings"]))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            retry.len(),
            1,
            "a retirement retry must not duplicate a learning"
        );
        assert_eq!(retry[0].text, "Prefers quiet mornings");
        let retry_ids = retry.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        reopened
            .remove(&retry_ids)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            reopened.list(),
            added,
            "a rejected retirement rolls back only its new rows"
        );
        let later = reopened
            .append("s2", &texts(&["Walks at six"]))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(added.iter().all(|row| row.id != later[0].id));
        assert!(!dir.join("learnings.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editing_changes_one_text_keeps_its_id_and_survives_a_reopen() {
        let dir = scratch("edit");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("learnings.json");
        let store = LearningStore::open(&path).unwrap_or_else(|e| panic!("{e}"));
        let added = store
            .append("s1", &texts(&["Likes tea", "Has two cats"]))
            .unwrap_or_else(|e| panic!("{e}"));
        store
            .edit(&added[0].id, "  Likes coffee ")
            .unwrap_or_else(|e| panic!("{e}"));

        let rows = LearningStore::open(&path)
            .unwrap_or_else(|e| panic!("{e}"))
            .list();
        assert_eq!(rows[0].id, added[0].id);
        assert_eq!(rows[0].text, "Likes coffee");
        assert_eq!(rows[0].source_session.as_deref(), Some("s1"));
        assert_eq!(rows[1], added[1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_input_and_unknown_ids_change_nothing() {
        let dir = scratch("invalid");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("learnings.json");
        let store = LearningStore::open(&path).unwrap_or_else(|e| panic!("{e}"));
        let added = store
            .append("s1", &texts(&["Likes tea"]))
            .unwrap_or_else(|e| panic!("{e}"));
        let id = &added[0].id;

        let at_limit = "x".repeat(MAX_TEXT_CHARS);
        let past_limit = "x".repeat(MAX_TEXT_CHARS + 1);
        assert_eq!(store.edit(id, " \n "), Err(LearningError::Blank));
        assert_eq!(store.edit(id, &past_limit), Err(LearningError::TooLong));
        assert_eq!(store.edit("nope", "Fine"), Err(LearningError::NotFound));
        assert_eq!(
            store.append("s2", &texts(&["Fine", past_limit.as_str()])),
            Err(LearningError::TooLong)
        );
        assert_eq!(
            store.append("s2", &texts(&["Fine", ""])),
            Err(LearningError::Blank)
        );
        assert_eq!(store.list(), added);

        assert_eq!(store.edit(id, &at_limit), Ok(()));
        assert_eq!(store.list()[0].text, at_limit);
        assert_eq!(store.append("s2", &[]), Ok(Vec::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_is_reported_and_leaves_memory_as_it_was() {
        let dir = scratch("disk");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("learnings.json");
        let store = LearningStore::open(&path).unwrap_or_else(|e| panic!("{e}"));
        let added = store
            .append("s1", &texts(&["Likes tea"]))
            .unwrap_or_else(|e| panic!("{e}"));
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&dir, "in the way").unwrap_or_else(|e| panic!("{e}"));

        assert!(matches!(
            store.edit(&added[0].id, "Likes coffee"),
            Err(LearningError::Disk(_))
        ));
        assert!(matches!(
            store.append("s2", &texts(&["Has two cats"])),
            Err(LearningError::Disk(_))
        ));
        assert_eq!(store.list(), added);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn an_unreadable_file_is_refused_rather_than_treated_as_empty() {
        let dir = scratch("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let path = dir.join("learnings.json");
        std::fs::write(&path, "{").unwrap_or_else(|e| panic!("{e}"));
        assert!(LearningStore::open(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

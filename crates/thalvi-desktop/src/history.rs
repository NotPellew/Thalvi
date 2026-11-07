//! The local history of dictations: what was heard, when, where it went and how it ended.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::capture::CaptureRoute;
use crate::delivery::Finished;
use crate::engine::CaptureOutcome;
use crate::metrics::{create_private_dir, now_ms};

/// The most entries a history may be asked to keep.
pub const MAX_LIMIT: usize = 1000;

const DEFAULT_LIMIT: usize = 200;

/// Whether history is kept and how many entries it holds. On by default: it stays on this computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawHistory")]
pub struct HistoryConfig {
    enabled: bool,
    limit: usize,
}

impl HistoryConfig {
    #[must_use]
    pub fn enabled(self) -> bool {
        self.enabled
    }

    /// The most entries kept; the oldest are dropped past it.
    #[must_use]
    pub fn limit(self) -> usize {
        self.limit
    }
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            limit: DEFAULT_LIMIT,
        }
    }
}

/// The history limit was outside what can be kept. Shown as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("The history limit must be a whole number of entries from 1 to {MAX_LIMIT}.")]
pub struct LimitError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHistory {
    #[serde(default = "enabled")]
    enabled: bool,
    limit: Option<usize>,
}

fn enabled() -> bool {
    true
}

impl TryFrom<RawHistory> for HistoryConfig {
    type Error = LimitError;

    fn try_from(raw: RawHistory) -> Result<Self, LimitError> {
        let limit = raw.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(LimitError);
        }
        Ok(Self {
            enabled: raw.enabled,
            limit,
        })
    }
}

/// How a recorded capture ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The text reached its destination.
    Accepted,
    Cancelled,
    /// The capture failed, or accepted text could not be delivered.
    Failed,
}

/// One recorded capture, as it is stored and shown.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Unique and increasing; names the entry to delete.
    pub id: u64,
    pub timestamp_ms: u64,
    /// What was transcribed. Never audio.
    pub text: String,
    /// The window the capture was bound to, when there was one.
    pub target: Option<String>,
    pub outcome: Outcome,
}

/// A finished capture to record; the store adds the id and the time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub text: String,
    pub target: Option<String>,
    pub outcome: Outcome,
}

impl Capture {
    /// The capture that `outcome` and its delivery came to, bound to `window`. `None` when no
    /// text was heard, so there is nothing to keep.
    #[must_use]
    pub fn of(
        outcome: &CaptureOutcome,
        finished: Option<&Finished>,
        window: Option<String>,
    ) -> Option<Self> {
        let (text, outcome, target) = match outcome {
            CaptureOutcome::Accepted(request) => {
                let outcome = if finished.is_some_and(|finished| finished.delivered) {
                    Outcome::Accepted
                } else {
                    Outcome::Failed
                };
                let target = if request.route == CaptureRoute::Task {
                    finished.and_then(|finished| finished.task_target.clone())
                } else {
                    window
                };
                (request.text.clone(), outcome, target)
            }
            CaptureOutcome::Cancelled { dictated } => {
                (dictated.clone()?, Outcome::Cancelled, window)
            }
            CaptureOutcome::Failed {
                recoverable_text, ..
            } => (recoverable_text.clone()?, Outcome::Failed, window),
        };
        (!text.trim().is_empty()).then_some(Self {
            text,
            target,
            outcome,
        })
    }
}

/// Why the history could not be used. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("This system has no per-user data directory to keep the history in.")]
    NoDataDir,
    #[error("The history at {path} could not be read or written: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("The history file {path} is damaged ({source}). Clear the history to start a new one.")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// The history file. One lock orders every change, so a write from the engine and a delete from
/// the settings window never overwrite each other.
pub struct Store {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl Store {
    /// The per-user history file (`~/.local/share/thalvi/history.json`,
    /// `%LOCALAPPDATA%\thalvi\history.json`). Nothing exists on disk until the first entry.
    #[must_use]
    pub fn per_user() -> Self {
        Self {
            path: dirs::data_local_dir().map(|dir| dir.join("thalvi").join("history.json")),
            lock: Mutex::default(),
        }
    }

    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            lock: Mutex::default(),
        }
    }

    /// The entries, newest first.
    ///
    /// # Errors
    /// Fails when the file cannot be read or is damaged.
    pub fn list(&self) -> Result<Vec<Entry>, HistoryError> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut entries = self.read()?;
        entries.reverse();
        Ok(entries)
    }

    /// Adds `capture` and drops the oldest entries past the limit; does nothing when history is
    /// off or the capture has no text.
    ///
    /// # Errors
    /// Fails when the file cannot be written or is damaged; the file is left as it was.
    pub fn record(&self, config: HistoryConfig, capture: Capture) -> Result<(), HistoryError> {
        if !config.enabled() {
            return Ok(());
        }
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut entries = self.read()?;
        let now = now_ms();
        let id = entries
            .iter()
            .map(|entry| entry.id)
            .max()
            .map_or(now, |last| now.max(last + 1));
        entries.push(Entry {
            id,
            timestamp_ms: now,
            text: capture.text,
            target: capture.target,
            outcome: capture.outcome,
        });
        drop_oldest(&mut entries, config.limit());
        self.write(&entries)
    }

    /// Drops the oldest entries past the limit, as [`record`](Self::record) does; does nothing
    /// when history is off, so turning it off never deletes.
    ///
    /// # Errors
    /// Fails when the file cannot be written or is damaged.
    pub fn trim(&self, config: HistoryConfig) -> Result<(), HistoryError> {
        if !config.enabled() {
            return Ok(());
        }
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut entries = self.read()?;
        if entries.len() <= config.limit() {
            return Ok(());
        }
        drop_oldest(&mut entries, config.limit());
        self.write(&entries)
    }

    /// Like [`record`](Self::record) on a blocking thread, so the caller does not wait for the
    /// disk. A failure is logged; the capture and its delivery are never affected.
    pub fn record_in_background(self: &Arc<Self>, config: HistoryConfig, capture: Capture) {
        self.in_background(move |store| store.record(config, capture));
    }

    /// Like [`trim`](Self::trim) on a blocking thread, with failures logged.
    pub fn trim_in_background(self: &Arc<Self>, config: HistoryConfig) {
        self.in_background(move |store| store.trim(config));
    }

    fn in_background(
        self: &Arc<Self>,
        work: impl FnOnce(&Self) -> Result<(), HistoryError> + Send + 'static,
    ) {
        let store = Arc::clone(self);
        drop(tokio::task::spawn_blocking(move || {
            if let Err(error) = work(&store) {
                tracing::warn!(%error, "the history could not be written");
            }
        }));
    }

    /// Removes one entry; an entry that is already gone is not an error.
    ///
    /// # Errors
    /// Fails when the file cannot be written or is damaged.
    pub fn delete(&self, id: u64) -> Result<(), HistoryError> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut entries = self.read()?;
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == before {
            return Ok(());
        }
        self.write(&entries)
    }

    /// Removes every entry, and a damaged file with them.
    ///
    /// # Errors
    /// Fails when the file cannot be removed.
    pub fn clear(&self) -> Result<(), HistoryError> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let path = self.path()?;
        match std::fs::remove_file(path) {
            Err(source) if source.kind() != io::ErrorKind::NotFound => Err(HistoryError::Io {
                path: path.to_owned(),
                source,
            }),
            _ => Ok(()),
        }
    }

    fn path(&self) -> Result<&Path, HistoryError> {
        self.path.as_deref().ok_or(HistoryError::NoDataDir)
    }

    /// The entries oldest first; none when there is no file yet.
    fn read(&self) -> Result<Vec<Entry>, HistoryError> {
        let path = self.path()?;
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| HistoryError::Corrupt {
                path: path.to_owned(),
                source,
            }),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(HistoryError::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Replaces the file in one step, readable by its owner only: it holds what the user said.
    fn write(&self, entries: &[Entry]) -> Result<(), HistoryError> {
        let path = self.path()?;
        let io = |source: io::Error| HistoryError::Io {
            path: path.to_owned(),
            source,
        };
        let json = serde_json::to_vec(entries).map_err(|error| io(error.into()))?;
        if let Some(parent) = path.parent() {
            create_private_dir(parent).map_err(io)?;
        }
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let written =
            write_private(&temporary, &json).and_then(|()| std::fs::rename(&temporary, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        written.map_err(io)
    }
}

fn drop_oldest(entries: &mut Vec<Entry>, limit: usize) {
    let surplus = entries.len().saturating_sub(limit);
    entries.drain(..surplus);
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // Windows files inherit the private ACL of the per-user data directory.
    options.open(path)?.write_all(contents)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::capture::CaptureRequest;
    use crate::engine::CaptureError;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("thalvi-history-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        /// The file the store creates on the first entry; its folder does not exist yet.
        fn file(&self) -> PathBuf {
            self.0.join("thalvi").join("history.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn config(json: &str) -> HistoryConfig {
        serde_json::from_str(json).unwrap()
    }

    fn capture(text: &str) -> Capture {
        Capture {
            text: text.to_owned(),
            target: Some("Slack".to_owned()),
            outcome: Outcome::Accepted,
        }
    }

    fn texts(store: &Store) -> Vec<String> {
        store
            .list()
            .unwrap()
            .into_iter()
            .map(|entry| entry.text)
            .collect()
    }

    fn request(route: CaptureRoute, text: &str) -> CaptureOutcome {
        CaptureOutcome::Accepted(CaptureRequest {
            route,
            text: text.to_owned(),
        })
    }

    fn finished(delivered: bool) -> Finished {
        Finished {
            delivered,
            maybe_partial: false,
            on_clipboard: false,
            notice: String::new(),
            headline: "",
            causes: Vec::new(),
            task_target: None,
        }
    }

    fn finished_task(delivered: bool, target: &str) -> Finished {
        Finished {
            task_target: Some(target.to_owned()),
            ..finished(delivered)
        }
    }

    fn failed(recoverable_text: Option<&str>) -> CaptureOutcome {
        CaptureOutcome::Failed {
            error: CaptureError::MicStopped,
            recoverable_text: recoverable_text.map(str::to_owned),
        }
    }

    #[test]
    fn each_kind_of_ending_becomes_one_capture_and_silence_becomes_none() {
        let slack = || Some("Slack".to_owned());
        let kept = |text: &str, target, outcome| {
            Some(Capture {
                text: text.to_owned(),
                target,
                outcome,
            })
        };
        for (name, outcome, delivery, expected) in [
            (
                "a delivered paste",
                request(CaptureRoute::Paste, "hello"),
                Some(finished(true)),
                kept("hello", slack(), Outcome::Accepted),
            ),
            (
                "a delivered submit",
                request(CaptureRoute::Submit, "hello"),
                Some(finished(true)),
                kept("hello", slack(), Outcome::Accepted),
            ),
            (
                "accepted text the window refused",
                request(CaptureRoute::Paste, "hello"),
                Some(finished(false)),
                kept("hello", slack(), Outcome::Failed),
            ),
            (
                "accepted text with no delivery result",
                request(CaptureRoute::Paste, "hello"),
                None,
                kept("hello", slack(), Outcome::Failed),
            ),
            (
                "a task names no window",
                request(CaptureRoute::Task, "open the docs"),
                Some(finished(true)),
                kept("open the docs", None, Outcome::Accepted),
            ),
            (
                "a delivered task names its destination, not the wake window",
                request(CaptureRoute::Task, "open the docs"),
                Some(finished_task(true, "https://example.com/docs")),
                kept(
                    "open the docs",
                    Some("https://example.com/docs".to_owned()),
                    Outcome::Accepted,
                ),
            ),
            (
                "a failed task names what it tried, not the wake window",
                request(CaptureRoute::Task, "switch to editor"),
                Some(finished_task(false, "the editor")),
                kept(
                    "switch to editor",
                    Some("the editor".to_owned()),
                    Outcome::Failed,
                ),
            ),
            (
                "a cancel after words",
                CaptureOutcome::Cancelled {
                    dictated: Some("never sent".to_owned()),
                },
                None,
                kept("never sent", slack(), Outcome::Cancelled),
            ),
            (
                "a failure that kept words",
                failed(Some("half a sentence")),
                Some(finished(false)),
                kept("half a sentence", slack(), Outcome::Failed),
            ),
            (
                "a cancel before any word",
                CaptureOutcome::Cancelled { dictated: None },
                None,
                None,
            ),
            (
                "a failure with no words",
                failed(None),
                Some(finished(false)),
                None,
            ),
            (
                "accepted blank text",
                request(CaptureRoute::Submit, "  "),
                Some(finished(true)),
                None,
            ),
        ] {
            assert_eq!(
                Capture::of(&outcome, delivery.as_ref(), slack()),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn entries_survive_a_restart_newest_first_with_only_text_time_target_and_outcome() {
        let dir = TempDir::new("restart");
        let store = Store::at(dir.file());
        assert_eq!(texts(&store), Vec::<String>::new(), "no file is no history");
        assert!(!dir.0.exists(), "reading creates nothing");

        let on = HistoryConfig::default();
        store.record(on, capture("first")).unwrap();
        store
            .record(
                on,
                Capture {
                    text: "second".to_owned(),
                    target: None,
                    outcome: Outcome::Cancelled,
                },
            )
            .unwrap();

        let reopened = Store::at(dir.file()).list().unwrap();
        assert_eq!(
            reopened
                .iter()
                .map(|entry| (&*entry.text, entry.target.as_deref(), entry.outcome))
                .collect::<Vec<_>>(),
            [
                ("second", None, Outcome::Cancelled),
                ("first", Some("Slack"), Outcome::Accepted)
            ]
        );
        assert!(reopened[0].id > reopened[1].id);
        assert!(
            reopened[0].timestamp_ms >= reopened[1].timestamp_ms && reopened[1].timestamp_ms > 0
        );
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.file()).unwrap()).unwrap();
        let mut keys: Vec<&str> = file[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["id", "outcome", "target", "text", "timestamp_ms"]);
        assert_eq!(file[0]["outcome"], "accepted");
    }

    #[test]
    fn the_oldest_entries_go_first_when_the_limit_is_passed_or_lowered() {
        let dir = TempDir::new("rotation");
        let store = Store::at(dir.file());
        let three = config(r#"{"limit": 3}"#);
        for text in ["a", "b", "c"] {
            store.record(three, capture(text)).unwrap();
        }
        assert_eq!(
            texts(&store),
            ["c", "b", "a"],
            "a full history is not trimmed"
        );
        store.record(three, capture("d")).unwrap();
        assert_eq!(texts(&store), ["d", "c", "b"]);

        store
            .record(config(r#"{"limit": 1}"#), capture("e"))
            .unwrap();
        assert_eq!(
            texts(&store),
            ["e"],
            "a lower limit applies on the next write"
        );
    }

    #[test]
    fn a_lowered_limit_trims_at_once_but_a_switched_off_history_keeps_everything() {
        let dir = TempDir::new("trim");
        let store = Store::at(dir.file());
        store.trim(config(r#"{"limit": 1}"#)).unwrap();
        assert!(!dir.0.exists(), "trimming nothing creates nothing");
        for text in ["a", "b", "c", "d"] {
            store
                .record(HistoryConfig::default(), capture(text))
                .unwrap();
        }
        store.trim(config(r#"{"limit": 10}"#)).unwrap();
        assert_eq!(texts(&store), ["d", "c", "b", "a"], "within the limit");
        store
            .trim(config(r#"{"enabled": false, "limit": 1}"#))
            .unwrap();
        assert_eq!(texts(&store), ["d", "c", "b", "a"], "off deletes nothing");
        store.trim(config(r#"{"limit": 2}"#)).unwrap();
        assert_eq!(texts(&store), ["d", "c"]);
    }

    #[test]
    fn history_that_is_off_writes_nothing_and_keeps_what_it_has() {
        let dir = TempDir::new("off");
        let store = Store::at(dir.file());
        let off = config(r#"{"enabled": false}"#);
        store.record(off, capture("never kept")).unwrap();
        assert!(!dir.0.exists(), "nothing is created");

        store
            .record(HistoryConfig::default(), capture("kept"))
            .unwrap();
        store.record(off, capture("never kept")).unwrap();
        assert_eq!(texts(&store), ["kept"], "turning it off deletes nothing");
    }

    #[test]
    fn deleting_one_entry_leaves_the_rest_and_a_gone_entry_is_no_error() {
        let dir = TempDir::new("delete");
        let store = Store::at(dir.file());
        for text in ["a", "b", "c"] {
            store
                .record(HistoryConfig::default(), capture(text))
                .unwrap();
        }
        let middle = store.list().unwrap()[1].id;
        store.delete(middle).unwrap();
        assert_eq!(texts(&store), ["c", "a"]);
        store.delete(middle).unwrap();
        assert_eq!(texts(&store), ["c", "a"]);

        let empty = TempDir::new("delete-empty");
        Store::at(empty.file()).delete(1).unwrap();
        assert!(!empty.0.exists(), "deleting from nothing creates nothing");
    }

    #[test]
    fn clearing_removes_every_entry_and_asks_nothing_of_an_empty_history() {
        let dir = TempDir::new("clear");
        let store = Store::at(dir.file());
        store.clear().unwrap();
        store
            .record(HistoryConfig::default(), capture("a"))
            .unwrap();
        store
            .record(HistoryConfig::default(), capture("b"))
            .unwrap();
        store.clear().unwrap();
        assert_eq!(texts(&store), Vec::<String>::new());
        store
            .record(HistoryConfig::default(), capture("c"))
            .unwrap();
        assert_eq!(texts(&store), ["c"]);
    }

    #[test]
    fn a_damaged_file_is_reported_untouched_and_clearing_starts_a_new_history() {
        let dir = TempDir::new("corrupt");
        let store = Store::at(dir.file());
        std::fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
        std::fs::write(dir.file(), b"[{\"id\": 1, \"te").unwrap();

        for error in [
            store.list().unwrap_err(),
            store
                .record(HistoryConfig::default(), capture("lost"))
                .unwrap_err(),
            store.delete(1).unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(
                message.contains("is damaged") && message.contains("Clear the history"),
                "{message}"
            );
        }
        assert_eq!(std::fs::read(dir.file()).unwrap(), b"[{\"id\": 1, \"te");

        store.clear().unwrap();
        store
            .record(HistoryConfig::default(), capture("fresh"))
            .unwrap();
        assert_eq!(texts(&store), ["fresh"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_history_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("private");
        let store = Store::at(dir.file());
        store
            .record(HistoryConfig::default(), capture("a"))
            .unwrap();
        store
            .record(HistoryConfig::default(), capture("b"))
            .unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            (mode(&dir.file()), mode(dir.file().parent().unwrap())),
            (0o600, 0o700)
        );
    }

    #[tokio::test]
    async fn recording_in_the_background_lands_in_the_file() {
        let dir = TempDir::new("background");
        let store = Arc::new(Store::at(dir.file()));
        store.record_in_background(HistoryConfig::default(), capture("later"));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !dir.file().exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(texts(&store), ["later"]);
    }
}

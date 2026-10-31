//! Owns saved conversations on disk and which connection has each one open.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};

use crate::history::{self, HistoryMatch, HistoryQuery, HistoryScope};
use crate::model::{ChatMessage, Role};

/// Longest stored title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;
const FALLBACK_TITLE_CHARS: usize = 40;
/// How long a takeover waits for the old connection to save its last words.
const HANDOFF_WAIT: Duration = Duration::from_secs(2);

/// One saved message, as written to disk and sent to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// Who wrote it.
    pub role: Role,
    /// What was said.
    pub text: String,
}

/// Where a conversation's title came from; later sources never lose to earlier ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// Cut from the first user line.
    FirstLine,
    /// Written by the background model.
    Model,
    /// Typed by the user.
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    id: String,
    title: String,
    title_source: TitleSource,
    created_ms: u64,
    updated_ms: u64,
    next_turn: u64,
    turns: Vec<Turn>,
    #[serde(default)]
    retired: bool,
}

/// One row of the conversation list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Session id to reopen it with.
    pub id: String,
    /// What the list shows.
    pub title: String,
    /// Seconds since the last turn was saved.
    pub age_s: u64,
}

/// One inactive old conversation awaiting a generated retirement summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementCandidate {
    /// Conversation id, rechecked when the summary is committed.
    pub id: String,
    /// Full transcript given to the retirement model.
    pub history: Vec<ChatMessage>,
    /// Version used to avoid replacing a conversation that changed meanwhile.
    pub updated_ms: u64,
}

/// Why a live connection must let go of its conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detach {
    /// Another connection opened the same conversation.
    Moved,
    /// The conversation was deleted.
    Deleted,
}

struct Live {
    dead: Arc<AtomicBool>,
    /// Earlier connections whose takeover has not resumed yet, so can still save.
    predecessors: Vec<Arc<AtomicBool>>,
    detach: oneshot::Sender<Detach>,
    released: oneshot::Receiver<()>,
    save_error: watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Stored>,
    live: HashMap<String, Live>,
}

enum DiskOp {
    Save {
        id: String,
    },
    Rename {
        id: String,
        title: String,
        done: oneshot::Sender<Result<bool, String>>,
    },
    Remove {
        id: String,
        done: oneshot::Sender<Result<bool, String>>,
    },
    Retire {
        id: String,
        expected_updated_ms: u64,
        summary: String,
        done: oneshot::Sender<Result<bool, String>>,
    },
    #[cfg(test)]
    Pause {
        ready: oneshot::Sender<()>,
        resume: mpsc::Receiver<()>,
    },
    Flush(oneshot::Sender<()>),
}

/// The connection's claim on one conversation, handed out by [`SessionStore::attach`].
pub struct Attachment {
    /// The conversation's id.
    pub id: String,
    /// Messages saved so far.
    pub history: Vec<ChatMessage>,
    /// The number of the last turn used.
    pub next_turn: u64,
    dead: Arc<AtomicBool>,
    detach: oneshot::Receiver<Detach>,
    released: Option<oneshot::Sender<()>>,
    save_error: watch::Receiver<bool>,
}

impl Attachment {
    /// Resolves when the conversation must be let go; pends forever if the store is gone.
    pub async fn detached(&mut self) -> Detach {
        match (&mut self.detach).await {
            Ok(reason) => reason,
            Err(_) => std::future::pending().await,
        }
    }

    /// Resolves when a save of this conversation has failed; pends forever if the store is gone.
    pub async fn save_failed(&mut self) {
        let signalled = self.save_error.wait_for(|failed| *failed).await.is_ok();
        if !signalled {
            std::future::pending::<()>().await;
        }
    }

    /// A second watcher for this connection's save failures, sharing the same signal.
    #[must_use]
    pub fn save_error_receiver(&self) -> watch::Receiver<bool> {
        self.save_error.clone()
    }
}

/// Conversations kept under one directory until the user deletes them.
pub struct SessionStore {
    state: Arc<Mutex<State>>,
    writer: mpsc::Sender<DiskOp>,
    counter: AtomicU64,
}

impl SessionStore {
    /// Opens the store at `dir`, creating it and loading every saved conversation.
    ///
    /// # Errors
    ///
    /// Returns a plain message when the directory cannot be created or read.
    pub fn open(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        let sessions = load(dir)?;
        let (writer, queue) = mpsc::channel();
        let dir = dir.to_path_buf();
        let state = Arc::new(Mutex::new(State {
            sessions,
            live: HashMap::new(),
        }));
        let writer_state = Arc::clone(&state);
        std::thread::spawn(move || write_loop(&dir, &queue, &writer_state));
        Ok(Self {
            state,
            writer,
            counter: AtomicU64::new(1),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn new_id(&self) -> String {
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("s{count:x}-{:x}", now_ms())
    }

    /// Opens `resume` if it is saved, else a new conversation; an older connection
    /// holding it is told it moved and given a moment to save before this returns.
    pub async fn attach(&self, resume: Option<&str>) -> Attachment {
        let dead = Arc::new(AtomicBool::new(false));
        let (detach_tx, detach) = oneshot::channel();
        let (released_tx, released_rx) = oneshot::channel();
        let (save_error_tx, save_error) = watch::channel(false);
        let (id, previous) = {
            let mut state = self.lock();
            let id = resume
                .filter(|id| state.sessions.contains_key(*id))
                .map_or_else(|| self.new_id(), str::to_string);
            let previous = state.live.remove(&id);
            let predecessors = previous.as_ref().map_or_else(Vec::new, |previous| {
                let mut all = previous.predecessors.clone();
                all.push(Arc::clone(&previous.dead));
                all
            });
            state.live.insert(
                id.clone(),
                Live {
                    dead: Arc::clone(&dead),
                    predecessors: predecessors.clone(),
                    detach: detach_tx,
                    released: released_rx,
                    save_error: save_error_tx,
                },
            );
            (id, previous.map(|previous| (previous, predecessors)))
        };
        if let Some((previous, predecessors)) = previous {
            if previous.detach.send(Detach::Moved).is_ok() {
                let _ = tokio::time::timeout(HANDOFF_WAIT, previous.released).await;
            }
            // Under the lock, so no save from any older connection lands after this.
            let mut state = self.lock();
            for older in &predecessors {
                older.store(true, Ordering::SeqCst);
            }
            if let Some(live) = state.live.get_mut(&id)
                && Arc::ptr_eq(&live.dead, &dead)
            {
                live.predecessors.clear();
            }
        }
        let (history, next_turn) = self.lock().sessions.get(&id).map_or_else(
            || (Vec::new(), 0),
            |stored| {
                let history = stored
                    .turns
                    .iter()
                    .map(|turn| ChatMessage {
                        role: turn.role,
                        content: turn.text.clone(),
                    })
                    .collect();
                (history, stored.next_turn)
            },
        );
        Attachment {
            id,
            history,
            next_turn,
            dead,
            detach,
            released: Some(released_tx),
            save_error,
        }
    }

    /// Ends the connection's claim, letting a waiting takeover continue.
    pub fn release(&self, mut attachment: Attachment) {
        let mut state = self.lock();
        if state
            .live
            .get(&attachment.id)
            .is_some_and(|live| Arc::ptr_eq(&live.dead, &attachment.dead))
        {
            state.live.remove(&attachment.id);
        }
        if let Some(released) = attachment.released.take() {
            let _ = released.send(());
        }
    }

    /// Saves the connection's history; empty histories and deleted sessions are not saved.
    pub fn save(&self, attachment: &Attachment, history: &[ChatMessage], next_turn: u64) {
        if history.is_empty() {
            return;
        }
        let mut state = self.lock();
        if attachment.dead.load(Ordering::SeqCst) {
            return;
        }
        let now = now_ms();
        let stored = state
            .sessions
            .entry(attachment.id.clone())
            .or_insert_with(|| Stored {
                id: attachment.id.clone(),
                title: String::new(),
                title_source: TitleSource::FirstLine,
                created_ms: now,
                updated_ms: now,
                next_turn: 0,
                turns: Vec::new(),
                retired: false,
            });
        stored.retired = false;
        stored.turns = history
            .iter()
            .map(|message| Turn {
                role: message.role,
                text: message.content.clone(),
            })
            .collect();
        stored.next_turn = next_turn;
        stored.updated_ms = now;
        if stored.title_source == TitleSource::FirstLine {
            stored.title = first_line_title(&stored.turns);
        }
        self.queue_save(stored);
    }

    /// Lists saved conversations, newest first.
    pub fn list(&self) -> Vec<Summary> {
        let now = now_ms();
        let state = self.lock();
        let mut stored: Vec<&Stored> = state.sessions.values().collect();
        stored.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then(a.id.cmp(&b.id)));
        stored
            .into_iter()
            .map(|stored| Summary {
                id: stored.id.clone(),
                title: stored.title.clone(),
                age_s: now.saturating_sub(stored.updated_ms) / 1000,
            })
            .collect()
    }

    /// Searches the live history and, for `All`, every saved conversation.
    #[must_use]
    pub fn search_history(
        &self,
        current_id: &str,
        current_history: &[ChatMessage],
        query: &HistoryQuery,
    ) -> Vec<HistoryMatch> {
        let state = self.lock();
        let mut matches = Vec::new();
        if let Some(stored) = state.sessions.get(current_id) {
            for message in current_history {
                if let Some(found) = history::find_match(
                    current_id,
                    &stored.title,
                    message.role,
                    &message.content,
                    stored.updated_ms,
                    &query.text,
                ) {
                    matches.push(found);
                }
            }
        } else {
            let now = now_ms();
            for message in current_history {
                if let Some(found) = history::find_match(
                    current_id,
                    "Current conversation",
                    message.role,
                    &message.content,
                    now,
                    &query.text,
                ) {
                    matches.push(found);
                }
            }
        }
        if query.scope == HistoryScope::All {
            for stored in state
                .sessions
                .values()
                .filter(|stored| stored.id != current_id)
            {
                for turn in &stored.turns {
                    if let Some(found) = history::find_match(
                        &stored.id,
                        &stored.title,
                        turn.role,
                        &turn.text,
                        stored.updated_ms,
                        &query.text,
                    ) {
                        matches.push(found);
                    }
                }
            }
        }
        matches.sort_by(|a, b| {
            b.updated_ms
                .cmp(&a.updated_ms)
                .then(a.session_id.cmp(&b.session_id))
        });
        matches.truncate(history::MAX_RESULTS);
        matches
    }

    /// Returns inactive, not-yet-retired sessions last updated before `cutoff_ms`.
    #[must_use]
    pub fn retirement_candidates(&self, cutoff_ms: u64) -> Vec<RetirementCandidate> {
        let state = self.lock();
        state
            .sessions
            .values()
            .filter(|stored| {
                !stored.retired
                    && stored.updated_ms < cutoff_ms
                    && !state.live.contains_key(&stored.id)
            })
            .map(|stored| RetirementCandidate {
                id: stored.id.clone(),
                history: stored
                    .turns
                    .iter()
                    .map(|turn| ChatMessage {
                        role: turn.role,
                        content: turn.text.clone(),
                    })
                    .collect(),
                updated_ms: stored.updated_ms,
            })
            .collect()
    }

    /// Whether a retirement candidate is still unchanged and closed.
    #[must_use]
    pub fn retirement_current(&self, id: &str, expected_updated_ms: u64) -> bool {
        let state = self.lock();
        state.sessions.get(id).is_some_and(|stored| {
            !stored.retired
                && stored.updated_ms == expected_updated_ms
                && !state.live.contains_key(id)
        })
    }

    /// Replaces an unchanged, inactive transcript with its summary after the write succeeds.
    ///
    /// # Errors
    ///
    /// Returns a plain message when the summary cannot be encoded or written.
    pub async fn retire(
        &self,
        id: &str,
        expected_updated_ms: u64,
        summary: &str,
    ) -> Result<bool, String> {
        let (done, result) = oneshot::channel();
        self.writer
            .send(DiskOp::Retire {
                id: id.to_string(),
                expected_updated_ms,
                summary: summary.to_string(),
                done,
            })
            .map_err(|_| "could not queue the session retirement".to_string())?;
        result
            .await
            .map_err(|_| "the session writer stopped".to_string())?
    }

    /// Sets a title the user typed and waits for it to reach disk; false when
    /// the conversation does not exist.
    ///
    /// # Errors
    ///
    /// Returns a plain message when the write fails.
    pub async fn rename(&self, id: &str, title: &str) -> Result<bool, String> {
        let (done, result) = oneshot::channel();
        self.writer
            .send(DiskOp::Rename {
                id: id.to_string(),
                title: title.to_string(),
                done,
            })
            .map_err(|_| "could not queue the session rename".to_string())?;
        result
            .await
            .map_err(|_| "the session writer stopped".to_string())?
    }

    /// Sets the background model's title unless the user already chose one.
    pub fn set_model_title(&self, id: &str, title: &str) {
        self.retitle(id, title, TitleSource::Model);
    }

    fn retitle(&self, id: &str, title: &str, source: TitleSource) -> bool {
        let mut state = self.lock();
        let Some(stored) = state.sessions.get_mut(id) else {
            return false;
        };
        if source == TitleSource::User || stored.title_source != TitleSource::User {
            stored.title = title.to_string();
            stored.title_source = source;
            self.queue_save(stored);
        }
        true
    }

    /// Whether `id` is saved with a title still cut from the first line.
    pub fn needs_title(&self, id: &str) -> bool {
        self.lock()
            .sessions
            .get(id)
            .is_some_and(|stored| stored.title_source == TitleSource::FirstLine)
    }

    /// Deletes a conversation, tells the connection holding it, and waits for
    /// the removal to reach disk; false if unknown.
    ///
    /// # Errors
    ///
    /// Returns a plain message when the removal fails.
    pub async fn delete(&self, id: &str) -> Result<bool, String> {
        let (done, result) = oneshot::channel();
        self.writer
            .send(DiskOp::Remove {
                id: id.to_string(),
                done,
            })
            .map_err(|_| "could not queue the session removal".to_string())?;
        result
            .await
            .map_err(|_| "the session writer stopped".to_string())?
    }

    /// Waits until everything saved so far has reached the disk.
    pub async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.writer.send(DiskOp::Flush(done)).is_ok() {
            let _ = flushed.await;
        }
    }

    fn queue_save(&self, stored: &Stored) {
        let _ = self.writer.send(DiskOp::Save {
            id: stored.id.clone(),
        });
    }
}

fn first_line_title(turns: &[Turn]) -> String {
    let line = turns
        .iter()
        .find(|turn| turn.role == Role::User)
        .and_then(|turn| turn.text.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or_default();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= FALLBACK_TITLE_CHARS {
        return line;
    }
    let cut: String = line.chars().take(FALLBACK_TITLE_CHARS).collect();
    format!("{}…", cut.trim_end())
}

fn load(dir: &Path) -> Result<HashMap<String, Stored>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("could not read {}: {error}", dir.display()))?;
    let mut sessions = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        match read_stored(&path) {
            Ok(stored) => {
                sessions.insert(stored.id.clone(), stored);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping unreadable session");
            }
        }
    }
    Ok(sessions)
}

fn read_stored(path: &Path) -> Result<Stored, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let stored: Stored = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if path.file_stem().and_then(|stem| stem.to_str()) != Some(stored.id.as_str()) {
        return Err("the file name does not match the session id".to_string());
    }
    if stored.turns.is_empty() {
        return Err("the session has no turns".to_string());
    }
    Ok(stored)
}

fn notify_save_failed(state: &Mutex<State>, id: &str) {
    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(live) = state.live.get(id) {
        let _ = live.save_error.send(true);
    }
}

fn write_loop(dir: &Path, queue: &mpsc::Receiver<DiskOp>, state: &Arc<Mutex<State>>) {
    while let Ok(message) = queue.recv() {
        match message {
            DiskOp::Save { id } => {
                let stored = state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .sessions
                    .get(&id)
                    .cloned();
                let Some(stored) = stored else { continue };
                let result = write_stored(dir, &stored);
                if let Err(error) = &result {
                    tracing::warn!(%error, %id, "could not save session");
                    notify_save_failed(state, &id);
                }
            }
            DiskOp::Rename { id, title, done } => {
                let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                let result = match state.sessions.get_mut(&id) {
                    Some(stored) => {
                        let mut next = stored.clone();
                        next.title = title;
                        next.title_source = TitleSource::User;
                        write_stored(dir, &next).map(|()| {
                            *stored = next;
                            true
                        })
                    }
                    None => Ok(false),
                };
                let _ = done.send(result);
            }
            DiskOp::Remove { id, done } => {
                let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                let result = if state.sessions.contains_key(&id) {
                    match std::fs::remove_file(session_path(dir, &id)) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(error) => Err(error.to_string()),
                    }
                    .map(|()| {
                        state.sessions.remove(&id);
                        if let Some(live) = state.live.remove(&id) {
                            live.dead.store(true, Ordering::SeqCst);
                            for older in &live.predecessors {
                                older.store(true, Ordering::SeqCst);
                            }
                            let _ = live.detach.send(Detach::Deleted);
                        }
                        true
                    })
                } else {
                    Ok(false)
                };
                let _ = done.send(result);
            }
            DiskOp::Retire {
                id,
                expected_updated_ms,
                summary,
                done,
            } => {
                let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                let inactive = !state.live.contains_key(&id);
                let result = match state.sessions.get_mut(&id) {
                    Some(stored)
                        if inactive
                            && !stored.retired
                            && stored.updated_ms == expected_updated_ms =>
                    {
                        let mut next = stored.clone();
                        next.turns = vec![Turn {
                            role: Role::Assistant,
                            text: format!("Summary of the earlier conversation:\n{summary}"),
                        }];
                        next.retired = true;
                        write_stored(dir, &next).map(|()| {
                            *stored = next;
                            true
                        })
                    }
                    _ => Ok(false),
                };
                let _ = done.send(result);
            }
            #[cfg(test)]
            DiskOp::Pause { ready, resume } => {
                let _ = ready.send(());
                let _ = resume.recv();
            }
            DiskOp::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn write_stored(dir: &Path, stored: &Stored) -> Result<(), String> {
    let json = serde_json::to_vec(stored).map_err(|error| error.to_string())?;
    write_atomic(dir, &stored.id, &json).map_err(|error| error.to_string())
}

fn session_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn write_atomic(dir: &Path, id: &str, json: &[u8]) -> std::io::Result<()> {
    let temp = dir.join(format!("{id}.json.tmp"));
    let mut file = File::create(&temp)?;
    file.write_all(json)?;
    file.sync_all()?;
    std::fs::rename(&temp, session_path(dir, id))
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
    use super::{Detach, SessionStore};
    use crate::history::parse_arguments;
    use crate::model::ChatMessage;

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("thalvi-store-test-{tag}-{}", std::process::id()))
    }

    async fn pause_writer(store: &SessionStore) -> std::sync::mpsc::Sender<()> {
        let (ready, paused) = tokio::sync::oneshot::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        store
            .writer
            .send(super::DiskOp::Pause {
                ready,
                resume: wait,
            })
            .expect("pause writer");
        tokio::time::timeout(std::time::Duration::from_secs(5), paused)
            .await
            .expect("writer paused")
            .expect("pause acknowledged");
        resume
    }

    #[tokio::test]
    async fn unreadable_empty_and_misnamed_files_are_skipped_and_long_multiline_first_lines_are_cut()
     {
        let dir = scratch("load");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let attachment = store.attach(None).await;
        let question = format!("\n  {}  \nsecond line", "word ".repeat(20));
        store.save(&attachment, &[ChatMessage::user(question)], 1);
        store.flush().await;
        std::fs::write(dir.join("broken.json"), "{").unwrap_or_else(|e| panic!("{e}"));
        let json = String::from_utf8_lossy(&attachment_file(&dir, &attachment.id)).to_string();
        std::fs::write(
            dir.join("misnamed.json"),
            json.replace(&attachment.id, "elsewhere"),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let without_turns = json.replace(&attachment.id, "empty");
        let (head, _) = without_turns.split_once("\"turns\":").unwrap_or_default();
        std::fs::write(dir.join("empty.json"), format!("{head}\"turns\":[]}}"))
            .unwrap_or_else(|e| panic!("{e}"));

        let reopened = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let titles: Vec<_> = reopened.list().into_iter().map(|row| row.title).collect();
        assert_eq!(titles, vec!["word word word word word word word word…"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_history_save_signals_the_live_attachment() {
        let dir = scratch("save-fails");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let mut attachment = store.attach(None).await;
        store.save(&attachment, &[ChatMessage::user("hi".to_string())], 1);
        store.flush().await;
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&dir, b"not a directory").unwrap_or_else(|e| panic!("{e}"));
        store.save(
            &attachment,
            &[
                ChatMessage::user("hi".to_string()),
                ChatMessage::assistant("hello".to_string()),
            ],
            1,
        );
        let signalled =
            tokio::time::timeout(std::time::Duration::from_secs(5), attachment.save_failed()).await;
        let _ = std::fs::remove_file(&dir);
        assert!(
            signalled.is_ok(),
            "a failed history save must signal the live attachment"
        );
    }

    fn attachment_file(dir: &std::path::Path, id: &str) -> Vec<u8> {
        std::fs::read(dir.join(format!("{id}.json"))).unwrap_or_default()
    }

    #[tokio::test(start_paused = true)]
    async fn a_takeover_does_not_wait_forever_for_a_stuck_old_connection() {
        let dir = scratch("handoff");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let mut old = store.attach(None).await;
        store.save(&old, &[ChatMessage::user("hi".to_string())], 1);

        let new = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            store.attach(Some(&old.id)),
        )
        .await
        .expect("the takeover waited for a connection that never answers");
        assert_eq!(new.id, old.id);
        assert_eq!(old.detached().await, Detach::Moved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_save_from_a_superseded_connection_changes_nothing() {
        let dir = scratch("late");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let old = store.attach(None).await;
        store.save(&old, &[ChatMessage::user("hi".to_string())], 1);

        let new = store.attach(Some(&old.id)).await;
        store.save(
            &old,
            &[
                ChatMessage::user("hi".to_string()),
                ChatMessage::assistant("late words".to_string()),
            ],
            9,
        );
        store.flush().await;
        let saved = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let history = saved.attach(Some(&new.id)).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            (history.history, history.next_turn),
            (vec![ChatMessage::user("hi".to_string())], 1)
        );
    }

    fn one(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage::user(text.to_string())]
    }

    #[tokio::test(start_paused = true)]
    async fn a_conversation_deleted_during_a_handoff_stays_deleted() {
        let dir = scratch("delete-during");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let mut old = store.attach(None).await;
        store.save(&old, &one("initial"), 1);
        let id = old.id.clone();
        let takeover = store.attach(Some(&id));
        tokio::pin!(takeover);
        assert!(futures_util::poll!(&mut takeover).is_pending());
        assert_eq!(old.detached().await, Detach::Moved);

        assert!(store.delete(&id).await.unwrap_or_else(|e| panic!("{e}")));
        store.save(&old, &one("partial reply after delete"), 2);
        store.release(old);
        let _ = takeover.await;
        store.flush().await;
        let remaining = SessionStore::open(&dir)
            .unwrap_or_else(|e| panic!("{e}"))
            .list();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(remaining, vec![], "the deleted conversation came back");
    }

    #[tokio::test(start_paused = true)]
    async fn a_takeover_of_a_takeover_revokes_every_older_connection() {
        let dir = scratch("chain");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let oldest = store.attach(None).await;
        store.save(&oldest, &one("initial"), 1);
        let id = oldest.id.clone();
        let middle = store.attach(Some(&id));
        tokio::pin!(middle);
        assert!(futures_util::poll!(&mut middle).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let newest = store.attach(Some(&id));
        tokio::pin!(newest);
        assert!(futures_util::poll!(&mut newest).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(2)).await;

        // Both deadlines have passed and the newest resumes first.
        let newest = newest.await;
        store.save(&newest, &one("newest history"), 3);
        store.save(&oldest, &one("stale oldest history"), 1);
        let _ = middle.await;
        store.flush().await;
        let reopened = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let history = reopened.attach(Some(&id)).await.history;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(history, one("newest history"));
    }

    #[tokio::test]
    async fn history_search_uses_live_turns_and_optionally_every_saved_session() {
        let dir = scratch("history-search");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let current = store.attach(None).await;
        store.save(&current, &one("Earlier decision from disk"), 1);
        let other = store.attach(None).await;
        store.save(&other, &one("Past decision in another session"), 1);
        let live = one("Current decision not yet saved");
        let all = parse_arguments(r#"{"query":"decision"}"#).unwrap_or_else(|e| panic!("{e}"));
        let matches = store.search_history(&current.id, &live, &all);
        assert_eq!(matches.len(), 2);
        assert!(
            matches
                .iter()
                .any(|found| found.session_id == current.id
                    && found.excerpt.contains("not yet saved"))
        );
        assert!(matches.iter().any(|found| found.session_id == other.id));
        assert!(
            !matches
                .iter()
                .any(|found| found.excerpt.contains("from disk"))
        );
        let current_only = parse_arguments(r#"{"query":"decision","scope":"current"}"#)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            store
                .search_history(&current.id, &live, &current_only)
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn retirement_replaces_an_old_transcript_with_a_summary_that_survives_reopen() {
        let dir = scratch("retire");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let attachment = store.attach(None).await;
        store.save(&attachment, &one("Full transcript that must go"), 1);
        store.release(attachment);
        let candidate = store
            .retirement_candidates(u64::MAX)
            .into_iter()
            .next()
            .expect("one old inactive session");
        assert!(
            !store
                .retire(&candidate.id, candidate.updated_ms + 1, "Wrong version")
                .await
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert!(
            store
                .retire(
                    &candidate.id,
                    candidate.updated_ms,
                    "Keep only this summary."
                )
                .await
                .unwrap_or_else(|e| panic!("{e}"))
        );
        let reopened = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(reopened.list().len(), 1);
        let history = reopened.attach(Some(&candidate.id)).await.history;
        assert_eq!(history.len(), 1);
        assert!(history[0].content.contains("Keep only this summary."));
        assert!(!history[0].content.contains("Full transcript"));
        assert!(
            reopened.retirement_candidates(u64::MAX).is_empty(),
            "a retired summary must not be summarized again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_retirement_write_keeps_the_full_transcript() {
        let dir = scratch("retire-fails");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let attachment = store.attach(None).await;
        store.save(&attachment, &one("Do not lose this"), 1);
        store.release(attachment);
        let candidate = store
            .retirement_candidates(u64::MAX)
            .into_iter()
            .next()
            .expect("one old inactive session");
        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&dir, "in the way").unwrap_or_else(|e| panic!("{e}"));
        assert!(
            store
                .retire(&candidate.id, candidate.updated_ms, "Summary")
                .await
                .is_err()
        );
        let history = store.attach(Some(&candidate.id)).await.history;
        assert_eq!(history, one("Do not lose this"));
        let _ = std::fs::remove_file(&dir);
    }

    #[tokio::test]
    async fn reopening_before_retirement_keeps_the_full_transcript_on_disk() {
        let dir = scratch("retire-reopen");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let old = store.attach(None).await;
        let id = old.id.clone();
        store.save(&old, &one("Full original transcript"), 1);
        store.flush().await;
        store.release(old);
        let candidate = store
            .retirement_candidates(u64::MAX)
            .into_iter()
            .next()
            .expect("one old inactive session");
        let resume = pause_writer(&store).await;
        let mut retiring = Box::pin(store.retire(&id, candidate.updated_ms, "Short summary"));
        assert!(futures_util::poll!(&mut retiring).is_pending());
        let reopened = store.attach(Some(&id)).await;
        resume.send(()).expect("resume writer");
        assert!(!retiring.await.unwrap_or_else(|e| panic!("{e}")));
        assert_eq!(reopened.history, one("Full original transcript"));
        let disk = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            disk.attach(Some(&id)).await.history,
            one("Full original transcript")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_save_during_delete_cannot_resurrect_the_conversation() {
        let dir = scratch("delete-save");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let live = store.attach(None).await;
        store.save(&live, &one("Initial"), 1);
        store.flush().await;
        let resume = pause_writer(&store).await;
        let mut deleting = Box::pin(store.delete(&live.id));
        assert!(futures_util::poll!(&mut deleting).is_pending());
        store.save(
            &live,
            &[
                ChatMessage::user("Initial".to_string()),
                ChatMessage::assistant("Late save".to_string()),
            ],
            2,
        );
        resume.send(()).expect("resume writer");
        assert!(deleting.await.unwrap_or_else(|e| panic!("{e}")));
        assert_eq!(
            SessionStore::open(&dir)
                .unwrap_or_else(|e| panic!("{e}"))
                .list(),
            Vec::new()
        );
        store.flush().await;
        assert_eq!(store.list().len(), 0);
        assert_eq!(
            SessionStore::open(&dir)
                .unwrap_or_else(|e| panic!("{e}"))
                .list()
                .len(),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_title_change_during_rename_cannot_overwrite_the_users_title() {
        let dir = scratch("rename-title");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let live = store.attach(None).await;
        store.save(&live, &one("Original title"), 1);
        store.flush().await;
        let resume = pause_writer(&store).await;
        let mut renaming = Box::pin(store.rename(&live.id, "User title"));
        assert!(futures_util::poll!(&mut renaming).is_pending());
        store.set_model_title(&live.id, "Model title");
        store.save(&live, &one("Latest transcript"), 2);
        resume.send(()).expect("resume writer");
        assert!(renaming.await.unwrap_or_else(|e| panic!("{e}")));
        let reopened = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(reopened.list()[0].title, "User title");
        assert_eq!(
            reopened.attach(Some(&live.id)).await.history,
            one("Latest transcript")
        );
        store.flush().await;
        let reopened = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(reopened.list()[0].title, "User title");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

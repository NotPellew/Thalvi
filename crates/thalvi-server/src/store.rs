//! Owns saved conversations on disk and which connection has each one open.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

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
    detach: oneshot::Sender<Detach>,
    released: oneshot::Receiver<()>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Stored>,
    live: HashMap<String, Live>,
}

enum DiskOp {
    Save { id: String, json: Vec<u8> },
    Remove(String),
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
}

impl Attachment {
    /// Resolves when the conversation must be let go; pends forever if the store is gone.
    pub async fn detached(&mut self) -> Detach {
        match (&mut self.detach).await {
            Ok(reason) => reason,
            Err(_) => std::future::pending().await,
        }
    }
}

/// Conversations kept under one directory until the user deletes them.
pub struct SessionStore {
    state: Mutex<State>,
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
        std::thread::spawn(move || write_loop(&dir, &queue));
        Ok(Self {
            state: Mutex::new(State {
                sessions,
                live: HashMap::new(),
            }),
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
        let (id, previous) = {
            let mut state = self.lock();
            let id = resume
                .filter(|id| state.sessions.contains_key(*id))
                .map_or_else(|| self.new_id(), str::to_string);
            let live = Live {
                dead: Arc::clone(&dead),
                detach: detach_tx,
                released: released_rx,
            };
            let previous = state.live.insert(id.clone(), live);
            (id, previous)
        };
        if let Some(previous) = previous
            && previous.detach.send(Detach::Moved).is_ok()
        {
            let _ = tokio::time::timeout(HANDOFF_WAIT, previous.released).await;
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
            });
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

    /// Sets a title the user typed; false when the conversation does not exist.
    pub fn rename(&self, id: &str, title: &str) -> bool {
        self.retitle(id, title, TitleSource::User)
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

    /// Deletes a conversation and tells the connection holding it; false if unknown.
    pub fn delete(&self, id: &str) -> bool {
        let mut state = self.lock();
        let existed = state.sessions.remove(id).is_some();
        if let Some(live) = state.live.remove(id) {
            live.dead.store(true, Ordering::SeqCst);
            let _ = live.detach.send(Detach::Deleted);
        }
        if existed {
            let _ = self.writer.send(DiskOp::Remove(id.to_string()));
        }
        existed
    }

    /// Waits until everything saved so far has reached the disk.
    pub async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.writer.send(DiskOp::Flush(done)).is_ok() {
            let _ = flushed.await;
        }
    }

    fn queue_save(&self, stored: &Stored) {
        match serde_json::to_vec(stored) {
            Ok(json) => {
                let _ = self.writer.send(DiskOp::Save {
                    id: stored.id.clone(),
                    json,
                });
            }
            Err(error) => tracing::warn!(%error, id = %stored.id, "could not encode session"),
        }
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
    if stored.turns.is_empty() {
        return Err("the session has no turns".to_string());
    }
    Ok(stored)
}

fn write_loop(dir: &Path, queue: &mpsc::Receiver<DiskOp>) {
    while let Ok(message) = queue.recv() {
        match message {
            DiskOp::Save { id, json } => {
                if let Err(error) = write_atomic(dir, &id, &json) {
                    tracing::warn!(%error, %id, "could not save session");
                }
            }
            DiskOp::Remove(id) => {
                if let Err(error) = std::fs::remove_file(session_path(dir, &id))
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(%error, %id, "could not remove session");
                }
            }
            DiskOp::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
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
    use crate::model::ChatMessage;

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("thalvi-store-test-{tag}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn unreadable_and_empty_files_are_skipped_and_long_multiline_first_lines_are_cut() {
        let dir = scratch("load");
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let attachment = store.attach(None).await;
        let question = format!("\n  {}  \nsecond line", "word ".repeat(20));
        store.save(&attachment, &[ChatMessage::user(question)], 1);
        store.flush().await;
        std::fs::write(dir.join("broken.json"), "{").unwrap_or_else(|e| panic!("{e}"));
        let json = String::from_utf8_lossy(&attachment_file(&dir, &attachment.id)).to_string();
        let without_turns = json.replace(&attachment.id, "empty");
        let (head, _) = without_turns.split_once("\"turns\":").unwrap_or_default();
        std::fs::write(dir.join("empty.json"), format!("{head}\"turns\":[]}}"))
            .unwrap_or_else(|e| panic!("{e}"));

        let reopened = SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}"));
        let titles: Vec<_> = reopened.list().into_iter().map(|row| row.title).collect();
        assert_eq!(titles, vec!["word word word word word word word word…"]);
        let _ = std::fs::remove_dir_all(&dir);
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
}

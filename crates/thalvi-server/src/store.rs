//! Owns saved session histories so a reconnecting client can rejoin.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use crate::model::ChatMessage;

/// How long a closed session's history is kept for rejoining.
pub const RETENTION: Duration = Duration::from_mins(10);

#[derive(Debug)]
struct Saved {
    history: Vec<ChatMessage>,
    expires_at: Instant,
}

/// In-memory histories of recently closed sessions.
#[derive(Debug)]
pub struct SessionStore {
    saved: Mutex<HashMap<String, Saved>>,
    ttl: Duration,
    counter: AtomicU64,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    /// Creates a store with the standard 10-minute retention.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(RETENTION)
    }

    /// Creates a store keeping histories for `ttl` after a socket closes.
    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            saved: Mutex::new(HashMap::new()),
            ttl,
            counter: AtomicU64::new(1),
        }
    }

    /// Generates a new opaque session id.
    #[must_use]
    pub fn new_id(&self) -> String {
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        format!("s{count:x}-{nanos:x}")
    }

    /// Takes the saved history for `id`, if present and not expired.
    pub fn take(&self, id: &str) -> Option<Vec<ChatMessage>> {
        let mut saved = self.saved.lock().ok()?;
        let entry = saved.remove(id)?;
        if entry.expires_at > Instant::now() {
            Some(entry.history)
        } else {
            None
        }
    }

    /// Saves `history` under `id` for the retention period.
    pub fn save(&self, id: String, history: Vec<ChatMessage>) {
        if let Ok(mut saved) = self.saved.lock() {
            let now = Instant::now();
            saved.retain(|_, entry| entry.expires_at > now);
            saved.insert(
                id,
                Saved {
                    history,
                    expires_at: Instant::now() + self.ttl,
                },
            );
        }
    }

    /// Whether a non-expired history is saved under `id`.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.saved.lock().is_ok_and(|saved| {
            saved
                .get(id)
                .is_some_and(|entry| entry.expires_at > Instant::now())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SessionStore;
    use crate::model::ChatMessage;
    use std::time::Duration;

    #[test]
    fn saved_history_is_taken_once_and_unknown_or_expired_ids_miss() {
        let store = SessionStore::with_ttl(Duration::from_secs(60));
        store.save(
            "known".to_string(),
            vec![ChatMessage::user("hi".to_string())],
        );
        assert_eq!(
            store.take("known"),
            Some(vec![ChatMessage::user("hi".to_string())])
        );
        assert_eq!(store.take("known"), None);
        assert_eq!(store.take("unknown"), None);

        let expired = SessionStore::with_ttl(Duration::ZERO);
        expired.save("old".to_string(), vec![ChatMessage::user("hi".to_string())]);
        assert_eq!(expired.take("old"), None);
        assert!(!expired.contains("old"));
    }
}

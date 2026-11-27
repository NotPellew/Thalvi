//! Learns from the user's cleanup of a pasted dictation through one text field.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{oneshot, watch};

use crate::metrics::{Event, Metrics};

/// Why reading the field ended. The watch stops on the first of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldEnd {
    /// Focus moved out of the bound window.
    FocusLeft,
    /// The window closed.
    Closed,
    /// The field refuses to share its text, such as a password field.
    Protected,
    /// The field's text could not be read.
    Unreadable,
}

/// Why no field could be watched. The plain sentence is shown on the switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldLimit {
    Desktop(&'static str),
    NoAccessibility,
    NotShared { app: String },
}

impl FieldLimit {
    /// The sentence Settings shows.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Desktop(reason) => (*reason).to_owned(),
            Self::NoAccessibility => {
                "Thalvi can't reach this desktop's accessibility service.".to_owned()
            }
            Self::NotShared { app } => format!(
                "{app} shares no text with Thalvi. For Chromium and Electron apps such as T3 Code, start them with ACCESSIBILITY_ENABLED=1 and --force-renderer-accessibility."
            ),
        }
    }
}

/// The whole text in one editable field, now. Blocks until it is read.
pub trait Field: Send {
    /// # Errors
    /// Why the field can no longer be read.
    fn text(&mut self) -> Result<String, FieldEnd>;
}

/// Armed just before a paste and resolved just after it, waiting up to a second for the field.
pub trait FieldProbe: Send {
    /// # Errors
    /// Why no field could be watched.
    fn field(self: Box<Self>, pasted: &str) -> Result<Box<dyn Field>, FieldLimit>;
}

/// The anchors around the pasted span, taken from the first read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    before: String,
    after: String,
}

impl Span {
    /// Anchors `pasted` in `text`, keeping up to 200 characters on each side. `None` when the
    /// paste is not in the field yet.
    #[must_use]
    pub fn at(pasted: &str, text: &str) -> Option<Self> {
        let at = text.find(pasted)?;
        let mut start = at.saturating_sub(200);
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        let paste_end = at + pasted.len();
        let mut end = (paste_end + 200).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        Some(Self {
            before: text[start..at].to_owned(),
            after: text[paste_end..end].to_owned(),
        })
    }

    /// The text between the anchors when each is found once; empty anchors mean the field's start
    /// or end.
    #[must_use]
    pub fn between(&self, text: &str) -> Option<String> {
        let start = if self.before.is_empty() {
            0
        } else {
            once(text, &self.before)? + self.before.len()
        };
        let end = if self.after.is_empty() {
            text.len()
        } else {
            once(text, &self.after)?
        };
        if start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return None;
        }
        Some(text[start..end].to_owned())
    }
}

fn once(text: &str, needle: &str) -> Option<usize> {
    let mut found = text.match_indices(needle).map(|(at, _)| at);
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// The anchors from the first read and the latest version found between them.
#[derive(Debug)]
pub struct Spans {
    pasted: String,
    anchor: Option<Span>,
    kept: Option<String>,
}

impl Spans {
    /// Starts watching for `pasted` in a field.
    #[must_use]
    pub fn new(pasted: impl Into<String>) -> Self {
        Self {
            pasted: pasted.into(),
            anchor: None,
            kept: None,
        }
    }

    /// Reads the field now, taking anchors on the first read and keeping the latest span found.
    pub fn observe(&mut self, text: &str) {
        if self.anchor.is_none() {
            self.anchor = Span::at(&self.pasted, text);
        }
        if let Some(anchor) = &self.anchor
            && let Some(span) = anchor.between(text)
            && !span.trim().is_empty()
        {
            self.kept = Some(span);
        }
    }

    /// The latest cleaned-up version, when it differs from what was pasted.
    #[must_use]
    pub fn kept(&self) -> Option<&str> {
        match &self.kept {
            Some(kept) if kept != &self.pasted => Some(kept),
            _ => None,
        }
    }
}

/// A name-like word the user corrected, with the words Thalvi wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub word: String,
    pub heard: String,
}

/// The name-like words the user corrected, skipping edits that are not names.
#[must_use]
pub fn suggestions(pasted: &str, cleaned: &str, known: &[String]) -> Vec<Suggestion> {
    let heard = words(pasted);
    let wrote = words(cleaned);
    let edits = diff(&heard, &wrote);
    let inserted: usize = edits
        .iter()
        .map(|edit| edit.wrote.len().saturating_sub(edit.heard.len()))
        .sum();
    let changed: usize = edits
        .iter()
        .map(|edit| edit.wrote.len() + edit.heard.len())
        .sum();
    if inserted > 3 && changed * 10 > 6 * heard.len().max(1) {
        return Vec::new();
    }
    let mut found = Vec::new();
    for edit in edits {
        for (offset, word) in edit.wrote.iter().enumerate() {
            let index = edit.wrote_start + offset;
            let previous = index.checked_sub(1).and_then(|at| wrote.get(at));
            if !name_like(word, previous)
                || known
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(word.trim_matches(not_word)))
                || found.len() >= 5
            {
                continue;
            }
            let heard = if edit.heard.len() == edit.wrote.len() {
                edit.heard[offset].to_owned()
            } else {
                edit.heard.join(" ")
            };
            found.push(Suggestion {
                word: word.trim_matches(not_word).to_owned(),
                heard,
            });
        }
    }
    found
}

fn not_word(c: char) -> bool {
    !c.is_alphanumeric() && c != '\''
}

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

fn key(word: &str) -> &str {
    word.trim_matches(not_word)
}

/// One run of changed words, with the span it replaced.
struct Edit<'a> {
    heard: Vec<&'a str>,
    wrote: Vec<&'a str>,
    wrote_start: usize,
}

fn diff<'a>(heard: &[&'a str], wrote: &[&'a str]) -> Vec<Edit<'a>> {
    let (before, after) = (heard.len(), wrote.len());
    let mut table = vec![vec![0usize; after + 1]; before + 1];
    for i in (0..before).rev() {
        for j in (0..after).rev() {
            table[i][j] = if key(heard[i]) == key(wrote[j]) {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(usize, bool, bool)> = Vec::new();
    let (mut i, mut j) = (before, after);
    while i > 0 && j > 0 {
        if key(heard[i - 1]) == key(wrote[j - 1]) {
            ops.push((i - 1, false, false));
            i -= 1;
            j -= 1;
        } else {
            let tie = table[i - 1][j] == table[i][j - 1];
            let heard_matches_later = wrote[..j].iter().any(|word| key(word) == key(heard[i - 1]));
            if table[i - 1][j] > table[i][j - 1] || (tie && !heard_matches_later) {
                ops.push((i - 1, true, false));
                i -= 1;
            } else {
                ops.push((j - 1, false, true));
                j -= 1;
            }
        }
    }
    while i > 0 {
        i -= 1;
        ops.push((i, true, false));
    }
    while j > 0 {
        j -= 1;
        ops.push((j, false, true));
    }
    ops.reverse();

    let mut edits = Vec::new();
    let (mut heard_run, mut wrote_run) = (Vec::new(), Vec::new());
    let mut wrote_start = 0;
    for (index, drop, add) in ops {
        if !drop && !add {
            if !heard_run.is_empty() || !wrote_run.is_empty() {
                edits.push(Edit {
                    heard: std::mem::take(&mut heard_run),
                    wrote: std::mem::take(&mut wrote_run),
                    wrote_start,
                });
            }
            continue;
        }
        if drop {
            heard_run.push(heard[index]);
        }
        if add {
            if wrote_run.is_empty() {
                wrote_start = index;
            }
            wrote_run.push(wrote[index]);
        }
    }
    if !heard_run.is_empty() || !wrote_run.is_empty() {
        edits.push(Edit {
            heard: heard_run,
            wrote: wrote_run,
            wrote_start,
        });
    }
    edits
}

/// Capitalized mid-sentence, all caps, or a mix of letters and digits.
fn name_like(word: &str, previous: Option<&&str>) -> bool {
    let core = word.trim_matches(not_word);
    let letters = core.chars().filter(|c| c.is_alphabetic()).count();
    let digits = core.chars().filter(char::is_ascii_digit).count();
    if letters > 0 && digits > 0 {
        return true;
    }
    if letters >= 2 && core.chars().all(char::is_uppercase) {
        return true;
    }
    if core.chars().next().is_some_and(char::is_uppercase)
        && core.chars().skip(1).all(|c| !c.is_uppercase())
    {
        return !previous.is_none_or(|word| {
            word.chars()
                .last()
                .is_some_and(|c| matches!(c, '.' | '!' | '?'))
        });
    }
    false
}

/// What one watch kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Learned {
    pub corrected: Option<String>,
    pub suggestions: Vec<Suggestion>,
}

impl Learned {
    /// Records one watch: its outcome, counts and duration, never field text.
    pub fn record(&self, metrics: &Metrics, outcome: &str, reads: usize, duration: Duration) {
        metrics.metric(
            Event::FieldWatch,
            outcome,
            Some(duration),
            serde_json::json!({"reads": reads, "suggestions": self.suggestions.len()}),
        );
    }
}

/// What the watch returns to the app.
#[derive(PartialEq, Eq)]
enum End {
    Field(FieldEnd),
    Cancel,
    Switch,
    Timeout,
}

/// Watches one field for up to five minutes, reading every 250 ms.
pub async fn watch(
    field: Result<Box<dyn Field>, FieldLimit>,
    pasted: &str,
    known: &[String],
    mut enabled: watch::Receiver<bool>,
    mut cancel: oneshot::Receiver<()>,
    metrics: &Metrics,
) -> Learned {
    let started = tokio::time::Instant::now();
    let mut field: Option<Box<dyn Field>> = match field {
        Ok(field) => Some(field),
        Err(limit) => {
            let learned = Learned::default();
            let outcome = format!("limit:{}", limit_kind(&limit));
            learned.record(metrics, &outcome, 0, started.elapsed());
            return learned;
        }
    };
    let mut spans = Spans::new(pasted);
    let mut reads = 0;
    let mut ticks = tokio::time::interval(Duration::from_millis(250));
    let deadline = started + Duration::from_secs(300);
    let end = loop {
        if !*enabled.borrow_and_update() {
            break End::Switch;
        }
        tokio::select! {
            biased;
            _ = &mut cancel => {
                break End::Cancel;
            }
            changed = enabled.changed() => {
                if changed.is_err() || !*enabled.borrow_and_update() {
                    break End::Switch;
                }
            }
            _ = ticks.tick() => {
                if tokio::time::Instant::now() >= deadline {
                    break End::Timeout;
                }
                let Some(mut taken) = field.take() else {
                    break End::Field(FieldEnd::Unreadable);
                };
                let (returned, read) = match tokio::task::spawn_blocking(move || {
                    let read: Result<String, FieldEnd> = taken.text();
                    (Some(taken), read)
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => (None, Err(FieldEnd::Unreadable)),
                };
                field = returned;
                reads += 1;
                match read {
                    Ok(text) => spans.observe(&text),
                    Err(reason) => break End::Field(reason),
                }
            }
        }
    };
    let corrected = spans.kept().map(str::to_owned);
    let suggestions = corrected
        .as_deref()
        .map(|cleaned| suggestions(pasted, cleaned, known))
        .unwrap_or_default();
    let learned = Learned {
        corrected,
        suggestions,
    };
    let outcome = match end {
        End::Field(_) if learned.corrected.is_some() => "corrected",
        End::Field(_) => "unchanged",
        End::Cancel => "ended:cancel",
        End::Switch => "ended:switch",
        End::Timeout => "ended:timeout",
    };
    learned.record(metrics, outcome, reads, started.elapsed());
    learned
}

fn limit_kind(limit: &FieldLimit) -> &'static str {
    match limit {
        FieldLimit::Desktop(_) => "desktop",
        FieldLimit::NoAccessibility => "no_accessibility",
        FieldLimit::NotShared { .. } => "not_shared",
    }
}

/// The suggestions and latest limit shown on the switch, kept only in memory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningView {
    pub suggestions: Vec<Suggestion>,
    pub limit: Option<String>,
}

#[derive(Default)]
struct State {
    suggestions: Vec<Suggestion>,
    limit: Option<FieldLimit>,
}

/// Suggestions and the latest field limit, shared by the engine, the commands and the UI.
#[derive(Default)]
pub struct Learning {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl Learning {
    /// A receiver that changes whenever a suggestion or the limit changes.
    #[must_use]
    pub fn changed(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// What Settings shows now.
    #[must_use]
    pub fn view(&self) -> LearningView {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        LearningView {
            suggestions: state.suggestions.clone(),
            limit: state.limit.as_ref().map(FieldLimit::reason),
        }
    }

    /// Keeps the result of a watch: at most ten suggestions, skipping words already pending.
    pub fn offer(&self, learned: Learned) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for suggestion in learned.suggestions {
            if state.suggestions.len() >= 10
                || state
                    .suggestions
                    .iter()
                    .any(|known| known.word == suggestion.word)
            {
                continue;
            }
            state.suggestions.push(suggestion);
        }
        state.limit = None;
        self.changed.send_modify(|version| *version += 1);
    }

    /// Clears the last reason once a field could be watched.
    pub fn clear_limit(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.limit.take().is_some() {
            self.changed.send_modify(|version| *version += 1);
        }
    }

    /// Keeps why a field could not be watched for the switch.
    pub fn limit(&self, limit: FieldLimit) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.limit = Some(limit);
        self.changed.send_modify(|version| *version += 1);
    }

    /// Removes a suggestion the user accepted or dismissed.
    pub fn remove(&self, word: &str) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let before = state.suggestions.len();
        state
            .suggestions
            .retain(|suggestion| suggestion.word != word);
        if state.suggestions.len() != before {
            self.changed.send_modify(|version| *version += 1);
        }
    }

    /// Whether a word is still offered.
    #[must_use]
    pub fn has(&self, word: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .suggestions
            .iter()
            .any(|suggestion| suggestion.word == word)
    }
}

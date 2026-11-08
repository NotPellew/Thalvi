//! Transcript accumulation, ending phrases, scratch and cancel, and the pending silence window.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::time::Duration;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::Script;
use crate::tasks::{self, Task};

/// The most characters one capture holds.
pub const MAX_CAPTURE_CHARS: usize = 20_000;

/// The most characters between the delimiters of an artifact such as `[noise]`.
const ARTIFACT_BODY_MAX: usize = 32;

/// The most artifacts that may follow an ending phrase.
const ENDING_ARTIFACTS_MAX: usize = 2;

const DEFAULT_ENDINGS: [(&str, Ending); 6] = [
    ("paste that", Ending::Paste),
    ("finish dictation", Ending::Paste),
    ("submit", Ending::Submit),
    ("do that", Ending::Task),
    ("scratch that", Ending::Scratch),
    ("never mind", Ending::Cancel),
];

/// What a spoken ending phrase does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ending {
    Paste,
    Submit,
    Task,
    Scratch,
    Cancel,
}

impl Ending {
    const ALL: [Self; 5] = [
        Self::Paste,
        Self::Submit,
        Self::Task,
        Self::Scratch,
        Self::Cancel,
    ];
}

/// The endings that deliver the capture; scratch and cancel never do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureRoute {
    Paste,
    Submit,
    Task,
}

impl CaptureRoute {
    /// The route's name in recordings.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Paste => "paste",
            Self::Submit => "submit",
            Self::Task => "task",
        }
    }
}

/// Why a set of ending phrases was rejected. Messages are shown to the user as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EndingsError {
    #[error("An ending phrase must not be empty.")]
    Empty,
    #[error("Ending phrases must be distinct.")]
    Duplicate,
    #[error("Configure paste, submit, task, scratch, and cancel endings.")]
    Incomplete,
}

/// The phrases that end or edit a capture, validated once: lowercase single-spaced, distinct,
/// and covering every [`Ending`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "BTreeMap<String, Ending>")]
pub struct Endings(Vec<(String, Ending)>);

impl TryFrom<BTreeMap<String, Ending>> for Endings {
    type Error = EndingsError;

    fn try_from(phrases: BTreeMap<String, Ending>) -> Result<Self, EndingsError> {
        let mut entries: Vec<(String, Ending)> = Vec::new();
        for (phrase, ending) in phrases {
            let phrase = phrase
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if phrase.is_empty() {
                return Err(EndingsError::Empty);
            }
            if entries.iter().any(|(known, _)| *known == phrase) {
                return Err(EndingsError::Duplicate);
            }
            entries.push((phrase, ending));
        }
        if Ending::ALL
            .iter()
            .any(|ending| entries.iter().all(|(_, known)| known != ending))
        {
            return Err(EndingsError::Incomplete);
        }
        Ok(Self::longest_first(entries))
    }
}

impl Serialize for Endings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (phrase, ending) in &self.0 {
            map.serialize_entry(phrase, ending)?;
        }
        map.end()
    }
}

impl Default for Endings {
    fn default() -> Self {
        Self::longest_first(
            DEFAULT_ENDINGS
                .iter()
                .map(|(phrase, ending)| ((*phrase).to_owned(), *ending))
                .collect(),
        )
    }
}

impl Endings {
    /// Every phrase with its ending, longest phrase first.
    pub fn entries(&self) -> impl Iterator<Item = (&str, Ending)> {
        self.0
            .iter()
            .map(|(phrase, ending)| (phrase.as_str(), *ending))
    }

    /// Longest first, so "please submit" wins over "submit".
    fn longest_first(mut entries: Vec<(String, Ending)>) -> Self {
        entries.sort_by_key(|(phrase, _)| (Reverse(phrase.chars().count()), phrase.clone()));
        Self(entries)
    }

    /// The first ending, longest phrase first, that `text` ends with (allowing trailing
    /// artifacts), and the text before its phrase.
    fn find<'a>(&self, text: &'a str) -> Option<(Ending, &'a str)> {
        let chars: Vec<char> = text.chars().collect();
        for (phrase, ending) in &self.0 {
            let starts = (0..chars.len()).filter(|&at| at == 0 || chars[at - 1].is_whitespace());
            for start in starts {
                if let Some(end) = phrase_end(&chars, start, phrase)
                    && artifacts_to_end(&chars[end..], 0, ENDING_ARTIFACTS_MAX)
                {
                    let byte = text.char_indices().nth(start).map_or(0, |(byte, _)| byte);
                    return Some((*ending, text[..byte].trim()));
                }
            }
        }
        None
    }
}

/// The speech silence the user must keep after each delivering ending before it is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RawEndingPauses")]
pub struct EndingPauses {
    paste: Duration,
    submit: Duration,
    task: Duration,
}

/// A pause was outside the range the user may choose. Shown as written.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("The {ending} pause must be a number of seconds from 0 to 5.")]
pub struct PauseError {
    ending: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEndingPauses {
    paste: Option<f64>,
    submit: Option<f64>,
    task: Option<f64>,
}

impl TryFrom<RawEndingPauses> for EndingPauses {
    type Error = PauseError;

    fn try_from(raw: RawEndingPauses) -> Result<Self, PauseError> {
        let defaults = Self::default();
        let pause = |ending: &'static str, seconds: Option<f64>, default: Duration| match seconds {
            None => Ok(default),
            Some(seconds) if (0.0..=5.0).contains(&seconds) => Ok(Duration::from_secs_f64(seconds)),
            Some(_) => Err(PauseError { ending }),
        };
        Ok(Self {
            paste: pause("paste", raw.paste, defaults.paste)?,
            submit: pause("submit", raw.submit, defaults.submit)?,
            task: pause("task", raw.task, defaults.task)?,
        })
    }
}

impl Serialize for EndingPauses {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("paste", &self.paste.as_secs_f64())?;
        map.serialize_entry("submit", &self.submit.as_secs_f64())?;
        map.serialize_entry("task", &self.task.as_secs_f64())?;
        map.end()
    }
}

impl Default for EndingPauses {
    fn default() -> Self {
        Self {
            paste: Duration::from_millis(700),
            submit: Duration::from_millis(2500),
            task: Duration::from_millis(2500),
        }
    }
}

impl EndingPauses {
    #[must_use]
    pub fn get(self, route: CaptureRoute) -> Duration {
        match route {
            CaptureRoute::Paste => self.paste,
            CaptureRoute::Submit => self.submit,
            CaptureRoute::Task => self.task,
        }
    }
}

/// How long a capture may stay open without words, and in total, before it is stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawCaptureLimits")]
pub struct CaptureLimits {
    idle: Duration,
    max: Duration,
}

/// A limit was outside what a capture can sensibly use. Shown as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("The {limit} limit must be a whole number of seconds from {min} to {max}.")]
pub struct LimitError {
    limit: &'static str,
    min: u64,
    max: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCaptureLimits {
    idle: Option<u64>,
    max: Option<u64>,
}

impl TryFrom<RawCaptureLimits> for CaptureLimits {
    type Error = LimitError;

    fn try_from(raw: RawCaptureLimits) -> Result<Self, LimitError> {
        let defaults = Self::default();
        let limit =
            |limit: &'static str, range: (u64, u64), seconds: Option<u64>, default| match seconds {
                None => Ok(default),
                Some(seconds) if (range.0..=range.1).contains(&seconds) => {
                    Ok(Duration::from_secs(seconds))
                }
                Some(_) => Err(LimitError {
                    limit,
                    min: range.0,
                    max: range.1,
                }),
            };
        Ok(Self {
            idle: limit("idle", (10, 600), raw.idle, defaults.idle)?,
            max: limit("max", (60, 3600), raw.max, defaults.max)?,
        })
    }
}

impl Serialize for CaptureLimits {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("idle", &self.idle.as_secs())?;
        map.serialize_entry("max", &self.max.as_secs())?;
        map.end()
    }
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(60),
            max: Duration::from_secs(300),
        }
    }
}

impl CaptureLimits {
    /// How long a capture may go without recognized words.
    #[must_use]
    pub fn idle(self) -> Duration {
        self.idle
    }

    /// How long a capture may stay open, however many words it receives.
    #[must_use]
    pub fn max(self) -> Duration {
        self.max
    }
}

/// Where an armed ending is in its silence window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuietWindow {
    /// The ending arrived while the user was still audibly speaking; counting starts at the
    /// first quiet frame.
    WaitingForQuiet,
    /// Continuous quiet since `since`; the ending is delivered once the pause has passed.
    CountingDown { since: Instant },
}

/// An ending heard but not yet delivered: continued speech cancels it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingEnding {
    pub route: CaptureRoute,
    /// What will be delivered: the capture without the ending phrase.
    pub text: String,
    pub window: QuietWindow,
    /// The whole capture is a task and no ending was spoken.
    pub without_ending: bool,
}

/// What a delivered capture asks the caller to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRequest {
    pub route: CaptureRoute,
    pub text: String,
}

/// What one final transcript did to the capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalStep {
    /// The capture grew, was scratched, or the final was ignored; capture continues.
    /// `cancelled_pending` is true when it also cancelled an armed ending.
    Continue { cancelled_pending: bool },
    /// An ending is armed (replacing any earlier one) and its quiet window starts.
    Armed(PendingEnding),
    /// The user said "never mind": the capture is discarded.
    Cancelled {
        /// What was dictated before the phrase; nothing was delivered.
        dictated: Option<String>,
    },
}

/// The capture cannot take more text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("This capture is full. Finish or cancel it before continuing.")]
pub struct CaptureFullError;

/// What a wake-word capture listens for; a hold has none of it.
#[derive(Clone, Debug)]
struct Spoken {
    endings: Endings,
    /// The scripts whose phrase, spoken alone, runs without an ending.
    scripts: Vec<Script>,
}

/// One dictation: final transcripts in, an ending delivered after a quiet pause out.
/// Pure and synchronous; the caller supplies every instant.
#[derive(Clone, Debug)]
pub struct CaptureSession {
    spoken: Option<Spoken>,
    pauses: EndingPauses,
    pieces: Vec<String>,
    scratched: bool,
    pending: Option<PendingEnding>,
}

impl CaptureSession {
    #[must_use]
    pub fn new(endings: Endings, pauses: EndingPauses, scripts: Vec<Script>) -> Self {
        Self {
            spoken: Some(Spoken { endings, scripts }),
            pauses,
            pieces: Vec::new(),
            scratched: false,
            pending: None,
        }
    }

    /// A capture opened by holding the dictation key: no spoken phrase is an ending, so
    /// every word is dictated text.
    #[must_use]
    pub fn without_endings() -> Self {
        Self {
            spoken: None,
            pauses: EndingPauses::default(),
            pieces: Vec::new(),
            scratched: false,
            pending: None,
        }
    }

    /// Whether spoken phrases can end or edit this capture.
    #[must_use]
    pub fn has_endings(&self) -> bool {
        self.spoken.is_some()
    }

    /// Everything captured so far; an armed ending's phrase is still in it until delivery.
    #[must_use]
    pub fn text(&self) -> String {
        self.pieces.join(" ")
    }

    /// Whether the user said "scratch that" at any point.
    #[must_use]
    pub fn scratched(&self) -> bool {
        self.scratched
    }

    #[must_use]
    pub fn pending(&self) -> Option<&PendingEnding> {
        self.pending.as_ref()
    }

    /// When the armed ending's running countdown started; `None` until quiet starts.
    #[must_use]
    pub fn quiet_since(&self) -> Option<Instant> {
        match self.pending.as_ref()?.window {
            QuietWindow::CountingDown { since } => Some(since),
            QuietWindow::WaitingForQuiet => None,
        }
    }

    /// When the armed ending is delivered if the quiet continues; `None` until quiet starts.
    #[must_use]
    pub fn ready_at(&self) -> Option<Instant> {
        let pause = self.pauses.get(self.pending.as_ref()?.route);
        Some(self.quiet_since()? + pause)
    }

    /// Applies one final transcript. `speech` is whether the user is audibly speaking right now.
    ///
    /// # Errors
    /// Fails, leaving the capture unchanged, when the text would pass [`MAX_CAPTURE_CHARS`].
    pub fn on_final(
        &mut self,
        transcript: &str,
        now: Instant,
        speech: bool,
    ) -> Result<FinalStep, CaptureFullError> {
        let text = transcript.trim();
        if text.is_empty() || artifacts_to_end(&text.chars().collect::<Vec<_>>(), 1, 2) {
            return Ok(FinalStep::Continue {
                cancelled_pending: false,
            });
        }
        let combined = self
            .pieces
            .iter()
            .map(String::as_str)
            .chain([text])
            .collect::<Vec<_>>()
            .join(" ");
        let cancelled_pending = self.pending.is_some();
        let ending = self
            .spoken
            .as_ref()
            .and_then(|spoken| spoken.endings.find(&combined));
        match ending {
            Some((Ending::Cancel, before)) => {
                self.pieces.clear();
                self.pending = None;
                Ok(FinalStep::Cancelled {
                    dictated: (!before.trim().is_empty()).then(|| before.to_owned()),
                })
            }
            Some((Ending::Scratch, before)) => {
                self.scratched = true;
                let kept = kept_pieces(
                    self.pieces.iter().map(String::as_str).chain([text]),
                    before.chars().count(),
                );
                self.pieces.truncate(kept);
                self.pending = None;
                Ok(FinalStep::Continue { cancelled_pending })
            }
            Some((Ending::Paste, before)) => {
                self.arm(CaptureRoute::Paste, before, text, now, speech, false)
            }
            Some((Ending::Submit, before)) => {
                self.arm(CaptureRoute::Submit, before, text, now, speech, false)
            }
            Some((Ending::Task, before)) => {
                self.arm(CaptureRoute::Task, before, text, now, speech, false)
            }
            None => {
                if combined.chars().count() > MAX_CAPTURE_CHARS {
                    return Err(CaptureFullError);
                }
                if self.is_whole_task(&combined) {
                    return self.arm(CaptureRoute::Task, &combined, text, now, speech, true);
                }
                self.pieces.push(text.to_owned());
                self.pending = None;
                Ok(FinalStep::Continue { cancelled_pending })
            }
        }
    }

    /// Whether `text` alone is a task that needs no spoken ending: a configured phrase, else
    /// `open <host>`, but not a Discord message, which matches on a prefix.
    fn is_whole_task(&self, text: &str) -> bool {
        self.spoken.as_ref().is_some_and(|spoken| {
            matches!(
                tasks::decide(text, &spoken.scripts),
                Ok(Task::OpenUrl { .. } | Task::FocusApp { .. })
            )
        })
    }

    fn arm(
        &mut self,
        route: CaptureRoute,
        before: &str,
        text: &str,
        now: Instant,
        speech: bool,
        without_ending: bool,
    ) -> Result<FinalStep, CaptureFullError> {
        if before.chars().count() > MAX_CAPTURE_CHARS {
            return Err(CaptureFullError);
        }
        self.pieces.push(text.to_owned());
        let window = match &self.pending {
            Some(pending) if pending.route == route && pending.text == before => pending.window,
            _ if speech => QuietWindow::WaitingForQuiet,
            _ => QuietWindow::CountingDown { since: now },
        };
        let pending = PendingEnding {
            route,
            text: before.to_owned(),
            window,
            without_ending,
        };
        self.pending = Some(pending.clone());
        Ok(FinalStep::Armed(pending))
    }

    /// Applies a partial transcript; returns whether it cancelled the armed ending.
    /// Empty and artifact-only partials (`[noise]`, `uh`) never do.
    pub fn on_partial(&mut self, transcript: &str) -> bool {
        if self.pending.is_none() || !has_words(transcript) {
            return false;
        }
        self.pending = None;
        true
    }

    /// Reports one quiet audio frame; returns whether it started the armed ending's countdown.
    pub fn on_quiet_frame(&mut self, now: Instant) -> bool {
        match self.pending.as_mut() {
            Some(PendingEnding {
                window: window @ QuietWindow::WaitingForQuiet,
                ..
            }) => {
                *window = QuietWindow::CountingDown { since: now };
                true
            }
            _ => false,
        }
    }

    /// Reports local speech while an ending is pending; returns whether it stopped a running
    /// countdown, which restarts at the next quiet frame. A recognized partial still cancels.
    pub fn on_local_speech(&mut self) -> bool {
        match self.pending.as_mut() {
            Some(PendingEnding {
                window: window @ QuietWindow::CountingDown { .. },
                ..
            }) => {
                *window = QuietWindow::WaitingForQuiet;
                true
            }
            _ => false,
        }
    }

    /// Delivers the armed ending once its quiet window has lasted the whole pause.
    pub fn poll_ready(&mut self, now: Instant) -> Option<CaptureRequest> {
        let ready_at = self.ready_at()?;
        if now < ready_at {
            return None;
        }
        let pending = self.pending.take()?;
        self.pieces = if pending.text.is_empty() {
            Vec::new()
        } else {
            vec![pending.text.clone()]
        };
        Some(CaptureRequest {
            route: pending.route,
            text: pending.text,
        })
    }
}

/// How many leading `pieces` survive "scratch that" when `before_len` characters precede the
/// phrase: the pieces wholly inside it, minus the last one if the phrase directly followed it.
fn kept_pieces<'a>(pieces: impl Iterator<Item = &'a str>, before_len: usize) -> usize {
    let mut used = 0;
    let mut kept = 0;
    for piece in pieces {
        let end = used + piece.chars().count();
        if end > before_len {
            break;
        }
        kept += 1;
        used = end + 1;
    }
    if kept > 0 && used >= before_len {
        kept -= 1;
    }
    kept
}

/// Where `phrase`'s words end if they start at `start`, separated by any punctuation or space.
fn phrase_end(chars: &[char], start: usize, phrase: &str) -> Option<usize> {
    let mut at = start;
    for (index, word) in phrase.split(' ').enumerate() {
        if index > 0 {
            let gap = chars[at..].iter().take_while(|c| is_separator(**c)).count();
            if gap == 0 {
                return None;
            }
            at += gap;
        }
        for expected in word.chars() {
            if !chars.get(at).is_some_and(|c| same_letter(*c, expected)) {
                return None;
            }
            at += 1;
        }
    }
    Some(at)
}

fn same_letter(heard: char, expected: char) -> bool {
    heard == expected || heard.to_lowercase().eq(expected.to_lowercase())
}

fn is_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | '!' | '?' | ',' | ';' | ':')
}

/// Whether `transcript` holds words, not only punctuation or one or two artifacts such as
/// `[noise]` or `uh`.
#[must_use]
pub fn has_words(transcript: &str) -> bool {
    let chars: Vec<char> = transcript.chars().collect();
    !chars.iter().all(|c| is_separator(*c)) && !artifacts_to_end(&chars, 1, 2)
}

/// Whether `chars` is only separators and `min..=max` artifacts, each followed by separators.
fn artifacts_to_end(chars: &[char], min: usize, max: usize) -> bool {
    let skipped = chars.iter().take_while(|c| is_separator(**c)).count();
    let rest = &chars[skipped..];
    if rest.is_empty() {
        return min == 0;
    }
    max > 0
        && artifact_lengths(rest)
            .into_iter()
            .any(|len| artifacts_to_end(&rest[len..], min.saturating_sub(1), max - 1))
}

/// Every length of an artifact that `rest` starts with: `[noise]`, `(breathing)`, `<|nospeech|>`,
/// and the filler sounds uh, um, hm, mh, ah and erm.
fn artifact_lengths(rest: &[char]) -> Vec<usize> {
    let mut lengths = Vec::new();
    delimited(rest, "[", "]", ['[', ']'], &mut lengths);
    delimited(rest, "(", ")", ['(', ')'], &mut lengths);
    delimited(rest, "<|", "|>", ['<', '>'], &mut lengths);
    for filler in ["uh", "um", "hm", "mh", "ah", "erm"] {
        filler_runs(rest, filler, 0, &mut lengths);
    }
    lengths
}

fn starts_with(chars: &[char], pattern: &str) -> bool {
    chars.len() >= pattern.len() && chars.iter().zip(pattern.chars()).all(|(a, b)| *a == b)
}

fn delimited(rest: &[char], open: &str, close: &str, banned: [char; 2], out: &mut Vec<usize>) {
    if !starts_with(rest, open) {
        return;
    }
    let body = &rest[open.len()..];
    for count in 1..=ARTIFACT_BODY_MAX {
        match body.get(count - 1) {
            Some(c) if !banned.contains(c) && !matches!(c, '\r' | '\n') => {}
            _ => return,
        }
        if starts_with(&body[count..], close) {
            out.push(open.len() + count + close.len());
        }
    }
}

/// Lengths of `rest`'s prefixes made of one or more repeats of each letter of `letters` in turn.
fn filler_runs(rest: &[char], letters: &str, used: usize, out: &mut Vec<usize>) {
    let Some(letter) = letters.chars().next() else {
        out.push(used);
        return;
    };
    let run = rest[used..]
        .iter()
        .take_while(|c| c.eq_ignore_ascii_case(&letter))
        .count();
    for take in 1..=run {
        filler_runs(rest, &letters[1..], used + take, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> CaptureSession {
        CaptureSession::new(Endings::default(), EndingPauses::default(), Vec::new())
    }

    fn ms(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn endings(entries: &[(&str, Ending)]) -> Result<Endings, EndingsError> {
        Endings::try_from(
            entries
                .iter()
                .map(|(phrase, ending)| ((*phrase).to_owned(), *ending))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    /// Feeds finals with no speech and returns the last step.
    fn feed(session: &mut CaptureSession, finals: &[&str]) -> FinalStep {
        let now = Instant::now();
        let mut step = FinalStep::Continue {
            cancelled_pending: false,
        };
        for text in finals {
            step = session.on_final(text, now, false).unwrap();
        }
        step
    }

    fn armed(route: CaptureRoute, text: &str) -> (CaptureRoute, String) {
        (route, text.to_owned())
    }

    fn cancelled(dictated: &str) -> FinalStep {
        FinalStep::Cancelled {
            dictated: Some(dictated.to_owned()),
        }
    }

    fn armed_parts(step: &FinalStep) -> (CaptureRoute, String) {
        match step {
            FinalStep::Armed(pending) => (pending.route, pending.text.clone()),
            other => panic!("expected an armed ending, got {other:?}"),
        }
    }

    #[test]
    fn an_ending_delivers_the_capture_only_after_the_whole_pause() {
        for (ending, route, pause) in [
            ("paste that", CaptureRoute::Paste, 700),
            ("Paste that.", CaptureRoute::Paste, 700),
            ("SUBMIT!", CaptureRoute::Submit, 2500),
            ("do that.", CaptureRoute::Task, 2500),
        ] {
            let mut session = session();
            let base = Instant::now();
            let expected = "Explain how to submit a change. Then show the next test.";
            for text in [
                "Explain how to submit a change.",
                "Then show the next test.",
            ] {
                assert_eq!(
                    session.on_final(text, base, false),
                    Ok(FinalStep::Continue {
                        cancelled_pending: false
                    })
                );
            }
            let step = session.on_final(ending, base, false).unwrap();
            assert_eq!(armed_parts(&step), armed(route, expected), "{ending}");
            assert_eq!(session.poll_ready(ms(base, pause - 1)), None, "{ending}");
            assert_eq!(
                session.poll_ready(ms(base, pause)),
                Some(CaptureRequest {
                    route,
                    text: expected.to_owned()
                }),
                "{ending}"
            );
            assert_eq!(session.pending(), None);
            assert_eq!(session.text(), expected);
        }
    }

    #[test]
    fn an_ending_armed_during_speech_counts_down_from_the_first_quiet_frame() {
        let base = Instant::now();
        let mut session = session();
        let step = session
            .on_final("hello paste that", ms(base, 100), true)
            .unwrap();
        assert_eq!(
            step,
            FinalStep::Armed(PendingEnding {
                route: CaptureRoute::Paste,
                text: "hello".to_owned(),
                window: QuietWindow::WaitingForQuiet,
                without_ending: false,
            })
        );
        assert_eq!(session.ready_at(), None);
        assert_eq!(session.poll_ready(ms(base, 100_000)), None);

        assert!(session.on_quiet_frame(ms(base, 10_000)));
        assert!(!session.on_quiet_frame(ms(base, 10_080)));
        assert_eq!(
            session.pending().map(|pending| pending.window),
            Some(QuietWindow::CountingDown {
                since: ms(base, 10_000)
            })
        );
        assert_eq!(session.ready_at(), Some(ms(base, 10_700)));
        assert_eq!(session.poll_ready(ms(base, 10_699)), None);
        assert_eq!(
            session
                .poll_ready(ms(base, 10_700))
                .map(|request| request.text),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn local_speech_stops_a_running_countdown_until_quiet_returns() {
        let base = Instant::now();
        let mut session = session();
        assert!(!session.on_local_speech(), "no ending pending");
        session
            .on_final("hello paste that", ms(base, 0), false)
            .unwrap();
        assert!(session.on_local_speech());
        assert!(!session.on_local_speech(), "already waiting for quiet");
        assert_eq!(session.ready_at(), None);
        assert_eq!(session.poll_ready(ms(base, 100_000)), None);

        assert!(session.on_quiet_frame(ms(base, 3_000)));
        assert_eq!(session.poll_ready(ms(base, 3_699)), None);
        assert!(session.poll_ready(ms(base, 3_700)).is_some());
    }

    #[test]
    fn continued_speech_cancels_an_ending_and_a_new_one_restarts_the_pause() {
        let base = Instant::now();
        let mut session = session();
        session
            .on_final("hello submit", ms(base, 10_000), false)
            .unwrap();
        assert!(!session.on_partial(""));
        assert!(!session.on_partial("[noise]"));
        assert!(session.pending().is_some());
        assert!(session.on_partial("more"));
        assert_eq!(session.pending(), None);
        assert_eq!(session.poll_ready(ms(base, 12_500)), None);
        assert!(!session.on_partial("more again"));

        assert_eq!(
            session.on_final("  ", ms(base, 11_000), false),
            Ok(FinalStep::Continue {
                cancelled_pending: false
            })
        );
        assert_eq!(session.poll_ready(ms(base, 13_500)), None);
        session
            .on_final("more words submit", ms(base, 13_000), false)
            .unwrap();
        assert_eq!(session.poll_ready(ms(base, 15_499)), None);
        assert_eq!(
            session.poll_ready(ms(base, 15_500)),
            Some(CaptureRequest {
                route: CaptureRoute::Submit,
                text: "hello submit more words".to_owned()
            })
        );
    }

    #[test]
    fn a_later_ending_replaces_the_earlier_one_and_restarts_the_pause() {
        let base = Instant::now();
        let mut session = session();
        session
            .on_final("hello submit", ms(base, 10_000), false)
            .unwrap();
        session
            .on_final("more words do that", ms(base, 11_000), false)
            .unwrap();
        assert_eq!(session.poll_ready(ms(base, 13_499)), None);
        assert_eq!(
            session.poll_ready(ms(base, 13_500)),
            Some(CaptureRequest {
                route: CaptureRoute::Task,
                text: "hello submit more words".to_owned()
            })
        );
    }

    #[test]
    fn a_final_that_leaves_the_ending_unchanged_keeps_its_pause() {
        let base = Instant::now();
        let mut session = session();
        session.on_final("Keep this submit", base, false).unwrap();
        let step = session.on_final("...", ms(base, 1_000), true).unwrap();
        assert_eq!(armed_parts(&step), armed(CaptureRoute::Submit, "Keep this"));
        assert_eq!(
            session.poll_ready(ms(base, 2_500)),
            Some(CaptureRequest {
                route: CaptureRoute::Submit,
                text: "Keep this".to_owned()
            })
        );
    }

    #[test]
    fn dictation_after_an_ending_cancels_it_and_stays_in_the_capture() {
        let base = Instant::now();
        let mut session = session();
        session.on_final("hello submit", base, false).unwrap();
        assert_eq!(
            session.on_final("more words", base, false),
            Ok(FinalStep::Continue {
                cancelled_pending: true
            })
        );
        assert_eq!(session.pending(), None);
        assert_eq!(session.text(), "hello submit more words");
    }

    #[test]
    fn artifact_finals_neither_cancel_nor_restart_an_armed_ending() {
        let base = Instant::now();
        for artifact in ["[noise] uh", "(breathing)", "<|nospeech|>", "um...", "   "] {
            let mut session = session();
            session
                .on_final("hello submit", ms(base, 10_000), false)
                .unwrap();
            assert_eq!(
                session.on_final(artifact, ms(base, 11_000), false),
                Ok(FinalStep::Continue {
                    cancelled_pending: false
                }),
                "{artifact}"
            );
            assert_eq!(session.poll_ready(ms(base, 12_499)), None, "{artifact}");
            assert_eq!(
                session
                    .poll_ready(ms(base, 12_500))
                    .map(|request| request.text),
                Some("hello".to_owned()),
                "{artifact}"
            );
        }
    }

    #[test]
    fn punctuation_alone_is_not_words_and_cannot_cancel_an_armed_ending() {
        let base = Instant::now();
        for noise in [".", "...", " ? ", ",", "!?", "\n"] {
            assert!(!has_words(noise), "{noise:?}");
            let mut session = session();
            session
                .on_final("hello submit", ms(base, 10_000), false)
                .unwrap();
            assert!(!session.on_partial(noise), "{noise:?}");
            assert_eq!(
                session
                    .poll_ready(ms(base, 12_500))
                    .map(|request| request.text),
                Some("hello".to_owned()),
                "{noise:?}"
            );
        }
        assert!(has_words("a."));
    }

    #[test]
    fn an_artifact_final_does_not_revive_a_cancelled_ending() {
        let base = Instant::now();
        let mut session = session();
        session
            .on_final("hello submit", ms(base, 10_000), false)
            .unwrap();
        assert!(session.on_partial("more"));
        session
            .on_final("[noise]", ms(base, 11_000), false)
            .unwrap();
        assert_eq!(session.pending(), None);
        assert_eq!(session.poll_ready(ms(base, 20_000)), None);
        assert_eq!(session.text(), "hello submit");
    }

    #[test]
    fn endings_span_final_boundaries_and_inserted_punctuation() {
        for (finals, expected) in [
            (
                ["Keep this paste.", "that!"],
                Some(armed(CaptureRoute::Paste, "Keep this")),
            ),
            (
                ["Keep this finish", "dictation."],
                Some(armed(CaptureRoute::Paste, "Keep this")),
            ),
            (
                ["Keep this do", "that."],
                Some(armed(CaptureRoute::Task, "Keep this")),
            ),
            (
                ["Keep this", "submit."],
                Some(armed(CaptureRoute::Submit, "Keep this")),
            ),
            (["Keep this never.", "mind"], None),
        ] {
            let mut session = session();
            let step = feed(&mut session, &finals);
            match expected {
                Some(expected) => assert_eq!(armed_parts(&step), expected, "{finals:?}"),
                None => assert!(matches!(step, FinalStep::Cancelled { .. }), "{finals:?}"),
            }
        }
    }

    #[test]
    fn an_artifact_final_does_not_revive_a_cancelled_whole_text_task() {
        let scripts = crate::config::DesktopConfig::from_json_str("{}")
            .unwrap()
            .scripts;
        let mut session = CaptureSession::new(Endings::default(), EndingPauses::default(), scripts);
        let now = Instant::now();
        let armed = session.on_final("open google.com", now, false).unwrap();
        assert!(
            matches!(&armed, FinalStep::Armed(pending) if pending.without_ending),
            "{armed:?}"
        );
        assert!(session.on_partial("and also"));
        assert_eq!(session.pending(), None);
        session.on_final("[noise]", now, false).unwrap();
        assert_eq!(session.pending(), None);
    }

    #[test]
    fn endings_tolerate_up_to_two_trailing_artifacts() {
        for (ending, route) in [
            ("paste that", CaptureRoute::Paste),
            ("submit", CaptureRoute::Submit),
            ("do that", CaptureRoute::Task),
        ] {
            for tail in [
                "...",
                " [noise]",
                " (breathing)",
                " <|nospeech|>",
                " uh",
                ", um... [MUSIC]",
            ] {
                let mut session = session();
                let step = feed(&mut session, &[&format!("Keep this {ending}{tail}")]);
                assert_eq!(
                    armed_parts(&step),
                    armed(route, "Keep this"),
                    "{ending}{tail}"
                );
            }
        }
    }

    #[test]
    fn near_matches_stay_literal_dictation() {
        for text in [
            "taste that.",
            "finish dictation later",
            "Please paste that into the document",
            "submitter",
            "submit um okay",
            "submit um uh hmm",
        ] {
            let mut session = session();
            assert_eq!(
                feed(&mut session, &[text]),
                FinalStep::Continue {
                    cancelled_pending: false
                },
                "{text}"
            );
            assert_eq!(session.text(), text);
        }
    }

    #[test]
    fn configured_endings_match_whole_phrases_longest_first() {
        let custom = endings(&[
            ("  Send   THIS ", Ending::Submit),
            ("so send this", Ending::Paste),
            ("paste", Ending::Paste),
            ("do it", Ending::Task),
            ("scratch", Ending::Scratch),
            ("cancel", Ending::Cancel),
        ])
        .unwrap();
        let mut session = CaptureSession::new(custom, EndingPauses::default(), Vec::new());
        let now = Instant::now();
        for literal in ["Resend this", "A literal send this inside the sentence"] {
            assert_eq!(
                session.on_final(literal, now, false),
                Ok(FinalStep::Continue {
                    cancelled_pending: false
                })
            );
        }
        let step = session
            .on_final("Final words SEND THIS.", now, false)
            .unwrap();
        assert_eq!(
            armed_parts(&step),
            armed(
                CaptureRoute::Submit,
                "Resend this A literal send this inside the sentence Final words"
            )
        );
        let step = session.on_final("ok so send this", now, false).unwrap();
        assert_eq!(armed_parts(&step).0, CaptureRoute::Paste);
    }

    #[test]
    fn scratch_discards_only_the_latest_piece_and_cancel_discards_everything() {
        let mut session = session();
        feed(&mut session, &["Keep the first thought."]);
        feed(&mut session, &["Throw this away scratch that."]);
        assert_eq!(session.text(), "Keep the first thought.");
        assert!(session.scratched());
        feed(&mut session, &["Second thought."]);
        feed(&mut session, &["scratch that"]);
        assert_eq!(session.text(), "Keep the first thought.");
        assert_eq!(
            feed(&mut session, &["never mind"]),
            cancelled("Keep the first thought.")
        );
        assert_eq!(session.text(), "");
    }

    #[test]
    fn scratch_across_finals_keeps_earlier_piece_boundaries() {
        for (finals, expected) in [
            (vec!["scratch that"], "First"),
            (vec!["scratch.", "that"], "First"),
            (vec!["Discard this scratch", "that"], "First Second"),
            (vec!["Discard this scratch that"], "First Second"),
        ] {
            let mut session = session();
            feed(&mut session, &["First", "Second"]);
            assert_eq!(
                feed(&mut session, &finals),
                FinalStep::Continue {
                    cancelled_pending: false
                },
                "{finals:?}"
            );
            assert_eq!(session.text(), expected, "{finals:?}");
        }
    }

    #[test]
    fn scratch_after_an_armed_ending_removes_its_piece_and_cancels_it() {
        let mut session = session();
        feed(&mut session, &["first piece"]);
        feed(&mut session, &["second submit"]);
        assert!(session.on_partial("more"));
        feed(&mut session, &["scratch that"]);
        assert_eq!(session.text(), "first piece");

        feed(&mut session, &["third submit"]);
        assert_eq!(
            feed(&mut session, &["scratch that"]),
            FinalStep::Continue {
                cancelled_pending: true
            }
        );
        assert_eq!(session.pending(), None);
        assert_eq!(session.text(), "first piece");
    }

    #[test]
    fn a_session_without_endings_keeps_every_ending_phrase_as_text() {
        for phrase in [
            "paste that",
            "finish dictation",
            "submit",
            "do that",
            "scratch that",
            "never mind",
        ] {
            let mut session = CaptureSession::without_endings();
            let said = format!("fix the bug {phrase}");
            assert_eq!(
                session.on_final(&said, Instant::now(), false),
                Ok(FinalStep::Continue {
                    cancelled_pending: false
                }),
                "{phrase}"
            );
            assert_eq!(session.text(), said, "{phrase}");
            assert_eq!(session.pending(), None, "{phrase}");
            assert!(!session.scratched(), "{phrase}");
            assert_eq!(
                session.poll_ready(Instant::now() + Duration::from_secs(100)),
                None,
                "{phrase}"
            );
        }
    }

    #[test]
    fn never_mind_discards_the_capture_and_any_armed_ending() {
        for spoken in ["never mind", "Never mind."] {
            let mut session = session();
            feed(&mut session, &["Draft words", "hello submit"]);
            assert_eq!(
                feed(&mut session, &[spoken]),
                cancelled("Draft words hello submit"),
                "{spoken}"
            );
            assert_eq!(session.pending(), None);
            assert_eq!(session.text(), "");
        }
    }

    #[test]
    fn a_capture_accepts_exactly_the_limit_and_refuses_one_character_more() {
        let x = |count: usize| "x".repeat(count);
        let cases: Vec<(usize, String, bool)> = vec![
            (19_990, x(9), false),
            (19_990, x(10), true),
            (0, "é".repeat(20_000), false),
            (0, "é".repeat(20_001), true),
            // The ending phrase does not count toward the limit.
            (19_990, "paste that".to_owned(), false),
            (19_990, "y".repeat(9) + " paste that", false),
            (19_990, "y".repeat(10) + " paste that", true),
        ];
        for (existing, next, refused) in cases {
            let mut session = session();
            if existing > 0 {
                feed(&mut session, &[&x(existing)]);
            }
            let result = session.on_final(&next, Instant::now(), false);
            if refused {
                assert_eq!(result, Err(CaptureFullError), "{existing} + {}", next.len());
                assert_eq!(session.text().chars().count(), existing);
                assert_eq!(session.pending(), None);
            } else {
                assert!(result.is_ok(), "{existing} + {}", next.len());
            }
        }
    }

    #[test]
    fn a_full_capture_can_still_deliver_what_it_holds() {
        let mut session = session();
        feed(&mut session, &[&"x".repeat(19_990)]);
        assert_eq!(
            session.on_final(&"y".repeat(20), Instant::now(), false),
            Err(CaptureFullError)
        );
        let step = feed(&mut session, &["paste that"]);
        assert_eq!(
            armed_parts(&step),
            armed(CaptureRoute::Paste, &"x".repeat(19_990))
        );
    }

    #[test]
    fn endings_reject_empty_duplicate_and_incomplete_sets() {
        let complete = [
            ("paste", Ending::Paste),
            ("send", Ending::Submit),
            ("do it", Ending::Task),
            ("scratch", Ending::Scratch),
            ("cancel", Ending::Cancel),
        ];
        let with = |extra: (&'static str, Ending)| {
            let mut entries = complete.to_vec();
            entries.push(extra);
            endings(&entries)
        };
        assert!(endings(&complete).is_ok());
        assert_eq!(with(("   ", Ending::Paste)), Err(EndingsError::Empty));
        assert_eq!(with(("Do  IT", Ending::Task)), Err(EndingsError::Duplicate));
        assert_eq!(endings(&complete[..4]), Err(EndingsError::Incomplete));
        assert_eq!(Endings::default(), endings(&DEFAULT_ENDINGS).unwrap());
    }

    #[test]
    fn each_ending_has_its_own_pause_from_zero_to_five_seconds() {
        let ms = |paste, submit, task| Ok([paste, submit, task].map(Duration::from_millis));
        for (json, expected) in [
            ("{}", ms(700, 2500, 2500)),
            (
                r#"{"paste": 0, "submit": 5, "task": 1.5}"#,
                ms(0, 5000, 1500),
            ),
            (r#"{"submit": 0.7}"#, ms(700, 700, 2500)),
            (
                r#"{"paste": -0.001}"#,
                Err("The paste pause must be a number of seconds from 0 to 5."),
            ),
            (r#"{"submit": 5.001}"#, Err("submit pause must be")),
            (r#"{"task": 6}"#, Err("task pause must be")),
            (r#"{"paste": "0.7"}"#, Err("invalid type")),
            (r#"{"task": null, "paste": true}"#, Err("invalid type")),
            ("2.5", Err("invalid type")),
            (r#"{"scratch": 1}"#, Err("unknown field `scratch`")),
        ] {
            let result = serde_json::from_str::<EndingPauses>(json);
            match (result, expected) {
                (Ok(pauses), Ok(expected)) => assert_eq!(
                    [
                        CaptureRoute::Paste,
                        CaptureRoute::Submit,
                        CaptureRoute::Task
                    ]
                    .map(|route| pauses.get(route)),
                    expected,
                    "{json}"
                ),
                (Err(error), Err(expected)) => {
                    assert!(error.to_string().contains(expected), "{json}: {error}");
                }
                (result, expected) => panic!("{json}: {result:?} vs {expected:?}"),
            }
        }
    }

    #[test]
    fn the_capture_limits_have_their_own_ranges() {
        let secs = |idle, max| Ok([idle, max].map(Duration::from_secs));
        for (json, expected) in [
            ("{}", secs(60, 300)),
            (r#"{"idle": 10, "max": 3600}"#, secs(10, 3600)),
            (r#"{"idle": 600, "max": 60}"#, secs(600, 60)),
            (r#"{"idle": 90}"#, secs(90, 300)),
            (r#"{"max": 900}"#, secs(60, 900)),
            (
                r#"{"idle": 9}"#,
                Err("The idle limit must be a whole number of seconds from 10 to 600."),
            ),
            (r#"{"idle": 601}"#, Err("idle limit must be")),
            (
                r#"{"max": 59}"#,
                Err("The max limit must be a whole number of seconds from 60 to 3600."),
            ),
            (r#"{"max": 3601}"#, Err("max limit must be")),
            (r#"{"idle": "60"}"#, Err("invalid type")),
            (r#"{"max": 60.5}"#, Err("invalid type")),
            (r#"{"idle": -1}"#, Err("invalid value")),
            ("60", Err("invalid type")),
            (r#"{"min": 1}"#, Err("unknown field `min`")),
        ] {
            let result = serde_json::from_str::<CaptureLimits>(json);
            match (result, expected) {
                (Ok(limits), Ok(expected)) => {
                    assert_eq!([limits.idle(), limits.max()], expected, "{json}");
                }
                (Err(error), Err(expected)) => {
                    assert!(error.to_string().contains(expected), "{json}: {error}");
                }
                (result, expected) => panic!("{json}: {result:?} vs {expected:?}"),
            }
        }
    }
}

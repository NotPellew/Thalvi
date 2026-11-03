//! Condenses per-frame wake scores into near misses and a per-minute summary of latency and sound.

use std::time::Duration;

use tokio::time::Instant;

use crate::wake::WakeFrame;

/// The score from which a head counts as nearly waking: in 13.7 h of one person's listening it
/// gave 3 near misses (about 5 a day), where 0.1 gave 6 and 0.05 gave 7.
pub const NEAR_MISS_LEVEL: f32 = 0.15;

/// How long a summary covers.
const SUMMARY_WINDOW: Duration = Duration::from_secs(60);

/// A head's score rose to `peak`, stayed below its threshold and fell back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NearMiss {
    pub head: &'static str,
    pub peak: f32,
    pub threshold: f32,
}

struct Head {
    name: &'static str,
    threshold: f32,
    /// The highest score of the episode in progress.
    peak: Option<f32>,
}

impl Head {
    fn observe(&mut self, score: f32) -> Option<NearMiss> {
        if score >= self.threshold {
            self.peak = None;
        } else if score >= NEAR_MISS_LEVEL {
            self.peak = Some(self.peak.map_or(score, |peak| peak.max(score)));
        } else if let Some(peak) = self.peak.take() {
            return Some(NearMiss {
                head: self.name,
                peak,
                threshold: self.threshold,
            });
        }
        None
    }
}

/// Follows both heads' scores frame by frame.
pub struct NearMisses {
    capture: Head,
    conversation: Head,
}

impl NearMisses {
    #[must_use]
    pub fn new(capture_threshold: f32, conversation_threshold: f32) -> Self {
        let head = |name, threshold| Head {
            name,
            threshold,
            peak: None,
        };
        Self {
            capture: head("capture", capture_threshold),
            conversation: head("conversation", conversation_threshold),
        }
    }

    /// The episodes that this frame ended.
    pub fn observe(&mut self, wake: &WakeFrame) -> impl Iterator<Item = NearMiss> {
        [
            self.capture.observe(wake.capture),
            self.conversation.observe(wake.conversation),
        ]
        .into_iter()
        .flatten()
    }
}

/// How long scoring a frame took over a window, and how much of it the microphone heard sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencySummary {
    pub count: usize,
    pub p50: Duration,
    pub p95: Duration,
    pub max: Duration,
    pub sound: Duration,
    pub quiet: Duration,
    /// Changes between sound and quiet.
    pub switches: usize,
}

/// Collects scoring times and sound levels and summarizes them once a minute has passed.
#[derive(Default)]
pub struct Latency {
    since: Option<Instant>,
    took: Vec<Duration>,
    sound: Duration,
    quiet: Duration,
    switches: usize,
    /// Whether the last level span had sound; kept across summaries so a switch is counted once.
    heard_sound: Option<bool>,
}

impl Latency {
    /// Adds one frame's time; the summary comes back when this frame ends the window.
    pub fn record(&mut self, now: Instant, took: Duration) -> Option<LatencySummary> {
        let since = *self.since.get_or_insert(now);
        self.took.push(took);
        if now.duration_since(since) >= SUMMARY_WINDOW {
            return self.finish();
        }
        None
    }

    /// Adds a stretch of microphone audio of length `span` that had `sound` or was quiet.
    pub fn level(&mut self, sound: bool, span: Duration) {
        if self.heard_sound.is_some_and(|before| before != sound) {
            self.switches += 1;
        }
        self.heard_sound = Some(sound);
        if sound {
            self.sound += span;
        } else {
            self.quiet += span;
        }
    }

    /// Summarizes what was collected so far and starts over; `None` when no scoring time was.
    pub fn finish(&mut self) -> Option<LatencySummary> {
        self.since = None;
        let sound = std::mem::take(&mut self.sound);
        let quiet = std::mem::take(&mut self.quiet);
        let switches = std::mem::take(&mut self.switches);
        let mut took = std::mem::take(&mut self.took);
        took.sort_unstable();
        let max = *took.last()?;
        let rank = |percent: usize| took[(took.len() * percent).div_ceil(100) - 1];
        Some(LatencySummary {
            count: took.len(),
            p50: rank(50),
            p95: rank(95),
            max,
            sound,
            quiet,
            switches,
        })
    }
}

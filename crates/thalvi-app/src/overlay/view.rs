//! What the overlay shows: the engine's status, the mic level and the settings turned into the
//! tone, label, line and motion every look draws.

use std::time::Duration;

use thalvi_desktop::capture::CaptureRoute;

use crate::status::{FailureKind, Status};

/// The overlay surface, in points; every look draws inside it.
pub const WIDTH: f32 = 460.0;
pub const HEIGHT: f32 = 200.0;
/// The module looks' wave, in points.
pub(super) const WAVE_W: f32 = 332.0;
pub(super) const WAVE_H: f32 = 44.0;
/// Longest target the countdown label spells out.
const MAX_TARGET: usize = 10;

const HISTORY: usize = 40;
const SAMPLE_S: f64 = 0.08;
pub(super) const FRAME: Duration = Duration::from_millis(33);
const SETTLED_FRAME: Duration = Duration::from_millis(100);
const SETTLED_BELOW: f32 = 0.015;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Live,
    Quiet,
    Pending,
    Working,
    Done,
    Failed,
}

/// What the one line of text shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum Line {
    #[default]
    None,
    Words {
        committed: String,
        partial: String,
        caret: bool,
    },
    Notice(String),
}

/// A running countdown to an ending's action.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Countdown {
    pub(super) route: CaptureRoute,
    pub(super) target: Option<String>,
    pub(super) copies: bool,
    pub(super) total_ms: u64,
    /// When it started, on the overlay's clock.
    pub(super) started: f64,
}

/// The mic wave's recent loudness and motion, stepped by the overlay's poll.
#[derive(Clone, Debug)]
pub(super) struct Wave {
    pub(super) history: [f32; HISTORY],
    pub(super) smoothed: f32,
    pub(super) phase: f32,
    pub(super) last_frame: Option<f64>,
    pub(super) last_sample: Option<f64>,
}

impl Default for Wave {
    fn default() -> Self {
        Self {
            history: [0.0; HISTORY],
            smoothed: 0.0,
            phase: 0.0,
            last_frame: None,
            last_sample: None,
        }
    }
}

impl Wave {
    fn settled(&self, level: f32) -> bool {
        self.smoothed < SETTLED_BELOW
            && level < SETTLED_BELOW
            && self.history.iter().all(|sample| *sample < SETTLED_BELOW)
    }

    /// Advances to `now` (seconds); the time to wait for the next frame.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "small positive counts"
    )]
    fn step(&mut self, now: f64, level: f32) -> Duration {
        let elapsed = self
            .last_frame
            .map_or(0.0, |last| (now - last).clamp(0.0, 0.05)) as f32;
        self.last_frame = Some(now);
        let due = self
            .last_sample
            .map_or(HISTORY, |last| ((now - last) / SAMPLE_S).floor() as usize);
        if due > 0 {
            let fresh = due.min(HISTORY);
            self.history.rotate_left(fresh);
            for sample in &mut self.history[HISTORY - fresh..] {
                *sample = level;
            }
            self.last_sample = Some(match self.last_sample {
                Some(last) if due <= HISTORY => last + due as f64 * SAMPLE_S,
                _ => now,
            });
        }
        let pace = |rate: f32| 1.0 - (1.0 - rate).powf(elapsed * 60.0);
        let rate = if level > self.smoothed { 0.35 } else { 0.08 };
        self.smoothed += (level - self.smoothed) * pace(rate);
        self.phase += elapsed * (1.1 + 3.2 * self.smoothed);
        if self.settled(level) {
            SETTLED_FRAME
        } else {
            FRAME
        }
    }

    pub(super) fn amplitude(&self, x: f32, seconds: f32) -> f32 {
        let distance = (x - 0.5).abs() * 2.0;
        #[expect(clippy::cast_precision_loss, reason = "HISTORY is 40")]
        let at = (HISTORY - 1) as f32 - distance * 14.0;
        let index = at.floor();
        let share = at - index;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "index is within 25..39"
        )]
        let index = index as usize;
        let sample = |i: usize| self.history.get(i).copied().unwrap_or(0.0);
        let recent = sample(index) * (1.0 - share) + sample(index + 1) * share;
        let breathing = 0.035 + 0.012 * (seconds * 2.2).sin();
        let envelope = (-((x - 0.5) / 0.27).powi(2)).exp();
        WAVE_H
            * 0.47
            * envelope
            * (breathing + 0.95 * (self.smoothed * (1.0 - distance * 0.6)).max(recent))
    }
}
/// What the overlay shows, fed from the engine's status, its level and the settings.
pub struct View {
    pub(super) status: Status,
    pub(super) tone: Tone,
    pub(super) label: Option<String>,
    pub(super) line: Line,
    pub(super) show_draft: bool,
    pub(super) takes_clicks: bool,
    pub(super) level: f32,
    pub(super) wave: Option<Wave>,
    pub(super) countdown: Option<Countdown>,
    pub(super) sweep: bool,
    pub(super) clock: f64,
}

impl View {
    pub fn new(takes_clicks: bool) -> Self {
        Self {
            status: Status::Paused,
            tone: Tone::Live,
            label: None,
            line: Line::None,
            show_draft: true,
            takes_clicks,
            level: 0.0,
            wave: None,
            countdown: None,
            sweep: false,
            clock: 0.0,
        }
    }

    pub fn set_draft(&mut self, show: bool) {
        self.show_draft = show;
    }

    /// The tone, label and line `next` shows; whether the wave runs, and the countdown if one does.
    fn take(&mut self, next: &Status, was_counting: bool, now: f64) -> (bool, Option<Countdown>) {
        let mut wave = false;
        let mut countdown = None;
        match next {
            Status::Paused | Status::Listening => {
                self.line = Line::None;
                self.tone = Tone::Live;
            }
            Status::Starting { .. } => {
                self.line = Line::None;
                self.tone = Tone::Live;
                wave = true;
            }
            Status::Capturing {
                committed,
                partial,
                notice,
                ..
            } => {
                self.line = match notice {
                    Some(notice) => Line::Notice(notice.clone()),
                    None if self.show_draft => Line::Words {
                        committed: committed.clone(),
                        partial: partial.clone().unwrap_or_default(),
                        caret: true,
                    },
                    None => Line::None,
                };
                self.tone = Tone::Live;
                wave = true;
            }
            Status::Pending {
                target,
                route,
                text: words,
                waiting_for_quiet,
                pause_ms,
                copies,
            } => {
                self.line = self.words(words, false);
                if *waiting_for_quiet {
                    self.tone = Tone::Quiet;
                    wave = true;
                } else {
                    self.tone = Tone::Pending;
                    let started = match &self.countdown {
                        Some(running) if was_counting => running.started,
                        _ => now,
                    };
                    countdown = Some(Countdown {
                        route: *route,
                        target: target.clone(),
                        copies: *copies,
                        total_ms: *pause_ms,
                        started,
                    });
                }
            }
            Status::Writing { text: words, .. } => {
                self.line = self.words(words, true);
                self.tone = Tone::Working;
                self.label = Some("WRITING…".to_owned());
                self.sweep = true;
            }
            Status::Thinking { .. } => {
                self.line = Line::None;
                self.tone = Tone::Working;
                self.label = Some("THINKING…".to_owned());
                self.sweep = true;
            }
            Status::Working { .. } => {
                self.tone = Tone::Working;
                self.label = Some("WORKING".to_owned());
                self.sweep = true;
            }
            Status::Loading { notice, .. } => {
                self.line = Line::Notice(notice.clone());
                self.tone = Tone::Working;
                self.label = Some("LOADING".to_owned());
                self.sweep = true;
            }
            Status::Done { .. } => {
                // The confirmation takes the line's place, so its caret stops blinking.
                self.line = Line::None;
                self.tone = Tone::Done;
            }
            Status::Failed { kind, headline, .. } => {
                self.line = Line::Notice((*headline).to_owned());
                self.tone = Tone::Failed;
                self.label = Some(
                    match kind {
                        FailureKind::Delivery => "NOT SENT",
                        FailureKind::Other => "FAILED",
                    }
                    .to_owned(),
                );
            }
        }
        (wave, countdown)
    }

    /// Takes in a new status at `now` on the overlay's clock.
    pub fn set_status(&mut self, next: Status, now: f64) {
        let was_counting = matches!(
            self.status,
            Status::Pending {
                waiting_for_quiet: false,
                ..
            }
        );
        self.clock = now;
        self.label = None;
        self.sweep = false;
        let (wave, countdown) = self.take(&next, was_counting, now);
        self.wave = match (wave, self.wave.take()) {
            (true, Some(running)) => Some(running),
            (true, None) => Some(Wave::default()),
            (false, _) => None,
        };
        if !wave {
            self.level = 0.0;
        }
        self.countdown = countdown;
        self.status = next;
        if let Some(running) = &self.countdown {
            self.label = Some(countdown_label(running, self.left_ms(running)));
        }
    }

    fn words(&self, words: &str, caret: bool) -> Line {
        if self.show_draft {
            Line::Words {
                committed: words.to_owned(),
                partial: String::new(),
                caret,
            }
        } else {
            Line::None
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "milliseconds of a pause"
    )]
    pub(super) fn left_ms(&self, countdown: &Countdown) -> u64 {
        let elapsed = ((self.clock - countdown.started) * 1000.0).max(0.0) as u64;
        countdown
            .total_ms
            .saturating_sub(elapsed)
            .min(countdown.total_ms)
    }

    pub fn cancellable(&self) -> bool {
        self.takes_clicks
            && matches!(
                self.status,
                Status::Starting { .. }
                    | Status::Capturing { .. }
                    | Status::Pending { .. }
                    | Status::Writing { .. }
                    | Status::Thinking { .. }
                    | Status::Working {
                        cancellable: true,
                        ..
                    }
            )
    }

    pub fn animating(&self) -> bool {
        self.wave.is_some() || self.countdown.is_some() || self.sweep || self.caret()
    }

    fn caret(&self) -> bool {
        matches!(self.line, Line::Words { caret: true, .. })
    }
}

impl View {
    /// Steps the wave, the countdown and the sweep to `clock` with the mic at `level`; how long
    /// until the next frame, or `None` while nothing moves.
    pub fn poll(&mut self, clock: f64, level: f32) -> Option<Duration> {
        if !self.animating() {
            return None;
        }
        self.clock = clock;
        let level = if self.wave.is_some() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.level = level;
        let next = self
            .wave
            .as_mut()
            .map_or(FRAME, |wave| wave.step(clock, level));
        if let Some(running) = self.countdown.clone() {
            self.label = Some(countdown_label(&running, self.left_ms(&running)));
        }
        Some(next)
    }
}

pub(super) fn copies_text(route: CaptureRoute, copies: bool) -> bool {
    copies && route != CaptureRoute::Task
}

/// The action, its target, and whole seconds rounded up.
pub(super) fn countdown_label(countdown: &Countdown, left_ms: u64) -> String {
    let seconds = left_ms.div_ceil(1000).max(1);
    let to = countdown.target.as_deref().map(|target| {
        let named = target.to_uppercase();
        if named.chars().count() > MAX_TARGET {
            format!(
                "{}…",
                named.chars().take(MAX_TARGET - 1).collect::<String>()
            )
        } else {
            named
        }
    });
    let action = if copies_text(countdown.route, countdown.copies) {
        "COPY".to_owned()
    } else {
        match (countdown.route, to) {
            (CaptureRoute::Paste, None) => "PASTE".to_owned(),
            (CaptureRoute::Paste, Some(to)) => format!("PASTE INTO {to}"),
            (CaptureRoute::Write, None) => "WRITE".to_owned(),
            (CaptureRoute::Write, Some(to)) => format!("WRITE FOR {to}"),
            (CaptureRoute::Submit, None) => "SEND".to_owned(),
            (CaptureRoute::Submit, Some(to)) => format!("SEND TO {to}"),
            (CaptureRoute::Task, _) => "TASK".to_owned(),
        }
    };
    format!("{action} IN {seconds} s")
}

/// What the slot says once a delivery finished.
pub(super) fn done_message(route: CaptureRoute, target: Option<&str>, notice: &str) -> String {
    match target {
        Some(target) if route != CaptureRoute::Task => {
            let verb = if matches!(route, CaptureRoute::Paste | CaptureRoute::Write) {
                "Pasted into"
            } else {
                "Sent to"
            };
            format!("✓ {verb} {target}. {notice}")
        }
        _ => format!("✓ {notice}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counting(route: CaptureRoute, target: Option<&str>, copies: bool) -> Countdown {
        Countdown {
            route,
            target: target.map(str::to_owned),
            copies,
            total_ms: 2500,
            started: 0.0,
        }
    }

    #[test]
    fn the_countdown_names_the_action_and_its_target_in_whole_seconds_rounded_up() {
        let cases = [
            (
                CaptureRoute::Paste,
                Some("kitty"),
                false,
                2500,
                "PASTE INTO KITTY IN 3 s",
            ),
            (
                CaptureRoute::Submit,
                Some("a-very-long-window"),
                false,
                1000,
                "SEND TO A-VERY-LO… IN 1 s",
            ),
            (CaptureRoute::Write, None, false, 1, "WRITE IN 1 s"),
            (CaptureRoute::Paste, Some("kitty"), true, 0, "COPY IN 1 s"),
            (CaptureRoute::Task, Some("kitty"), true, 2000, "TASK IN 2 s"),
        ];
        for (route, target, copies, left, expected) in cases {
            assert_eq!(
                countdown_label(&counting(route, target, copies), left),
                expected
            );
        }
    }

    #[test]
    fn a_countdown_keeps_its_start_while_the_pending_status_updates() {
        let pending = |quiet: bool| Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: "hello".to_owned(),
            waiting_for_quiet: quiet,
            pause_ms: 2500,
            copies: false,
        };
        let mut view = View::new(false);
        view.set_status(pending(false), 1.0);
        view.set_status(pending(false), 2.0);
        assert_eq!(view.label.as_deref(), Some("PASTE IN 2 s"));
        view.set_status(pending(true), 3.0);
        view.set_status(pending(false), 4.0);
        assert_eq!(view.label.as_deref(), Some("PASTE IN 3 s"));
    }

    #[test]
    fn a_delivery_names_where_the_words_went() {
        assert_eq!(
            done_message(CaptureRoute::Write, Some("kitty"), "Done."),
            "✓ Pasted into kitty. Done."
        );
        assert_eq!(
            done_message(CaptureRoute::Submit, Some("kitty"), "Done."),
            "✓ Sent to kitty. Done."
        );
        assert_eq!(
            done_message(CaptureRoute::Task, Some("kitty"), "Opened."),
            "✓ Opened."
        );
    }
    #[test]
    fn the_wave_shows_new_sound_at_its_centre_moves_older_sound_outwards_and_stays_in_its_strip() {
        let heard = |age: usize| {
            let mut wave = Wave::default();
            wave.history[HISTORY - 1 - age] = 1.0;
            wave
        };
        #[expect(clippy::cast_precision_loss, reason = "small counts")]
        let out = |age: usize| age as f32 / 28.0;
        let newest = heard(0);
        assert!(newest.amplitude(0.5, 0.0) > 2.0 * newest.amplitude(0.5 + out(4), 0.0));
        let older = heard(4);
        for side in [0.5 - out(4), 0.5 + out(4)] {
            assert!(
                older.amplitude(side, 0.0) > 2.0 * older.amplitude(0.5, 0.0),
                "{side}"
            );
            assert!(
                older.amplitude(side, 0.0) > 2.0 * older.amplitude(0.5 + out(8), 0.0),
                "{side}"
            );
        }
        assert!(
            (older.amplitude(0.5 - out(4), 0.0) - older.amplitude(0.5 + out(4), 0.0)).abs() < 1e-4
        );
        let between = older.amplitude(0.5 + out(3) + out(1) / 2.0, 0.0);
        assert!(
            between > older.amplitude(0.5 + out(3), 0.0)
                && between < older.amplitude(0.5 + out(4), 0.0)
        );
        let loud = Wave {
            history: [1.0; HISTORY],
            smoothed: 1.0,
            ..Wave::default()
        };
        let peak = loud.amplitude(0.5, 0.0);
        assert!(peak > 0.4 * WAVE_H && peak <= WAVE_H / 2.0, "{peak}");
        let quiet = Wave::default();
        assert!(quiet.amplitude(0.5, 0.0) > 0.0 && quiet.amplitude(0.5, 0.0) < 0.05 * WAVE_H);
    }
}

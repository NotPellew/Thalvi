//! Owns what the ending spotter's hits mean for a capture: whether the transcript carried the ending, and the quiet gate a dispute passes before the user is told.
//!
//! Sync and clock-free: the caller passes the instant, the count of samples sent to the
//! recognizer, and whether the user is speaking. A hit never acts on the capture; at most it
//! becomes a notice.

use std::time::Duration;

use tokio::time::Instant;

use crate::capture::{
    CaptureFullError, CaptureRoute, CaptureSession, Ending, EndingPauses, FinalStep, QuietWindow,
};
use crate::frame::{FRAME_SAMPLES, SAMPLE_RATE};
use crate::spotter::SpotterHit;

/// How long after the end of a heard phrase the covering final may take before the transcript is
/// taken to have none. Measured on the local recognizer, whose final comes up to 2 s after the
/// words end; a dispute is otherwise judged by the final itself, never by the clock.
pub const NO_FINAL_AFTER: Duration = Duration::from_millis(2_800);

const SAMPLES_PER_SECOND: u64 = SAMPLE_RATE as u64;

fn samples(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis() * u128::from(SAMPLE_RATE) / 1000).unwrap_or(u64::MAX)
}

/// What one final did to the capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Empty or only artifacts.
    Nothing,
    /// Plain words were added.
    Text,
    /// The final carried this ending and the capture acted on it.
    Ending(Ending),
}

impl Effect {
    /// What `step` did to `before`, leaving `after`.
    #[must_use]
    pub fn of(
        before: &CaptureSession,
        after: &CaptureSession,
        step: &Result<FinalStep, CaptureFullError>,
    ) -> Self {
        match step {
            Err(_) => Self::Nothing,
            Ok(FinalStep::Armed(pending)) => Self::Ending(match pending.route {
                CaptureRoute::Paste => Ending::Paste,
                CaptureRoute::Submit => Ending::Submit,
                CaptureRoute::Task => Ending::Task,
            }),
            Ok(FinalStep::Cancelled { .. }) => Self::Ending(Ending::Cancel),
            Ok(FinalStep::Continue { .. }) => {
                let (was, now) = (before.text().len(), after.text().len());
                if after.scratched() && (!before.scratched() || now < was) {
                    Self::Ending(Ending::Scratch)
                } else if now > was {
                    Self::Text
                } else {
                    Self::Nothing
                }
            }
        }
    }
}

/// How a heard ending ended up, for the metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The covering final carried the same ending.
    Agreed,
    /// The covering final carried another ending; the transcript wins.
    Conflict,
    /// The transcript lacked the ending, the user stayed quiet, and was told.
    Notified,
    /// The transcript lacked the ending, then the user spoke on: nothing was said.
    Continued,
    /// A final carried the ending after the transcript was thought to lack it.
    AgreedLate,
    /// The capture ended with the hit unresolved.
    Ended,
}

impl Outcome {
    /// The name in metrics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Agreed => "agreed",
            Self::Conflict => "conflict",
            Self::Notified => "notified",
            Self::Continued => "continued",
            Self::AgreedLate => "agreed_late",
            Self::Ended => "ended",
        }
    }
}

/// A heard ending that has been settled.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    pub outcome: Outcome,
    pub phrase: String,
    pub ending: Ending,
    pub confidence: f32,
    /// From the hit until now.
    pub since_hit: Duration,
    /// From the end of the phrase to the final that covered it, when one did.
    pub covering_after: Option<Duration>,
}

#[derive(Clone, Debug)]
struct Heard {
    phrase: String,
    ending: Ending,
    confidence: f32,
    /// The end of the phrase, in samples sent to the recognizer.
    end: u64,
    at: Instant,
    /// How long after the phrase's end the covering final arrived.
    covering_after: Option<Duration>,
}

impl Heard {
    fn resolve(&self, outcome: Outcome, now: Instant) -> Resolution {
        Resolution {
            outcome,
            phrase: self.phrase.clone(),
            ending: self.ending,
            confidence: self.confidence,
            since_hit: now.saturating_duration_since(self.at),
            covering_after: self.covering_after,
        }
    }
}

#[derive(Clone, Debug)]
struct Dispute {
    heard: Heard,
    window: QuietWindow,
    pause: Duration,
}

#[derive(Clone, Debug)]
enum Spotting {
    Listening,
    /// Heard; the final that covers the phrase has not decided it yet.
    Claimed(Heard),
    /// The transcript lacks it; the user is told once the quiet gate passes.
    Disputed(Dispute),
}

/// The notice to give: the transcript did not take a heard ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub phrase: String,
    pub ending: Ending,
}

/// The ending spotter's hits for one capture, judged against the transcript.
pub struct Watch {
    pauses: EndingPauses,
    spotting: Spotting,
    /// Samples sent to the recognizer so far.
    sent: u64,
    /// When each final that changed the capture arrived, in samples sent.
    finals: Vec<(u64, Effect)>,
}

impl Watch {
    #[must_use]
    pub fn new(pauses: EndingPauses) -> Self {
        Self {
            pauses,
            spotting: Spotting::Listening,
            sent: 0,
            finals: Vec::new(),
        }
    }

    /// The quiet interval a dispute of `ending` waits for: the ending's own pause, and a
    /// paste's for scratch and cancel, which act at once and have none.
    fn pause(&self, ending: Ending) -> Duration {
        self.pauses.get(match ending {
            Ending::Paste | Ending::Scratch | Ending::Cancel => CaptureRoute::Paste,
            Ending::Submit => CaptureRoute::Submit,
            Ending::Task => CaptureRoute::Task,
        })
    }

    fn window(speech: bool, now: Instant) -> QuietWindow {
        if speech {
            QuietWindow::WaitingForQuiet
        } else {
            QuietWindow::CountingDown { since: now }
        }
    }

    /// Whether a dispute is counting its quiet interval or waiting for it to start.
    #[must_use]
    pub fn disputed(&self) -> bool {
        matches!(self.spotting, Spotting::Disputed(_))
    }

    /// Counts `frames` more frames sent to the recognizer; the transcript is taken to have no
    /// final for a heard phrase once [`NO_FINAL_AFTER`] has passed since its end.
    pub fn sent(&mut self, frames: usize, now: Instant, speech: bool) {
        self.sent += (frames * FRAME_SAMPLES) as u64;
        let Spotting::Claimed(heard) = &self.spotting else {
            return;
        };
        if self.sent >= heard.end + samples(NO_FINAL_AFTER) {
            self.dispute(heard.clone(), now, speech);
        }
    }

    fn dispute(&mut self, heard: Heard, now: Instant, speech: bool) {
        let pause = self.pause(heard.ending);
        self.spotting = Spotting::Disputed(Dispute {
            heard,
            window: Self::window(speech, now),
            pause,
        });
    }

    /// The spotter heard a phrase just now. A hit while another waits replaces it: the latest
    /// ending and end win.
    pub fn hit(&mut self, hit: &SpotterHit, now: Instant, speech: bool) -> Option<Resolution> {
        let heard = Heard {
            phrase: hit.phrase.clone(),
            ending: hit.ending,
            confidence: hit.confidence,
            end: hit.end_sample,
            at: now,
            covering_after: None,
        };
        self.spotting = Spotting::Claimed(heard);
        // A final that covers the phrase may have come before the slower spotter.
        let covering = self
            .finals
            .iter()
            .copied()
            .find(|(arrived, _)| *arrived >= hit.end_sample);
        covering.and_then(|(arrived, effect)| self.judge(arrived, effect, now, speech))
    }

    /// A final arrived and did `effect` to the capture.
    pub fn final_arrived(
        &mut self,
        effect: Effect,
        now: Instant,
        speech: bool,
    ) -> Option<Resolution> {
        if effect != Effect::Nothing {
            self.finals.push((self.sent, effect));
        }
        match &self.spotting {
            Spotting::Listening => None,
            Spotting::Claimed(_) => self.judge(self.sent, effect, now, speech),
            Spotting::Disputed(dispute) => {
                let heard = dispute.heard.clone();
                let outcome = match effect {
                    Effect::Nothing => return None,
                    Effect::Text => Outcome::Continued,
                    Effect::Ending(ending) if ending == heard.ending => Outcome::AgreedLate,
                    Effect::Ending(_) => Outcome::Conflict,
                };
                self.spotting = Spotting::Listening;
                Some(heard.resolve(outcome, now))
            }
        }
    }

    /// Judges the claim by a final that arrived after `arrived` samples had been sent.
    fn judge(
        &mut self,
        arrived: u64,
        effect: Effect,
        now: Instant,
        speech: bool,
    ) -> Option<Resolution> {
        let Spotting::Claimed(heard) = &self.spotting else {
            return None;
        };
        if arrived < heard.end || effect == Effect::Nothing {
            return None;
        }
        let mut heard = heard.clone();
        let late = arrived - heard.end;
        heard.covering_after = Some(Duration::from_micros(late * 1_000_000 / SAMPLES_PER_SECOND));
        match effect {
            Effect::Ending(ending) => {
                self.spotting = Spotting::Listening;
                let outcome = if ending == heard.ending {
                    Outcome::Agreed
                } else {
                    Outcome::Conflict
                };
                Some(heard.resolve(outcome, now))
            }
            Effect::Text => {
                self.dispute(heard, now, speech);
                None
            }
            Effect::Nothing => None,
        }
    }

    /// Words arrived in a partial: the user is speaking on, which dismisses a dispute.
    pub fn words(&mut self, now: Instant) -> Option<Resolution> {
        let Spotting::Disputed(dispute) = &self.spotting else {
            return None;
        };
        let resolution = dispute.heard.resolve(Outcome::Continued, now);
        self.spotting = Spotting::Listening;
        Some(resolution)
    }

    /// A quiet frame: starts the dispute's countdown if it was waiting for quiet.
    pub fn quiet_frame(&mut self, now: Instant) {
        if let Spotting::Disputed(Dispute {
            window: window @ QuietWindow::WaitingForQuiet,
            ..
        }) = &mut self.spotting
        {
            *window = QuietWindow::CountingDown { since: now };
        }
    }

    /// The user is heard speaking: a running countdown restarts at the next quiet frame.
    pub fn speaking(&mut self) {
        if let Spotting::Disputed(Dispute {
            window: window @ QuietWindow::CountingDown { .. },
            ..
        }) = &mut self.spotting
        {
            *window = QuietWindow::WaitingForQuiet;
        }
    }

    /// When the quiet interval ends if the quiet lasts; none until it starts.
    #[must_use]
    pub fn ready_at(&self) -> Option<Instant> {
        match &self.spotting {
            Spotting::Disputed(Dispute {
                window: QuietWindow::CountingDown { since },
                pause,
                ..
            }) => Some(*since + *pause),
            _ => None,
        }
    }

    /// Takes the notice once the quiet interval has passed.
    pub fn notice(&mut self, now: Instant) -> Option<(Notice, Resolution)> {
        if now < self.ready_at()? {
            return None;
        }
        let Spotting::Disputed(dispute) =
            std::mem::replace(&mut self.spotting, Spotting::Listening)
        else {
            return None;
        };
        let notice = Notice {
            phrase: dispute.heard.phrase.clone(),
            ending: dispute.heard.ending,
        };
        Some((notice, dispute.heard.resolve(Outcome::Notified, now)))
    }

    /// The capture ended: a hit still waiting is told as unresolved.
    pub fn finish(&mut self, now: Instant) -> Option<Resolution> {
        let heard = match std::mem::replace(&mut self.spotting, Spotting::Listening) {
            Spotting::Listening => return None,
            Spotting::Claimed(heard) => heard,
            Spotting::Disputed(dispute) => dispute.heard,
        };
        Some(heard.resolve(Outcome::Ended, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(ending: Ending, end: u64) -> SpotterHit {
        SpotterHit {
            phrase: "paste that".to_owned(),
            ending,
            start_sample: end - 8_000,
            end_sample: end,
            confidence: 0.5,
        }
    }

    fn watch() -> Watch {
        Watch::new(EndingPauses::default())
    }

    /// Sends frames until `samples` have gone to the recognizer.
    fn send_to(watch: &mut Watch, samples: u64, now: Instant) {
        let more = usize::try_from((samples - watch.sent) / FRAME_SAMPLES as u64).expect("fits");
        watch.sent(more, now, false);
    }

    #[test]
    fn a_covering_final_decides_a_hit_by_what_it_did_to_the_capture() {
        let now = Instant::now();
        let cases = [
            (Effect::Ending(Ending::Paste), Some(Outcome::Agreed)),
            (Effect::Ending(Ending::Submit), Some(Outcome::Conflict)),
            (Effect::Ending(Ending::Scratch), Some(Outcome::Conflict)),
            (Effect::Text, None),
            (Effect::Nothing, None),
        ];
        for (effect, outcome) in cases {
            let mut watch = watch();
            assert_eq!(watch.hit(&hit(Ending::Paste, 40_000), now, false), None);
            send_to(&mut watch, 52_000, now);
            let resolved = watch.final_arrived(effect, now, false);
            assert_eq!(resolved.map(|r| r.outcome), outcome, "{effect:?}");
            assert_eq!(watch.disputed(), effect == Effect::Text, "{effect:?}");
        }
    }

    #[test]
    fn a_final_that_ended_before_the_phrase_does_not_cover_it() {
        let now = Instant::now();
        let mut watch = watch();
        send_to(&mut watch, 30_000, now);
        assert_eq!(watch.hit(&hit(Ending::Paste, 40_000), now, false), None);
        assert_eq!(watch.final_arrived(Effect::Text, now, false), None);
        assert!(!watch.disputed());
    }

    #[test]
    fn a_final_that_came_before_the_spotter_still_decides_the_hit() {
        let now = Instant::now();
        let mut watch = watch();
        send_to(&mut watch, 50_000, now);
        assert_eq!(
            watch.final_arrived(Effect::Ending(Ending::Paste), now, false),
            None
        );
        let resolved = watch.hit(&hit(Ending::Paste, 40_000), now, false);
        assert_eq!(resolved.map(|r| r.outcome), Some(Outcome::Agreed));
    }

    #[test]
    fn a_disputed_ending_is_told_only_after_the_quiet_interval_and_only_once() {
        let now = Instant::now();
        let mut watch = watch();
        watch.hit(&hit(Ending::Paste, 40_000), now, false);
        send_to(&mut watch, 52_000, now);
        watch.final_arrived(Effect::Text, now, false);
        let ready = watch.ready_at().expect("counting down");
        assert_eq!(
            ready,
            now + EndingPauses::default().get(CaptureRoute::Paste)
        );
        assert_eq!(watch.notice(ready - Duration::from_millis(1)), None);
        let (notice, resolved) = watch.notice(ready).expect("quiet long enough");
        assert_eq!(notice.phrase, "paste that");
        assert_eq!(resolved.outcome, Outcome::Notified);
        assert_eq!(watch.notice(ready), None);
    }

    #[test]
    fn speech_restarts_the_quiet_interval_and_words_dismiss_the_dispute_silently() {
        let now = Instant::now();
        let step = Duration::from_millis(100);
        let mut watch = watch();
        watch.hit(&hit(Ending::Submit, 40_000), now, true);
        send_to(&mut watch, 52_000, now);
        watch.final_arrived(Effect::Text, now, true);
        assert_eq!(watch.ready_at(), None, "waits for quiet while speaking");
        watch.quiet_frame(now + step);
        assert_eq!(
            watch.ready_at(),
            Some(now + step + Duration::from_millis(2_500))
        );
        watch.speaking();
        assert_eq!(watch.ready_at(), None);
        watch.quiet_frame(now + step * 2);
        assert!(watch.ready_at().is_some());
        let resolved = watch.words(now + step * 3).expect("dismissed");
        assert_eq!(resolved.outcome, Outcome::Continued);
        assert_eq!(watch.notice(now + Duration::from_secs(60)), None);
    }

    #[test]
    fn a_final_after_the_dispute_decides_whether_it_was_one() {
        let cases = [
            (Effect::Ending(Ending::Paste), Outcome::AgreedLate),
            (Effect::Ending(Ending::Cancel), Outcome::Conflict),
            (Effect::Text, Outcome::Continued),
        ];
        for (effect, outcome) in cases {
            let now = Instant::now();
            let mut watch = watch();
            watch.hit(&hit(Ending::Paste, 40_000), now, false);
            send_to(&mut watch, 52_000, now);
            watch.final_arrived(Effect::Text, now, false);
            let resolved = watch.final_arrived(effect, now, false);
            assert_eq!(resolved.map(|r| r.outcome), Some(outcome), "{effect:?}");
            assert!(!watch.disputed());
        }
    }

    #[test]
    fn no_final_at_all_is_a_dispute_only_after_the_measured_delay() {
        let now = Instant::now();
        let mut watch = watch();
        watch.hit(&hit(Ending::Paste, 40_000), now, false);
        let limit = 40_000 + samples(NO_FINAL_AFTER);
        send_to(&mut watch, limit - 1_280, now);
        assert!(!watch.disputed(), "an 800 ms wait is not enough");
        send_to(&mut watch, limit + 1_280, now);
        assert!(watch.disputed());
    }

    #[test]
    fn a_hit_that_waits_when_the_capture_ends_is_reported_unresolved() {
        let now = Instant::now();
        let mut watch = watch();
        watch.hit(&hit(Ending::Paste, 40_000), now, false);
        assert_eq!(watch.finish(now).map(|r| r.outcome), Some(Outcome::Ended));
        assert_eq!(watch.finish(now), None);
    }
}

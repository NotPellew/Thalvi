//! Test doubles for the engine's seams: scripted microphone frames, wake events and cues.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{CuePlayer, FrameSource, WakeDetector};
use crate::audio::{AudioError, InputGate};
use crate::cues::{Cue, CueError, CueTiming};
use crate::frame::{Frame, Queued};
use crate::wake::{WakeError, WakeEvent, WakeFrame};

/// Microphone frames a test feeds in, dropped while the gate is off or from before it changed,
/// as the capture callback does.
pub struct FakeFrames {
    gate: InputGate,
    frames: mpsc::UnboundedReceiver<(u64, Result<Queued, AudioError>)>,
}

/// Test-side handle of [`FakeFrames`].
pub struct FrameFeed {
    gate: InputGate,
    frames: mpsc::UnboundedSender<(u64, Result<Queued, AudioError>)>,
}

impl FakeFrames {
    #[must_use]
    pub fn new(gate: &InputGate) -> (Self, FrameFeed) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                gate: gate.clone(),
                frames: rx,
            },
            FrameFeed {
                gate: gate.clone(),
                frames: tx,
            },
        )
    }
}

impl FrameFeed {
    /// Delivers a frame captured now; lost when the gate is off.
    pub fn send(&self, frame: Frame) {
        let (enabled, epoch) = self.gate.state();
        if enabled {
            let _ = self.frames.send((epoch, Ok(Queued::now(frame))));
        }
    }

    /// Fails the microphone.
    pub fn fail(&self, error: AudioError) {
        let (_, epoch) = self.gate.state();
        let _ = self.frames.send((epoch, Err(error)));
    }
}

impl FrameSource for FakeFrames {
    async fn next(&mut self) -> Result<Queued, AudioError> {
        loop {
            let (epoch, frame) = self.frames.recv().await.ok_or(AudioError::Closed)?;
            if frame.is_err() || epoch == self.gate.state().1 {
                return frame;
            }
        }
    }
}

/// A wake detector that reports the event a test sets for the next frame.
pub struct FakeWake {
    next: Rc<Cell<Option<Result<WakeEvent, ()>>>>,
    scores: Rc<Cell<(f32, f32)>>,
    inference: Rc<Cell<Duration>>,
    resets: Rc<Cell<u32>>,
}

/// Test-side handle of [`FakeWake`].
pub struct WakeControl {
    next: Rc<Cell<Option<Result<WakeEvent, ()>>>>,
    scores: Rc<Cell<(f32, f32)>>,
    inference: Rc<Cell<Duration>>,
    resets: Rc<Cell<u32>>,
}

impl FakeWake {
    #[must_use]
    pub fn new() -> (Self, WakeControl) {
        let next = Rc::new(Cell::new(None));
        let scores = Rc::new(Cell::new((0.0, 0.0)));
        let inference = Rc::new(Cell::new(Duration::ZERO));
        let resets = Rc::new(Cell::new(0));
        (
            Self {
                next: Rc::clone(&next),
                scores: Rc::clone(&scores),
                inference: Rc::clone(&inference),
                resets: Rc::clone(&resets),
            },
            WakeControl {
                next,
                scores,
                inference,
                resets,
            },
        )
    }
}

impl WakeControl {
    /// The next frame scored reports `event`.
    pub fn fire(&self, event: WakeEvent) {
        self.next.set(Some(Ok(event)));
    }

    /// The frames scored from now on get these head scores.
    pub fn score(&self, capture: f32, conversation: f32) {
        self.scores.set((capture, conversation));
    }

    /// The frames scored from now on report taking `took`.
    pub fn inference(&self, took: Duration) {
        self.inference.set(took);
    }

    /// The next frame scored fails.
    pub fn fail(&self) {
        self.next.set(Some(Err(())));
    }

    /// How many times the engine forgot the audio so far.
    #[must_use]
    pub fn resets(&self) -> u32 {
        self.resets.get()
    }
}

impl WakeDetector for FakeWake {
    fn process(&mut self, _frame: &Frame) -> Result<WakeFrame, WakeError> {
        let event = match self.next.take() {
            Some(Err(())) => return Err(WakeError::Inference("scripted failure".to_owned())),
            Some(Ok(event)) => Some(event),
            None => None,
        };
        let (capture, conversation) = self.scores.get();
        Ok(WakeFrame {
            capture,
            conversation,
            inference: self.inference.get(),
            event,
        })
    }

    fn reset(&mut self) {
        self.resets.set(self.resets.get() + 1);
    }
}

/// What a [`FakeCuePlayer`] did to a cue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CueLog {
    Started(Cue),
    Finished(Cue),
    /// Dropped before it finished, as when the capture is cancelled.
    Cut(Cue),
}

type Log = Rc<RefCell<Vec<(Duration, CueLog)>>>;

struct CueScript {
    lengths: Vec<(Cue, Duration)>,
    failing: Vec<Cue>,
    timing: CueTiming,
}

/// A cue player whose cues take the lengths a test sets on the runtime's clock.
pub struct FakeCuePlayer {
    origin: Instant,
    log: Log,
    script: Rc<RefCell<CueScript>>,
}

/// Test-side handle of [`FakeCuePlayer`].
pub struct CueControl {
    origin: Instant,
    log: Log,
    script: Rc<RefCell<CueScript>>,
}

impl FakeCuePlayer {
    /// Cues take 230 ms for the wake cue and 100 ms for the others, and report starting after
    /// 25 ms with a 15 ms device delay.
    #[must_use]
    pub fn new() -> (Self, CueControl) {
        let origin = Instant::now();
        let log = Log::default();
        let script = Rc::new(RefCell::new(CueScript {
            lengths: vec![(Cue::Wake, Duration::from_millis(230))],
            failing: Vec::new(),
            timing: CueTiming {
                start: Duration::from_millis(25),
                output: Some(Duration::from_millis(15)),
            },
        }));
        (
            Self {
                origin,
                log: Rc::clone(&log),
                script: Rc::clone(&script),
            },
            CueControl {
                origin,
                log,
                script,
            },
        )
    }
}

impl CueControl {
    /// How long `cue` takes to play.
    pub fn set_length(&self, cue: Cue, length: Duration) {
        self.script.borrow_mut().lengths.push((cue, length));
    }

    /// What every cue reports as the timing of its first sample.
    pub fn set_timing(&self, timing: CueTiming) {
        self.script.borrow_mut().timing = timing;
    }

    /// `cue` fails to play, after its length.
    pub fn fail(&self, cue: Cue) {
        self.script.borrow_mut().failing.push(cue);
    }

    /// Every cue event so far, with the time since the player was made.
    #[must_use]
    pub fn log(&self) -> Vec<(Duration, CueLog)> {
        self.log.borrow().clone()
    }

    /// The time since the player was made.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Logs a cue as cut unless disarmed.
struct CutOnDrop<'a> {
    player: &'a FakeCuePlayer,
    cue: Cue,
    done: bool,
}

impl Drop for CutOnDrop<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.player.record(CueLog::Cut(self.cue));
        }
    }
}

impl FakeCuePlayer {
    fn record(&self, entry: CueLog) {
        self.log.borrow_mut().push((self.origin.elapsed(), entry));
    }
}

impl CuePlayer for FakeCuePlayer {
    async fn play(&self, cue: Cue) -> Result<Option<CueTiming>, CueError> {
        let (length, fails, timing) = {
            let script = self.script.borrow();
            let length = script
                .lengths
                .iter()
                .rev()
                .find(|(known, _)| *known == cue)
                .map_or(Duration::from_millis(100), |(_, length)| *length);
            (length, script.failing.contains(&cue), script.timing)
        };
        self.record(CueLog::Started(cue));
        let mut cut = CutOnDrop {
            player: self,
            cue,
            done: false,
        };
        tokio::time::sleep(length).await;
        cut.done = true;
        self.record(CueLog::Finished(cue));
        if fails {
            return Err(CueError::NoOutputDevice);
        }
        Ok(Some(timing))
    }
}

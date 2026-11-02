//! The capture engine on a paused clock, with a fake microphone, wake word, cues and recognizer.

#![expect(clippy::unwrap_used, reason = "a test fails by panicking")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use thalvi_desktop::audio::{AudioError, InputGate};
use thalvi_desktop::capture::{CaptureRequest, CaptureRoute};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::cues::Cue;
use thalvi_desktop::engine::fakes::{
    CueControl, CueLog, FakeCuePlayer, FakeFrames, FakeWake, FrameFeed, WakeControl,
};
use thalvi_desktop::engine::{
    CaptureError, CaptureOutcome, Engine, EngineHandle, EngineState, ListenError, Parts,
    WakeTrigger,
};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::metrics::{RecordKinds, Recorder};
use thalvi_desktop::recognizer::{
    FakeCall, FakeControl, FakeRecognizer, RecognizerError, RecognizerEvent,
};
use thalvi_desktop::vad::VadDetector;
use thalvi_desktop::wake::WakeEvent;
use tokio::task::{JoinHandle, LocalSet};

type TestEngine = Engine<FakeRecognizer, FakeWake, FakeCuePlayer>;

const FRAME_MS: u64 = 80;

#[derive(Clone, Copy)]
enum Kind {
    Quiet,
    Voice,
    /// Low noise the detector calls speech normally but not while an ending is pending.
    Hiss,
}

/// Silence, numbered by its `id` so a test can tell which frame reached the recognizer.
fn quiet(id: usize) -> Frame {
    let mut frame = [0; FRAME_SAMPLES];
    frame[0] = i16::try_from(id).unwrap();
    frame
}

/// A 140 Hz buzz with harmonics and a slow swell, standing in for a voice.
fn voiced(id: usize) -> Frame {
    std::array::from_fn(|i| {
        let t = f64::from(u32::try_from(id * FRAME_SAMPLES + i).unwrap()) / 16_000.0;
        let swell = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
        let buzz: f64 = (1..=12)
            .map(|h| (2.0 * std::f64::consts::PI * 140.0 * f64::from(h) * t).sin() / f64::from(h))
            .sum();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "amplitude stays below 16 bits"
        )]
        let sample = (buzz * 4000.0 * swell) as i16;
        sample
    })
}

fn hiss(id: usize) -> Frame {
    let mut state = u32::try_from(id).unwrap() + 1;
    std::array::from_fn(|_| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        i16::try_from(i32::try_from((state >> 16) % 101).unwrap() - 50).unwrap()
    })
}

impl Kind {
    fn frame(self, id: usize) -> Frame {
        match self {
            Self::Quiet => quiet(id),
            Self::Voice => voiced(id),
            Self::Hiss => hiss(id),
        }
    }
}

/// The first sample of each frame, to compare frames readably.
fn tags(frames: &[Frame]) -> Vec<i16> {
    frames.iter().map(|frame| frame[0]).collect()
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

struct Rig {
    feed: FrameFeed,
    wake: WakeControl,
    cues: CueControl,
    server: FakeControl,
    handle: EngineHandle,
    gate: InputGate,
    notices: Rc<RefCell<Vec<String>>>,
}

impl Rig {
    fn new(config: &str) -> (Self, TestEngine, FakeFrames) {
        Self::recording(config, Recorder::disabled())
    }

    fn recording(config: &str, recorder: Recorder) -> (Self, TestEngine, FakeFrames) {
        let config = DesktopConfig::from_json_str(config, None).unwrap();
        let gate = InputGate::default();
        let (frames, feed) = FakeFrames::new(&gate);
        let (wake_detector, wake) = FakeWake::new();
        let (cue_player, cues) = FakeCuePlayer::new();
        let (recognizer, server) = FakeRecognizer::new();
        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&notices);
        let (engine, handle) = Engine::new(
            Parts {
                recognizer,
                wake: wake_detector,
                cues: cue_player,
                gate: gate.clone(),
                http: reqwest::Client::new(),
                notice: Box::new(move |notice| sink.borrow_mut().push(notice.to_owned())),
            },
            &config,
            recorder,
        );
        (
            Self {
                feed,
                wake,
                cues,
                server,
                handle,
                gate,
                notices,
            },
            engine,
            frames,
        )
    }

    fn now(&self) -> u64 {
        u64::try_from(self.cues.elapsed().as_millis()).unwrap()
    }

    /// Moves the clock to `at` milliseconds from the start, letting everything due run.
    async fn at(&self, at: u64) {
        let now = self.now();
        assert!(at >= now, "time only moves forward: {at} < {now}");
        tokio::time::advance(ms(at - now)).await;
        settle().await;
    }

    /// Sends a frame every 80 ms until `until`, then moves the clock there.
    async fn feed_until(&self, until: u64, kind: Kind) {
        let mut next = (self.now() / FRAME_MS + 1) * FRAME_MS;
        while next <= until {
            self.at(next).await;
            self.feed
                .send(kind.frame(usize::try_from(next / FRAME_MS).unwrap()));
            settle().await;
            next += FRAME_MS;
        }
        self.at(until).await;
    }

    async fn finals(&self, text: &str) {
        self.server
            .emit(Ok(RecognizerEvent::Final(text.to_owned())));
        settle().await;
    }

    async fn partial(&self, text: &str) {
        self.server
            .emit(Ok(RecognizerEvent::Partial(text.to_owned())));
        settle().await;
    }

    /// The wake word is heard at the current time.
    async fn wake_word(&self) {
        self.wake.fire(WakeEvent::Capture);
        self.feed.send(quiet(0));
        settle().await;
    }

    /// Wake word at 0 and the recognizer ready at once: audio flows from the end of the wake cue.
    async fn open(&mut self) {
        self.wake_word().await;
        self.server.ready();
        settle().await;
        self.at(230).await;
    }

    fn sent(&self) -> Vec<Frame> {
        self.server
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                FakeCall::Frame(frame) => Some(*frame),
                _ => None,
            })
            .collect()
    }

    fn cue_log(&self) -> Vec<(u64, CueLog)> {
        self.cues
            .log()
            .into_iter()
            .map(|(at, log)| (u64::try_from(at.as_millis()).unwrap(), log))
            .collect()
    }

    /// When `cue` started playing, each time.
    fn started(&self, cue: Cue) -> Vec<u64> {
        self.cue_log()
            .into_iter()
            .filter(|(_, log)| *log == CueLog::Started(cue))
            .map(|(at, _)| at)
            .collect()
    }

    fn state(&self) -> EngineState {
        self.handle.state().borrow().clone()
    }

    fn notices(&self) -> Vec<String> {
        self.notices.borrow().clone()
    }
}

/// Runs a wake then a capture on the engine, as a caller would.
fn capture_task(mut engine: TestEngine, mut frames: FakeFrames) -> JoinHandle<CaptureOutcome> {
    tokio::task::spawn_local(async move {
        let trigger = engine.listen(&mut frames).await.unwrap();
        assert_eq!(trigger, WakeTrigger::Capture);
        let outcome = engine.capture(&mut frames).await;
        engine.finish().await;
        outcome
    })
}

fn accepted(route: CaptureRoute, text: &str) -> CaptureOutcome {
    CaptureOutcome::Accepted(CaptureRequest {
        route,
        text: text.to_owned(),
    })
}

#[test]
fn the_voice_fixture_is_speech_to_both_detector_modes() {
    for pending in [false, true] {
        let mut vad = VadDetector::new();
        for id in 0..6 {
            assert!(vad.speech(&voiced(id), pending), "pending {pending} {id}");
        }
        assert!(!VadDetector::new().speech(&quiet(7), pending));
    }
}

#[tokio::test(start_paused = true)]
async fn the_wake_cue_and_the_connection_overlap_and_only_audio_after_both_is_sent() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.wake_word().await;
            assert!(!rig.gate.is_enabled(), "the gate closes at the wake word");
            assert_eq!(rig.state(), EngineState::StartingCapture);
            rig.feed.send(quiet(100));

            rig.at(229).await;
            assert!(
                !rig.gate.is_enabled(),
                "the gate stays closed during the cue"
            );
            rig.at(230).await;
            assert!(rig.gate.is_enabled(), "the gate opens when the cue ends");

            rig.at(310).await;
            rig.feed.send(quiet(31));
            rig.at(390).await;
            rig.feed.send(quiet(39));
            rig.at(600).await;
            assert_eq!(
                rig.sent().len(),
                0,
                "nothing is sent before the recognizer is ready"
            );
            assert_eq!(rig.state(), EngineState::StartingCapture);
            rig.server.ready();
            settle().await;
            assert_eq!(tags(&rig.sent()), tags(&[quiet(31), quiet(39)]));
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: String::new(),
                    partial: None
                }
            );

            rig.at(800).await;
            rig.finals("never mind").await;
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
            assert_eq!(
                rig.cue_log(),
                [
                    (0, CueLog::Started(Cue::Wake)),
                    (230, CueLog::Finished(Cue::Wake)),
                    (800, CueLog::Started(Cue::Cancelled)),
                    (900, CueLog::Finished(Cue::Cancelled)),
                ]
            );
            assert_eq!(
                rig.server.calls(),
                [
                    FakeCall::Frame(Box::new(quiet(31))),
                    FakeCall::Frame(Box::new(quiet(39))),
                    FakeCall::Cancel
                ]
            );
            assert!(rig.gate.is_enabled());
            assert_eq!(rig.state(), EngineState::Listening);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_wake_cue_that_is_off_or_fails_still_reopens_the_gate_and_the_capture_goes_on() {
    LocalSet::new()
        .run_until(async {
            for (config, fails, open_at) in [
                (r#"{"cues": {"wake": false}}"#, false, 0),
                ("{}", true, 230),
            ] {
                let (mut rig, engine, frames) = Rig::new(config);
                if fails {
                    rig.cues.fail(Cue::Wake);
                }
                let task = capture_task(engine, frames);
                rig.wake_word().await;
                rig.server.ready();
                rig.at(open_at.max(1) - 1).await;
                assert_eq!(rig.gate.is_enabled(), open_at == 0, "{config}");
                rig.at(open_at.max(1)).await;
                assert!(rig.gate.is_enabled(), "{config}");
                rig.feed.send(quiet(5));
                settle().await;
                assert_eq!(tags(&rig.sent()), tags(&[quiet(5)]), "{config}");
                let expected: &[&str] = if fails {
                    &["Audio feedback failed. Check the output device. The capture continues."]
                } else {
                    &[]
                };
                assert_eq!(rig.notices(), expected, "{config}");
                rig.handle.cancel();
                assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
            }
        })
        .await;
}

enum Act {
    Audio(u64, Kind),
    Final(u64, &'static str),
    Partial(u64, &'static str),
}

struct Case {
    name: &'static str,
    script: Vec<Act>,
    route: CaptureRoute,
    text: &'static str,
    provisional_at: &'static [u64],
    /// When the ending is delivered, in milliseconds after the wake word.
    commit: u64,
}

impl Rig {
    async fn play(&mut self, script: &[Act]) {
        for act in script {
            match act {
                Act::Audio(until, kind) => self.feed_until(*until, *kind).await,
                Act::Final(at, text) => {
                    self.at(*at).await;
                    self.finals(text).await;
                }
                Act::Partial(at, text) => {
                    self.at(*at).await;
                    self.partial(text).await;
                }
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn an_ending_is_delivered_once_the_quiet_pause_has_passed() {
    use Act::{Audio, Final, Partial};
    use Kind::{Quiet, Voice};
    let cases = [
        Case {
            name: "second final ends a multi-piece dictation",
            script: vec![
                Audio(1000, Quiet),
                Final(1000, "First sentence."),
                Audio(2000, Quiet),
                Final(2000, "Second paste that."),
            ],
            route: CaptureRoute::Paste,
            text: "First sentence. Second",
            provisional_at: &[2000],
            commit: 4500,
        },
        Case {
            name: "an ending split across finals",
            script: vec![
                Audio(1000, Quiet),
                Final(1000, "Keep this finish,"),
                Audio(1400, Quiet),
                Final(1400, "dictation!"),
            ],
            route: CaptureRoute::Paste,
            text: "Keep this",
            provisional_at: &[1400],
            commit: 3900,
        },
        Case {
            name: "breaths and artifacts neither cancel nor restart the pause",
            script: vec![
                Audio(1000, Quiet),
                Final(1000, "Run tests submit, um... [noise]"),
                Partial(1500, "<|nospeech|>"),
                Audio(2000, Quiet),
                Final(2000, "(breathing)"),
            ],
            route: CaptureRoute::Submit,
            text: "Run tests",
            provisional_at: &[1000],
            commit: 3500,
        },
        Case {
            // The detector keeps judging speech for two frames after the voice stops.
            name: "an ending armed while still speaking counts from the first quiet frame",
            script: vec![
                Audio(880, Quiet),
                Audio(1000, Voice),
                Final(1000, "Hello paste that"),
                Audio(1600, Voice),
            ],
            route: CaptureRoute::Paste,
            text: "Hello",
            provisional_at: &[1000],
            commit: 1840 + 2500,
        },
        Case {
            name: "sustained noise and an artifact partial do not cancel the ending",
            script: vec![
                Audio(1000, Quiet),
                Final(1000, "Hello paste that"),
                Audio(1360, Quiet),
                Audio(1600, Voice),
                Partial(1600, "[noise]"),
            ],
            route: CaptureRoute::Paste,
            text: "Hello",
            provisional_at: &[1000],
            commit: 3500,
        },
    ];
    LocalSet::new()
        .run_until(async {
            for case in cases {
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.play(&case.script).await;
                rig.feed_until(case.commit - 1, Quiet).await;
                assert!(!task.is_finished(), "{}: delivered early", case.name);
                assert_eq!(rig.started(Cue::Accepted).len(), 0, "{}", case.name);
                rig.at(case.commit).await;
                assert_eq!(rig.started(Cue::Accepted), [case.commit], "{}", case.name);
                assert_eq!(
                    task.await.unwrap(),
                    accepted(case.route, case.text),
                    "{}",
                    case.name
                );
                assert_eq!(
                    rig.started(Cue::Provisional),
                    case.provisional_at,
                    "{}",
                    case.name
                );
                assert_eq!(rig.started(Cue::Cancelled).len(), 0, "{}", case.name);
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn speech_while_an_ending_is_pending_is_held_until_it_lasts_then_cancels_the_ending() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            let before_ending: Vec<Frame> = (3..=12).map(quiet).collect();
            assert_eq!(tags(&rig.sent()), tags(&before_ending));

            rig.finals("Hello submit").await;
            rig.feed_until(1760, Kind::Quiet).await;
            rig.feed_until(1920, Kind::Voice).await;
            assert_eq!(
                rig.sent(),
                before_ending,
                "two speech frames are not enough"
            );

            rig.feed_until(2000, Kind::Voice).await;
            let relayed: Vec<Frame> = (23..=25).map(voiced).collect();
            assert_eq!(
                rig.sent()[10..],
                relayed,
                "the held audio is relayed at once"
            );

            rig.feed_until(2100, Kind::Quiet).await;
            rig.partial("more words").await;
            assert_eq!(rig.started(Cue::Cancelled), [2100]);
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: "Hello submit".to_owned(),
                    partial: Some("more words".to_owned())
                }
            );

            rig.feed_until(3000, Kind::Quiet).await;
            rig.finals("more words submit").await;
            rig.feed_until(5499, Kind::Quiet).await;
            assert!(!task.is_finished());
            rig.at(5500).await;
            assert_eq!(
                task.await.unwrap(),
                accepted(CaptureRoute::Submit, "Hello submit more words")
            );
            assert_eq!(rig.started(Cue::Provisional), [1000, 3000]);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn audio_held_back_by_a_cancelled_ending_is_sent_with_the_next_frame() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello submit").await;
            rig.feed_until(1200, Kind::Quiet).await;
            let before_ending: Vec<Frame> = (3..=12).map(quiet).collect();
            assert_eq!(tags(&rig.sent()), tags(&before_ending));

            rig.partial("hello again").await;
            assert_eq!(
                tags(&rig.sent()),
                tags(&before_ending),
                "a cancelled ending sends nothing by itself"
            );
            rig.feed_until(1280, Kind::Quiet).await;
            let held: Vec<Frame> = (13..=16).map(quiet).collect();
            assert_eq!(
                tags(&rig.sent()[10..]),
                tags(&held),
                "the three held frames and the new one"
            );

            rig.handle.cancel();
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn an_ending_heard_only_in_a_partial_never_arms() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.partial("Hello paste that").await;
            rig.feed_until(6000, Kind::Quiet).await;
            assert!(!task.is_finished());
            assert_eq!(rig.started(Cue::Provisional).len(), 0);
            assert!(matches!(rig.state(), EngineState::Capturing { .. }));
            rig.finals("never mind").await;
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn never_mind_cancels_at_once_without_waiting_or_a_provisional_cue() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Draft words").await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.finals("never mind").await;
            assert_eq!(rig.started(Cue::Cancelled), [1500]);
            rig.at(1600).await;
            assert!(task.is_finished(), "no pause is waited out");
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
            assert_eq!(rig.started(Cue::Provisional).len(), 0);
            assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel));
            assert!(rig.gate.is_enabled());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_from_outside_stops_whatever_the_capture_is_doing() {
    #[derive(Clone, Copy)]
    enum Moment {
        Starting,
        Countdown,
        AcceptedCue,
    }
    LocalSet::new()
        .run_until(async {
            for moment in [Moment::Starting, Moment::Countdown, Moment::AcceptedCue] {
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = capture_task(engine, frames);
                match moment {
                    Moment::Starting => {
                        rig.wake_word().await;
                        rig.at(100).await;
                    }
                    Moment::Countdown | Moment::AcceptedCue => {
                        rig.open().await;
                        rig.feed_until(1000, Kind::Quiet).await;
                        rig.finals("Hello paste that").await;
                        let until = if matches!(moment, Moment::Countdown) {
                            2200
                        } else {
                            3550
                        };
                        rig.feed_until(until, Kind::Quiet).await;
                    }
                }
                rig.handle.cancel();
                assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
                let log = rig.cue_log();
                match moment {
                    Moment::Starting => {
                        assert_eq!(log.last(), Some(&(100, CueLog::Cut(Cue::Wake))));
                        assert_eq!(rig.server.calls().len(), 0);
                    }
                    Moment::Countdown => {
                        assert_eq!(rig.started(Cue::Accepted).len(), 0);
                        assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel));
                    }
                    Moment::AcceptedCue => {
                        assert_eq!(log.last(), Some(&(3550, CueLog::Cut(Cue::Accepted))));
                        assert_eq!(rig.server.calls().last(), Some(&FakeCall::Close));
                    }
                }
                assert!(rig.gate.is_enabled());
                assert_eq!(rig.state(), EngineState::Listening);
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_cancel_while_listening_does_not_cancel_the_next_capture() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.handle.cancel();
            settle().await;
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.feed_until(3500, Kind::Quiet).await;
            assert_eq!(task.await.unwrap(), accepted(CaptureRoute::Paste, "Hello"));
        })
        .await;
}

#[derive(Clone, Copy)]
enum Fault {
    RecognizerDrops,
    MicDies,
    MicGoesSilent,
    CannotConnect,
    NeverReady,
    WakeCueTooLong,
    Full,
}

const DISCONNECTED: &str = "Speech recognition disconnected. Nothing was delivered.";

/// Drives the capture on `rig` into `fault`, the way a real failure would arrive.
async fn inflict(rig: &mut Rig, task: &JoinHandle<CaptureOutcome>, fault: Fault) {
    match fault {
        Fault::CannotConnect => {
            rig.wake_word().await;
            rig.server
                .fail("Thalvi could not reach speech recognition.");
        }
        Fault::NeverReady => {
            rig.wake_word().await;
            rig.at(7999).await;
            assert!(!task.is_finished(), "still waiting for 8 s");
            rig.at(8000).await;
        }
        Fault::WakeCueTooLong => {
            rig.cues.set_length(Cue::Wake, ms(9000));
            rig.wake_word().await;
            rig.server.ready();
            rig.at(7999).await;
            assert!(!task.is_finished());
            rig.at(8000).await;
            assert!(rig.cue_log().contains(&(8000, CueLog::Cut(Cue::Wake))));
        }
        Fault::RecognizerDrops | Fault::MicDies | Fault::Full => {
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            let first = if matches!(fault, Fault::Full) {
                "x".repeat(19_990)
            } else {
                "Saved paragraph".to_owned()
            };
            rig.finals(&first).await;
            rig.at(1100).await;
            match fault {
                Fault::RecognizerDrops => rig.server.emit(Err(RecognizerError {
                    message: DISCONNECTED.to_owned(),
                })),
                Fault::MicDies => rig.feed.fail(AudioError::DeviceLost),
                _ => rig.finals(&"y".repeat(20)).await,
            }
        }
        Fault::MicGoesSilent => {
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.at(1100).await;
            rig.at(1349).await;
            assert!(!task.is_finished(), "250 ms after the cue ended");
            rig.at(1350).await;
        }
    }
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_capture_says_what_happened_keeps_the_text_and_returns_to_listening() {
    let saved = Some("Saved paragraph".to_owned());
    let recognizer = |message: &str| {
        CaptureError::Recognizer(RecognizerError {
            message: message.to_owned(),
        })
    };
    // Fault, the error, the text kept, when the error cue plays, and whether a stream was open.
    let cases = [
        (
            Fault::RecognizerDrops,
            recognizer(DISCONNECTED),
            saved.clone(),
            1100,
            true,
        ),
        (
            Fault::MicDies,
            CaptureError::Audio(AudioError::DeviceLost),
            saved,
            1100,
            true,
        ),
        (
            Fault::MicGoesSilent,
            CaptureError::MicStopped,
            Some("Hello".to_owned()),
            1350,
            true,
        ),
        (
            Fault::CannotConnect,
            recognizer("Thalvi could not reach speech recognition."),
            None,
            0,
            false,
        ),
        (Fault::NeverReady, CaptureError::NotReady, None, 8000, false),
        (
            Fault::WakeCueTooLong,
            CaptureError::NotReady,
            None,
            8000,
            true,
        ),
        (
            Fault::Full,
            CaptureError::Full(thalvi_desktop::capture::CaptureFullError),
            Some("x".repeat(19_990)),
            1100,
            true,
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (fault, error, recoverable_text, error_cue_at, had_stream) in cases {
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = capture_task(engine, frames);
                inflict(&mut rig, &task, fault).await;
                assert_eq!(
                    task.await.unwrap(),
                    CaptureOutcome::Failed {
                        error: error.clone(),
                        recoverable_text
                    },
                    "{error}"
                );
                assert_eq!(rig.started(Cue::Error), [error_cue_at], "{error}");
                assert_eq!(
                    rig.server.calls().last() == Some(&FakeCall::Cancel),
                    had_stream,
                    "{error}"
                );
                assert!(rig.gate.is_enabled(), "{error}");
                assert_eq!(rig.state(), EngineState::Listening, "{error}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_microphone_watchdog_waits_while_a_cue_is_playing() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            rig.cues.set_length(Cue::Provisional, ms(1000));
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.at(2000).await;
            rig.at(2249).await;
            assert!(!task.is_finished(), "the cue ended at 2000");
            rig.at(2250).await;
            assert_eq!(rig.started(Cue::Error), [2250]);
            assert_eq!(
                task.await.unwrap(),
                CaptureOutcome::Failed {
                    error: CaptureError::MicStopped,
                    recoverable_text: Some("Hello".to_owned())
                }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn wake_words_are_reported_and_an_ambiguous_one_only_once() {
    LocalSet::new()
        .run_until(async {
            let (rig, mut engine, mut frames) = Rig::new("{}");
            let task = tokio::task::spawn_local(async move {
                let first = engine.listen(&mut frames).await.unwrap();
                let second = engine.listen(&mut frames).await.unwrap();
                (first, second)
            });
            for _ in 0..25 {
                rig.wake.fire(WakeEvent::Ambiguous);
                rig.feed.send(quiet(1));
                settle().await;
            }
            assert_eq!(rig.notices(), ["Wake was ambiguous; still listening."]);
            assert!(!task.is_finished());
            rig.feed.send(quiet(2));
            settle().await;
            rig.wake.fire(WakeEvent::Ambiguous);
            rig.feed.send(quiet(3));
            settle().await;
            assert_eq!(rig.notices().len(), 2, "a quiet frame re-arms the notice");

            rig.wake.fire(WakeEvent::Conversation);
            rig.feed.send(quiet(4));
            settle().await;
            assert!(
                rig.gate.is_enabled(),
                "a conversation wake leaves the microphone on"
            );
            rig.wake.fire(WakeEvent::Capture);
            rig.feed.send(quiet(5));
            let (first, second) = task.await.unwrap();
            assert_eq!(
                (first, second),
                (WakeTrigger::Conversation, WakeTrigger::Capture)
            );
            assert!(!rig.gate.is_enabled(), "a capture wake closes the gate");
            assert_eq!(
                rig.wake.resets(),
                2,
                "each listen starts from forgotten audio"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn listening_stops_with_a_plain_error_when_the_microphone_or_the_wake_model_fails() {
    LocalSet::new()
        .run_until(async {
            for wake_fails in [true, false] {
                let (rig, mut engine, mut frames) = Rig::new("{}");
                let task =
                    tokio::task::spawn_local(async move { engine.listen(&mut frames).await });
                if wake_fails {
                    rig.wake.fail();
                    rig.feed.send(quiet(1));
                } else {
                    rig.feed.fail(AudioError::DeviceLost);
                }
                let error = task.await.unwrap().unwrap_err();
                match (wake_fails, error) {
                    (true, ListenError::Wake(_))
                    | (false, ListenError::Audio(AudioError::DeviceLost)) => {}
                    (_, other) => panic!("{other}"),
                }
            }
        })
        .await;
}

#[derive(Clone, Copy)]
enum Reply {
    Json(&'static str),
    Status(u16),
    /// Never answers.
    Stall,
}

/// A secondary transcription endpoint; reports how many frames each uploaded WAV holds.
async fn serve(reply: Reply) -> (String, tokio::sync::mpsc::UnboundedReceiver<usize>) {
    use axum::Router;
    use axum::body::to_bytes;
    use axum::extract::Request;
    use axum::http::StatusCode;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().fallback(move |request: Request| {
        let tx = tx.clone();
        async move {
            let body = to_bytes(request.into_body(), usize::MAX).await.unwrap();
            let riff = body.windows(4).position(|w| w == b"RIFF").unwrap();
            let data = u32::from_le_bytes(body[riff + 40..riff + 44].try_into().unwrap());
            // 80 ms at 24 kHz, 16 bits.
            let _ = tx.send(usize::try_from(data).unwrap() / 3840);
            match reply {
                Reply::Json(json) => (StatusCode::OK, json),
                Reply::Status(code) => (StatusCode::from_u16(code).unwrap(), "{}"),
                Reply::Stall => std::future::pending().await,
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, rx)
}

fn with_secondary(url: &str, api: &str) -> String {
    format!(r#"{{"stt": {{"secondary": {{"url": "{url}", "model": "m", "api": "{api}"}}}}}}"#)
}

/// A capture that says "live words paste that" at 1000 and is delivered at 3500.
async fn live_words(rig: &mut Rig) {
    rig.open().await;
    rig.feed_until(1000, Kind::Quiet).await;
    rig.finals("live words paste that").await;
    rig.feed_until(3499, Kind::Quiet).await;
    rig.at(3500).await;
}

const FALLBACK_NOTICE: &str = "The second transcription failed. Using the live transcript.";

#[tokio::test(start_paused = true)]
async fn the_secondary_transcription_replaces_the_text_of_the_frames_that_were_sent() {
    LocalSet::new()
        .run_until(async {
            for (api, reply) in [
                (
                    "open_ai",
                    r#"{"text": "Polished words paste that [noise]"}"#,
                ),
                (
                    "meta",
                    r#"{"transcript": "Polished words paste that [noise]"}"#,
                ),
            ] {
                let (url, mut uploads) = serve(Reply::Json(reply)).await;
                let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, api));
                let task = capture_task(engine, frames);
                live_words(&mut rig).await;
                assert_eq!(
                    task.await.unwrap(),
                    accepted(CaptureRoute::Paste, "Polished words"),
                    "{api}"
                );
                // Every frame from the end of the wake cue to the end of the capture.
                assert_eq!(uploads.recv().await, Some(rig.sent().len()), "{api}");
                assert_eq!(rig.notices().len(), 0, "{api}");
                assert_eq!(rig.state(), EngineState::Listening);
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_live_transcript_is_kept_with_a_notice_when_the_secondary_cannot_stand_in() {
    LocalSet::new()
        .run_until(async {
            for (name, reply) in [
                ("it times out", Reply::Stall),
                ("it fails", Reply::Status(500)),
                ("it hears nothing", Reply::Json(r#"{"text": ""}"#)),
                (
                    "it hears another ending",
                    Reply::Json(r#"{"text": "Polished submit"}"#),
                ),
                (
                    "it hears never mind",
                    Reply::Json(r#"{"text": "never mind"}"#),
                ),
                (
                    "it hears scratch that",
                    Reply::Json(r#"{"text": "Polished scratch that"}"#),
                ),
            ] {
                let (url, _uploads) = serve(reply).await;
                let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, "open_ai"));
                let task = capture_task(engine, frames);
                live_words(&mut rig).await;
                assert_eq!(
                    task.await.unwrap(),
                    accepted(CaptureRoute::Paste, "live words"),
                    "{name}"
                );
                assert_eq!(rig.notices(), [FALLBACK_NOTICE], "{name}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_the_secondary_transcription_abandons_it() {
    LocalSet::new()
        .run_until(async {
            let (url, mut uploads) = serve(Reply::Stall).await;
            let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, "open_ai"));
            let task = capture_task(engine, frames);
            live_words(&mut rig).await;
            rig.at(3600).await;
            assert!(uploads.recv().await.is_some(), "the upload started");
            assert_eq!(
                rig.state(),
                EngineState::TranscribingSecondary {
                    route: CaptureRoute::Paste
                }
            );
            assert!(!rig.gate.is_enabled(), "no audio is wanted meanwhile");
            rig.handle.cancel();
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
            assert_eq!(rig.server.calls().last(), Some(&FakeCall::Close));
            assert!(rig.gate.is_enabled());
            assert_eq!(rig.notices().len(), 0);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn scratching_skips_the_secondary_and_only_the_final_ending_plays_a_provisional_cue() {
    LocalSet::new()
        .run_until(async {
            let (url, mut uploads) = serve(Reply::Json(r#"{"text": "Other words"}"#)).await;
            let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, "open_ai"));
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Keep.").await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.finals("Bad words scratch that.").await;
            rig.feed_until(2000, Kind::Quiet).await;
            rig.finals("Good paste that").await;
            rig.feed_until(4499, Kind::Quiet).await;
            rig.at(4500).await;
            assert_eq!(
                task.await.unwrap(),
                accepted(CaptureRoute::Paste, "Keep. Good")
            );
            assert_eq!(rig.started(Cue::Provisional), [2000]);
            assert!(uploads.try_recv().is_err(), "nothing was uploaded");
            assert_eq!(rig.notices().len(), 0);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_state_follows_the_capture() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            assert_eq!(rig.state(), EngineState::Listening);
            let task = capture_task(engine, frames);
            rig.wake_word().await;
            assert_eq!(rig.state(), EngineState::StartingCapture);
            rig.server.ready();
            rig.at(230).await;
            let capturing = |committed: &str, partial: Option<&str>| EngineState::Capturing {
                committed: committed.to_owned(),
                partial: partial.map(str::to_owned),
            };
            assert_eq!(rig.state(), capturing("", None));

            rig.feed_until(880, Kind::Quiet).await;
            rig.feed_until(1000, Kind::Voice).await;
            rig.partial("Hello").await;
            assert_eq!(rig.state(), capturing("", Some("Hello")));
            rig.finals("Hello paste that").await;
            let pending = |waiting_for_quiet| EngineState::PendingEnding {
                route: CaptureRoute::Paste,
                text: "Hello".to_owned(),
                waiting_for_quiet,
            };
            assert_eq!(rig.state(), pending(true), "still speaking");
            rig.feed_until(1760, Kind::Voice).await;
            assert_eq!(rig.state(), pending(true));
            // The detector keeps judging speech for two frames after the voice stops.
            rig.feed_until(1920, Kind::Quiet).await;
            assert_eq!(rig.state(), pending(true));
            rig.feed_until(2000, Kind::Quiet).await;
            assert_eq!(rig.state(), pending(false), "the pause has begun");

            rig.partial("and more").await;
            assert_eq!(rig.state(), capturing("Hello paste that", Some("and more")));
            rig.handle.cancel();
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
            assert_eq!(rig.state(), EngineState::Listening);
        })
        .await;
}

fn record_dir(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn read_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn events(lines: &[serde_json::Value]) -> Vec<(String, String)> {
    lines
        .iter()
        .map(|line| {
            (
                line["event"].as_str().unwrap().to_owned(),
                line["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn duration_of(lines: &[serde_json::Value], event: &str) -> f64 {
    lines.iter().find(|line| line["event"] == event).unwrap()["duration_ms"]
        .as_f64()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn every_model_call_records_its_latency_and_outcome_apart_from_what_the_user_perceives() {
    LocalSet::new()
        .run_until(async {
            let all = RecordKinds {
                metrics: true,
                text: true,
                audio: true,
            };

            let dir = record_dir("accepted");
            let (url, _uploads) =
                serve(Reply::Json(r#"{"text": "Polished words paste that"}"#)).await;
            let recorder = Recorder::in_dir(all, &dir).unwrap();
            let (mut rig, engine, frames) =
                Rig::recording(&with_secondary(&url, "open_ai"), recorder);
            let task = capture_task(engine, frames);
            live_words(&mut rig).await;
            assert_eq!(
                task.await.unwrap(),
                accepted(CaptureRoute::Paste, "Polished words")
            );
            let metrics = read_lines(&dir.join("metrics.jsonl"));
            let seen = events(&metrics);
            for expected in [
                ("desktop_wake_reset", "ok"),
                ("desktop_wake_inference", "capture"),
                ("desktop_wake_level", "quiet"),
                ("desktop_ready_cue", "ok"),
                ("desktop_transcription_ready", "ok"),
                ("desktop_cue", "ok"),
                ("desktop_ending_state", "pending"),
                ("desktop_ending_state", "accepted"),
                ("desktop_ending_pause", "ok"),
                ("desktop_end_cue", "ok"),
                ("desktop_secondary_stt", "ok"),
                ("desktop_capture_flow", "ok"),
            ] {
                assert!(
                    seen.contains(&(expected.0.to_owned(), expected.1.to_owned())),
                    "{expected:?} missing from {seen:?}"
                );
            }
            // The connection took no time; the wake cue the user waited through took 230 ms.
            assert!(duration_of(&metrics, "desktop_transcription_ready").abs() < 0.5);
            assert!((duration_of(&metrics, "desktop_ready_cue") - 230.0).abs() < 0.5);
            assert!((duration_of(&metrics, "desktop_ending_pause") - 2500.0).abs() < 0.5);
            let secondary = metrics
                .iter()
                .find(|line| line["event"] == "desktop_secondary_stt")
                .unwrap();
            assert_eq!(secondary["model"], "m");
            assert_eq!(secondary["api"], "openai");

            let day = std::fs::read_dir(dir.join("recordings"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let text = read_lines(&day.join("desktop.jsonl"));
            let kinds: Vec<&str> = text
                .iter()
                .map(|line| line["event"].as_str().unwrap())
                .collect();
            assert_eq!(kinds, ["final", "secondary", "accepted"]);
            assert_eq!(text[0]["text"], "live words paste that");
            assert_eq!(text[2]["text"], "Polished words");
            let wav = std::fs::read_dir(&day)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|ext| ext == "wav"))
                .unwrap();
            assert_eq!(
                std::fs::metadata(wav).unwrap().len(),
                44 + 2560 * u64::try_from(rig.sent().len()).unwrap(),
                "only audio sent after the cue is recorded"
            );

            let dir = record_dir("failed");
            let recorder = Recorder::in_dir(all, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            let task = capture_task(engine, frames);
            rig.wake_word().await;
            rig.server
                .fail("Thalvi could not reach speech recognition.");
            assert!(matches!(task.await.unwrap(), CaptureOutcome::Failed { .. }));
            let seen = events(&read_lines(&dir.join("metrics.jsonl")));
            for expected in [
                ("desktop_transcription_ready", "error"),
                ("desktop_end_cue", "ok"),
                ("desktop_capture_flow", "failed"),
            ] {
                assert!(
                    seen.contains(&(expected.0.to_owned(), expected.1.to_owned())),
                    "{expected:?} missing from {seen:?}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_new_ending_withholds_audio_again_after_speech_was_relayed() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello submit").await;
            rig.feed_until(1760, Kind::Quiet).await;
            rig.feed_until(2000, Kind::Voice).await;
            rig.finals("submit").await;
            let sent = rig.sent().len();
            rig.feed_until(2640, Kind::Quiet).await;
            assert_eq!(rig.sent().len(), sent, "quiet audio is held again");
            rig.handle.cancel();
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn quiet_hiss_does_not_count_as_speech_to_a_pending_ending() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(880, Kind::Quiet).await;
            rig.feed_until(1000, Kind::Voice).await;
            rig.finals("Hello paste that").await;
            let waiting = |waiting_for_quiet| EngineState::PendingEnding {
                route: CaptureRoute::Paste,
                text: "Hello".to_owned(),
                waiting_for_quiet,
            };
            assert_eq!(rig.state(), waiting(true));
            rig.feed_until(1360, Kind::Hiss).await;
            assert_eq!(rig.state(), waiting(false));
            rig.handle.cancel();
            assert_eq!(task.await.unwrap(), CaptureOutcome::Cancelled);
        })
        .await;
}

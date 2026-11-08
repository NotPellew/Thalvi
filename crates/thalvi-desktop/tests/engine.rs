//! The capture engine on a paused clock, with a fake microphone, wake word, cues and recognizer.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]
#![expect(clippy::unwrap_used, reason = "a test fails by panicking")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use serde_json::json;
use thalvi_desktop::audio::{AudioError, InputGate};
use thalvi_desktop::capture::{CaptureRequest, CaptureRoute};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::cues::{Cue, CueTiming};
use thalvi_desktop::engine::fakes::{
    CueControl, CueLog, FakeCuePlayer, FakeFrames, FakeWake, FrameFeed, WakeControl,
};
use thalvi_desktop::engine::{
    CaptureError, CaptureOutcome, Engine, EngineHandle, EngineState, ListenError, Parts,
    WakeTrigger,
};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::hold::{HoldInput, Refusal};
use thalvi_desktop::metrics::{RecordKinds, Recorder};
use thalvi_desktop::recognizer::{
    FakeCall, FakeControl, FakeRecognizer, RecognizerError, RecognizerEvent, ServerUrl,
    SpeechSource,
};
use thalvi_desktop::vad::VadDetector;
use thalvi_desktop::wake::WakeEvent;
use tokio::task::{JoinHandle, LocalSet};

type TestEngine = Engine<FakeRecognizer, FakeWake, FakeCuePlayer>;

const FRAME_MS: u64 = 80;

/// The paste pause the countdown tests below were written around, longer than the default.
const SLOW_PASTE: &str = r#"{"ending_pause": {"paste": 2.5}}"#;

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

/// A frame numbered like [`quiet`] with one sample of `peak`.
fn peaking(id: usize, peak: i16) -> Frame {
    let mut frame = quiet(id);
    frame[1] = peak;
    frame
}

/// A frame loud enough for a wake on it to count.
fn audible(id: usize) -> Frame {
    peaking(id, 1_500)
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
    /// The dictation key.
    key: HoldInput,
}

impl Rig {
    fn new(config: &str) -> (Self, TestEngine, FakeFrames) {
        Self::recording(config, Recorder::disabled())
    }

    fn recording(config: &str, recorder: Recorder) -> (Self, TestEngine, FakeFrames) {
        let config = DesktopConfig::from_json_str(config).unwrap();
        let gate = InputGate::default();
        let (frames, feed) = FakeFrames::new(&gate);
        let (wake_detector, wake) = FakeWake::new();
        let (cue_player, cues) = FakeCuePlayer::new();
        let (recognizer, server) = FakeRecognizer::new();
        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&notices);
        let (key, hold) = HoldInput::new();
        let (engine, handle) = Engine::new(
            Parts {
                recognizer,
                wake: wake_detector,
                cues: cue_player,
                gate: gate.clone(),
                http: reqwest::Client::new(),
                hold,
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
                key,
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
        self.feed.send(audible(0));
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

/// The result of `task`, failing instead of waiting forever when it never ends.
async fn finished<T>(task: JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(120), task)
        .await
        .expect("the task never ended")
        .unwrap()
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
async fn the_microphone_level_follows_the_capture_frames_and_is_zero_outside_it() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let level = rig.handle.level();
            let task = capture_task(engine, frames);
            rig.open().await;
            assert_eq!(*level.borrow(), 0.0, "nothing is loud before a frame");

            // -30 dBFS sits 60% of the way from -60 dBFS to -10 dBFS.
            rig.feed.send([1036; FRAME_SAMPLES]);
            settle().await;
            assert!((*level.borrow() - 0.6).abs() < 0.01, "{}", *level.borrow());
            rig.feed.send([i16::MAX; FRAME_SAMPLES]);
            settle().await;
            assert_eq!(*level.borrow(), 1.0);
            rig.feed.send(quiet(1));
            settle().await;
            assert_eq!(*level.borrow(), 0.0);

            rig.feed.send([i16::MAX; FRAME_SAMPLES]);
            settle().await;
            rig.finals("never mind").await;
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );
            assert_eq!(*level.borrow(), 0.0, "the level ends with the capture");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_microphone_level_follows_wake_listening_and_resets_when_it_stops() {
    LocalSet::new()
        .run_until(async {
            let (rig, mut engine, mut frames) = Rig::new("{}");
            let level = rig.handle.level();
            let task = tokio::task::spawn_local(async move { engine.listen(&mut frames).await });
            rig.feed.send([i16::MAX; FRAME_SAMPLES]);
            settle().await;
            assert_eq!(*level.borrow(), 1.0, "listening frames move the level");
            rig.feed.send(quiet(1));
            settle().await;
            assert_eq!(*level.borrow(), 0.0);
            rig.feed.send([1036; FRAME_SAMPLES]);
            settle().await;
            assert!((*level.borrow() - 0.6).abs() < 0.01, "{}", *level.borrow());
            rig.feed.fail(AudioError::DeviceLost);
            let error = finished(task).await.unwrap_err();
            assert!(matches!(error, ListenError::Audio(AudioError::DeviceLost)));
            assert_eq!(
                *level.borrow(),
                0.0,
                "the level resets when the microphone stops"
            );
        })
        .await;
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );
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
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Cancelled { dictated: None }
                );
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
            commit: 2000 + 700,
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
            commit: 1400 + 700,
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
            commit: 1840 + 700,
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
            // Noise without words delays the commit: the pause restarts at the first quiet frame.
            commit: 1840 + 700,
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
                    finished(task).await,
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
                finished(task).await,
                accepted(CaptureRoute::Submit, "Hello submit more words")
            );
            assert_eq!(rig.started(Cue::Provisional), [1000, 3000]);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn local_speech_stops_the_countdown_until_quiet_returns_unless_words_cancel_the_ending() {
    // What the user does after "paste that" at 1000, and, in its last step, 1 ms before the paste is accepted.
    let cases: [(&str, &[(u64, Kind)]); 3] = [
        // Observed on Hyprland: talking from 1.5 s, no partial before the old 2.5 s countdown ended.
        (
            "talking",
            &[
                (2480, Kind::Quiet),
                (3400, Kind::Voice),
                (5939, Kind::Quiet),
            ],
        ),
        // A cough: three speech frames, no words.
        (
            "cough",
            &[
                (2000, Kind::Quiet),
                (2240, Kind::Voice),
                (4819, Kind::Quiet),
            ],
        ),
        // Speaking again after a short quiet gap pauses the countdown again.
        (
            "resuming",
            &[
                (2000, Kind::Quiet),
                (2240, Kind::Voice),
                (3000, Kind::Quiet),
                (3400, Kind::Voice),
                (5939, Kind::Quiet),
            ],
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (name, script) in cases {
                let (mut rig, engine, frames) = Rig::new(SLOW_PASTE);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals("Hello paste that").await;
                let pending = |waiting_for_quiet| EngineState::PendingEnding {
                    route: CaptureRoute::Paste,
                    text: "Hello".to_owned(),
                    waiting_for_quiet,
                };
                for (until, kind) in script {
                    rig.feed_until(*until, *kind).await;
                    assert!(!task.is_finished(), "{name}: still pending at {until}");
                    if matches!(kind, Kind::Voice) {
                        assert_eq!(rig.state(), pending(true), "{name}: speech at {until}");
                    }
                }
                assert_eq!(rig.state(), pending(false), "{name}: counting again");
                // The script ends 1 ms before the pause does.
                rig.at(script[script.len() - 1].0 + 1).await;
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Paste, "Hello"),
                    "{name}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn words_recognized_while_the_countdown_waits_for_quiet_still_cancel_the_ending() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new(SLOW_PASTE);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.feed_until(2480, Kind::Quiet).await;
            rig.feed_until(3000, Kind::Voice).await;
            rig.partial("and also").await;
            assert_eq!(rig.started(Cue::Cancelled), [3000]);
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: "Hello paste that".to_owned(),
                    partial: Some("and also".to_owned())
                }
            );
            rig.feed_until(6000, Kind::Quiet).await;
            assert!(!task.is_finished(), "the cancelled ending is not delivered");
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello paste that".to_owned())
                }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_paste_still_yields_to_speech_inside_its_short_window() {
    LocalSet::new()
        .run_until(async {
            // Local speech pauses the countdown; the paste waits for quiet and the full 0.7 s again.
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.feed_until(1200, Kind::Quiet).await;
            rig.feed_until(1600, Kind::Voice).await;
            rig.feed_until(2400, Kind::Quiet).await;
            assert!(
                !task.is_finished(),
                "speech at 1.2 s moved the paste past 1.7 s"
            );
            rig.feed_until(3000, Kind::Quiet).await;
            assert_eq!(finished(task).await, accepted(CaptureRoute::Paste, "Hello"));
            assert!(rig.started(Cue::Accepted)[0] > 2400);

            // Recognized words cancel the pending paste.
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.feed_until(1300, Kind::Quiet).await;
            rig.partial("and also").await;
            assert_eq!(rig.started(Cue::Cancelled), [1300]);
            rig.feed_until(3000, Kind::Quiet).await;
            assert!(!task.is_finished(), "the cancelled paste is not delivered");
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello paste that".to_owned())
                }
            );
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello submit".to_owned())
                }
            );
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Draft words".to_owned())
                }
            );
            assert_eq!(rig.started(Cue::Provisional).len(), 0);
            assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel));
            assert!(rig.gate.is_enabled());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_spoken_never_mind_keeps_what_was_dictated_before_it_without_the_phrase() {
    LocalSet::new()
        .run_until(async {
            for (finals, dictated) in [
                (
                    &["Second check that I drop. Never mind."][..],
                    Some("Second check that I drop."),
                ),
                (&["Draft words", "never mind"], Some("Draft words")),
                (
                    &["First part.", "Second part. Never mind."],
                    Some("First part. Second part."),
                ),
                (&["never mind"], None),
            ] {
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                for words in finals {
                    rig.finals(words).await;
                }
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Cancelled {
                        dictated: dictated.map(str::to_owned)
                    },
                    "{finals:?}"
                );
            }
        })
        .await;
}

/// Scripts for the commands that run without a spoken ending.
const COMMANDS: &str = r#"{
    "apps": {"editor": {"classes": ["Code"], "input_keys": ["ctrl", "l"]}},
    "scripts": [
        {"phrase": "open the project page", "action": "open_url",
         "destination": "https://github.com/NotPellew/Thalvi", "label": "the project page"},
        {"phrase": "switch to editor", "action": "focus_app", "destination": "editor"},
        {"phrase": "message the demo channel saying", "action": "discord_message",
         "destination": "https://discord.com/channels/1/2", "label": "Demo"}
    ]
}"#;

fn pending_task(text: &str, waiting_for_quiet: bool) -> EngineState {
    EngineState::PendingEnding {
        route: CaptureRoute::Task,
        text: text.to_owned(),
        waiting_for_quiet,
    }
}

#[tokio::test(start_paused = true)]
async fn a_whole_capture_command_runs_after_the_task_pause_and_an_explicit_ending_still_wins() {
    // (what is recognized and when, the route and text delivered, the provisional cues, when it is delivered)
    type Case = (
        &'static [(u64, &'static str)],
        CaptureRoute,
        &'static str,
        &'static [u64],
        u64,
    );
    let cases: [Case; 9] = [
        (
            &[(1000, "open google.com")],
            CaptureRoute::Task,
            "open google.com",
            &[1000],
            3500,
        ),
        (
            &[(1000, "Open Google.com.")],
            CaptureRoute::Task,
            "Open Google.com.",
            &[1000],
            3500,
        ),
        (
            &[(1000, "open the project page")],
            CaptureRoute::Task,
            "open the project page",
            &[1000],
            3500,
        ),
        (
            &[(1000, "switch to editor")],
            CaptureRoute::Task,
            "switch to editor",
            &[1000],
            3500,
        ),
        (
            &[(1000, "open"), (1400, "google.com")],
            CaptureRoute::Task,
            "open google.com",
            &[1400],
            3900,
        ),
        (
            &[(1000, "open google.com paste that")],
            CaptureRoute::Paste,
            "open google.com",
            &[1000],
            1700,
        ),
        (
            &[(1000, "open google.com submit")],
            CaptureRoute::Submit,
            "open google.com",
            &[1000],
            3500,
        ),
        (
            &[(1000, "open google.com do that")],
            CaptureRoute::Task,
            "open google.com",
            &[1000],
            3500,
        ),
        // The command is armed, then "do that" is said on its own: the spoken ending starts a fresh pause.
        (
            &[(1000, "open google.com"), (1200, "do that")],
            CaptureRoute::Task,
            "open google.com",
            &[1000, 1200],
            3700,
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (finals, route, text, provisional_at, commit) in cases {
                let (mut rig, engine, frames) = Rig::new(COMMANDS);
                let task = capture_task(engine, frames);
                rig.open().await;
                for (at, said) in finals {
                    rig.feed_until(*at, Kind::Quiet).await;
                    rig.partial(said).await;
                    rig.finals(said).await;
                }
                if route == CaptureRoute::Task && finals.len() == 1 {
                    assert_eq!(rig.state(), pending_task(text, false), "{finals:?}");
                }
                rig.feed_until(commit - 1, Kind::Quiet).await;
                assert!(!task.is_finished(), "{finals:?}: delivered early");
                rig.at(commit).await;
                assert_eq!(finished(task).await, accepted(route, text), "{finals:?}");
                assert_eq!(rig.started(Cue::Accepted), [commit], "{finals:?}");
                assert_eq!(rig.started(Cue::Provisional), provisional_at, "{finals:?}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn speech_during_a_command_pause_cancels_it_and_the_capture_goes_on_as_dictation() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new(COMMANDS);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("open google.com").await;
            rig.feed_until(2000, Kind::Quiet).await;
            rig.partial("and then").await;
            assert_eq!(rig.started(Cue::Cancelled), [2000]);
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: "open google.com".to_owned(),
                    partial: Some("and then".to_owned())
                }
            );
            rig.feed_until(6000, Kind::Quiet).await;
            assert!(!task.is_finished(), "the cancelled command is not run");
            rig.finals("and then paste that").await;
            rig.feed_until(6699, Kind::Quiet).await;
            assert!(!task.is_finished());
            rig.at(6700).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "open google.com and then")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn local_speech_pauses_a_command_countdown_until_quiet_returns() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new(COMMANDS);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("open google.com").await;
            rig.feed_until(2000, Kind::Quiet).await;
            rig.feed_until(2240, Kind::Voice).await;
            assert_eq!(rig.state(), pending_task("open google.com", true));
            rig.feed_until(4819, Kind::Quiet).await;
            assert!(
                !task.is_finished(),
                "the pause restarted when quiet returned"
            );
            assert_eq!(rig.state(), pending_task("open google.com", false));
            rig.at(4820).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Task, "open google.com")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn never_mind_drops_a_command_that_is_waiting_out_its_pause() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new(COMMANDS);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("open google.com").await;
            rig.feed_until(1500, Kind::Quiet).await;
            assert_eq!(rig.state(), pending_task("open google.com", false));
            rig.finals("never mind").await;
            assert_eq!(rig.started(Cue::Cancelled), [1500]);
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("open google.com".to_owned())
                }
            );
            assert_eq!(rig.started(Cue::Accepted).len(), 0);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn text_that_is_not_the_whole_command_never_runs_without_an_ending() {
    LocalSet::new()
        .run_until(async {
            for said in [
                "I said open google.com yesterday",
                "open the file",
                "open google.com and then some",
                "message the demo channel saying hello",
            ] {
                let (mut rig, engine, frames) = Rig::new(COMMANDS);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals(said).await;
                rig.feed_until(8000, Kind::Quiet).await;
                assert!(!task.is_finished(), "{said}: nothing runs by itself");
                assert_eq!(rig.started(Cue::Provisional).len(), 0, "{said}");
                assert_eq!(
                    rig.state(),
                    EngineState::Capturing {
                        committed: said.to_owned(),
                        partial: None
                    },
                    "{said}"
                );
                rig.handle.cancel();
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Cancelled {
                        dictated: Some(said.to_owned())
                    },
                    "{said}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_task_that_ran_without_an_ending_is_recorded_apart_from_one_that_had_one() {
    LocalSet::new()
        .run_until(async {
            let all = RecordKinds {
                metrics: true,
                text: false,
                audio: false,
            };
            for (said, states) in [
                (
                    "open google.com",
                    ["pending_without_ending", "accepted_without_ending"],
                ),
                ("open google.com do that", ["pending", "accepted"]),
            ] {
                let dir = record_dir(&format!("without-ending-{}", said.len()));
                let recorder = Recorder::in_dir(all, &dir).unwrap();
                let (mut rig, engine, frames) = Rig::recording(COMMANDS, recorder);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals(said).await;
                rig.feed_until(3500, Kind::Quiet).await;
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Task, "open google.com"),
                    "{said}"
                );
                let seen: Vec<String> = events(&read_lines(&dir.join("metrics.jsonl")))
                    .into_iter()
                    .filter(|(event, _)| event == "desktop_ending_state")
                    .map(|(_, outcome)| outcome)
                    .collect();
                assert_eq!(seen, states, "{said}");
            }
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
                let (mut rig, engine, frames) = Rig::new(SLOW_PASTE);
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
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Cancelled {
                        dictated: (!matches!(moment, Moment::Starting)).then(|| "Hello".to_owned())
                    }
                );
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
            assert_eq!(finished(task).await, accepted(CaptureRoute::Paste, "Hello"));
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
                Fault::RecognizerDrops => rig.server.emit(Err(RecognizerError::said(DISCONNECTED))),
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
    let recognizer = |message: &str| CaptureError::Recognizer(RecognizerError::said(message));
    let not_ready = || CaptureError::NotReady {
        waited: Duration::from_secs(8),
        speech: SpeechSource::Server(ServerUrl::default()),
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
        (Fault::NeverReady, not_ready(), None, 8000, false),
        (Fault::WakeCueTooLong, not_ready(), None, 8000, true),
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
                    finished(task).await,
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
            let (mut rig, engine, frames) = Rig::new(SLOW_PASTE);
            rig.cues.set_length(Cue::Provisional, ms(1000));
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.at(1300).await;
            assert!(!task.is_finished(), "no timeout while the cue plays");
            rig.at(2000).await;
            rig.at(2249).await;
            assert!(!task.is_finished(), "the cue ended at 2000");
            rig.at(2250).await;
            assert_eq!(rig.started(Cue::Error), [2250]);
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: CaptureError::MicStopped,
                    recoverable_text: Some("Hello".to_owned())
                }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_capture_fails_when_the_microphone_gives_no_audio_for_three_seconds() {
    LocalSet::new()
        .run_until(async {
            // Unplugged mid-capture: the last frame arrived at 960.
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello world").await;
            rig.at(3959).await;
            assert!(!task.is_finished(), "2999 ms without audio");
            rig.at(3960).await;
            assert_eq!(rig.started(Cue::Error), [3960]);
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: CaptureError::MicStopped,
                    recoverable_text: Some("Hello world".to_owned())
                }
            );

            // A gap just under the limit, then audio again, is only a pause.
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.at(3859).await;
            rig.feed.send(quiet(1));
            rig.feed_until(8000, Kind::Quiet).await;
            assert!(!task.is_finished());
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );

            // Cues mute the microphone, so the limit starts when the cue ends.
            let (mut rig, engine, frames) = Rig::new("{}");
            rig.cues.set_length(Cue::Cancelled, ms(5000));
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello submit").await;
            rig.at(1100).await;
            rig.finals("and more").await;
            assert_eq!(rig.started(Cue::Cancelled), [1100]);
            rig.at(6099).await;
            assert!(!task.is_finished(), "the cue is still playing");
            rig.at(9099).await;
            assert!(!task.is_finished(), "3 s after the cue ended is not yet");
            rig.at(9100).await;
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: CaptureError::MicStopped,
                    recoverable_text: Some("Hello submit and more".to_owned())
                }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_capture_without_words_for_a_minute_ends_and_keeps_its_text() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello world").await;
            rig.feed_until(40_000, Kind::Quiet).await;
            rig.partial("more").await;
            rig.feed_until(70_000, Kind::Quiet).await;
            rig.partial("[noise]").await;
            rig.feed_until(90_000, Kind::Quiet).await;
            rig.partial("uh").await;
            rig.feed_until(99_960, Kind::Quiet).await;
            assert!(
                !task.is_finished(),
                "words at 40 s restart the minute; noise does not"
            );
            rig.feed_until(100_080, Kind::Quiet).await;
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: CaptureError::NoEnding(Duration::from_secs(60)),
                    recoverable_text: Some("Hello world".to_owned())
                }
            );
        })
        .await;
}

/// Milliseconds after the wake word at which the capture opens; the five-minute cap counts from here.
const CAPTURE_OPENS: u64 = 230;

#[tokio::test(start_paused = true)]
async fn a_capture_that_keeps_hearing_words_is_stopped_after_five_minutes_and_keeps_its_text() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            let words: Vec<String> = (1..=9).map(|n| format!("word{n}")).collect();
            for (n, word) in (1..).zip(&words) {
                rig.feed_until(n * 30_000, Kind::Quiet).await;
                rig.finals(word).await;
            }
            rig.feed_until(CAPTURE_OPENS + 299_920, Kind::Quiet).await;
            assert!(!task.is_finished(), "words keep the idle limit away");
            assert_eq!(rig.started(Cue::Error).len(), 0);

            rig.at(CAPTURE_OPENS + 300_000).await;
            assert_eq!(rig.started(Cue::Error), [CAPTURE_OPENS + 300_000]);
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: CaptureError::TooLong(Duration::from_secs(300)),
                    recoverable_text: Some(words.join(" "))
                }
            );
            assert_eq!(
                CaptureError::TooLong(Duration::from_secs(300)).to_string(),
                "Capture stopped after 5 minutes without an ending."
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn an_ending_still_pending_when_the_five_minutes_pass_is_not_delivered() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            for n in 1..=5 {
                rig.feed_until(n * 50_000, Kind::Quiet).await;
                rig.finals("more").await;
            }
            rig.feed_until(299_000, Kind::Voice).await;
            rig.finals("Almost done submit").await;
            rig.feed_until(CAPTURE_OPENS + 299_920, Kind::Voice).await;
            assert!(!task.is_finished(), "speech keeps the countdown waiting");

            rig.at(CAPTURE_OPENS + 300_000).await;
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: CaptureError::TooLong(Duration::from_secs(300)),
                    recoverable_text: Some("more more more more more Almost done".to_owned())
                }
            );
            assert_eq!(rig.started(Cue::Accepted).len(), 0);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn configured_capture_limits_replace_the_defaults() {
    LocalSet::new()
        .run_until(async {
            let config = r#"{"capture_limits": {"idle": 20, "max": 90}}"#;

            let (mut rig, engine, frames) = Rig::new(config);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(10_000, Kind::Quiet).await;
            rig.finals("Hello").await;
            rig.feed_until(29_920, Kind::Quiet).await;
            assert!(!task.is_finished());
            rig.at(30_000).await;
            let idle = CaptureError::NoEnding(Duration::from_secs(20));
            assert_eq!(idle.to_string(), "No words heard for 20 seconds.");
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: idle,
                    recoverable_text: Some("Hello".to_owned())
                }
            );

            let (mut rig, engine, frames) = Rig::new(config);
            let task = capture_task(engine, frames);
            rig.open().await;
            for n in 1..=5 {
                rig.feed_until(n * 15_000, Kind::Quiet).await;
                rig.finals("more").await;
            }
            rig.feed_until(CAPTURE_OPENS + 89_920, Kind::Quiet).await;
            assert!(!task.is_finished());
            rig.at(CAPTURE_OPENS + 90_000).await;
            let long = CaptureError::TooLong(Duration::from_secs(90));
            assert_eq!(
                long.to_string(),
                "Capture stopped after 90 seconds without an ending."
            );
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: long,
                    recoverable_text: Some("more more more more more".to_owned())
                }
            );
        })
        .await;
}

#[test]
fn the_time_notices_read_whole_minutes_as_minutes_and_the_rest_as_seconds() {
    for (seconds, words) in [
        (10, "10 seconds"),
        (60, "1 minute"),
        (90, "90 seconds"),
        (120, "2 minutes"),
        (600, "10 minutes"),
        (3600, "60 minutes"),
    ] {
        let seconds = Duration::from_secs(seconds);
        assert_eq!(
            CaptureError::NoEnding(seconds).to_string(),
            format!("No words heard for {words}.")
        );
        assert_eq!(
            CaptureError::TooLong(seconds).to_string(),
            format!("Capture stopped after {words} without an ending.")
        );
    }
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
                rig.feed.send(audible(1));
                settle().await;
            }
            assert_eq!(rig.notices(), ["Wake was ambiguous; still listening."]);
            assert!(!task.is_finished());
            rig.feed.send(audible(2));
            settle().await;
            rig.wake.fire(WakeEvent::Ambiguous);
            rig.feed.send(audible(3));
            settle().await;
            assert_eq!(rig.notices().len(), 2, "a quiet frame re-arms the notice");

            rig.wake.fire(WakeEvent::Conversation);
            rig.feed.send(audible(4));
            settle().await;
            assert!(
                rig.gate.is_enabled(),
                "a conversation wake leaves the microphone on"
            );
            rig.wake.fire(WakeEvent::Capture);
            rig.feed.send(audible(5));
            let (first, second) = finished(task).await;
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
                    rig.feed.send(audible(1));
                } else {
                    rig.feed.fail(AudioError::DeviceLost);
                }
                let error = finished(task).await.unwrap_err();
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
    format!(
        r#"{{"ending_pause": {{"paste": 2.5}}, "stt": {{"secondary": {{"enabled": true, "url": "{url}", "model": "m", "api": "{api}"}}}}}}"#
    )
}

fn with_secondary_off(url: &str) -> String {
    format!(
        r#"{{"ending_pause": {{"paste": 2.5}}, "stt": {{"secondary": {{"enabled": false, "url": "{url}", "model": "m", "api": "openai"}}}}}}"#
    )
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
                ("openai", r#"{"text": "Polished words paste that [noise]"}"#),
                (
                    "meta",
                    r#"{"transcript": "Polished words paste that [noise]"}"#,
                ),
                // The second pass heard the ending twice; the user said it once.
                (
                    "meta",
                    r#"{"transcript": "Polished words paste that. Paste that."}"#,
                ),
            ] {
                let (url, mut uploads) = serve(Reply::Json(reply)).await;
                let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, api));
                let task = capture_task(engine, frames);
                live_words(&mut rig).await;
                assert_eq!(
                    finished(task).await,
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
async fn no_secondary_call_or_working_step_when_the_switch_is_off() {
    LocalSet::new()
        .run_until(async {
            for config_of in [
                with_secondary_off as fn(&str) -> String,
                (|url: &str| {
                    format!(
                        r#"{{"ending_pause": {{"paste": 2.5}}, "stt": {{"secondary": {{"url": "{url}", "model": "m", "api": "openai"}}}}}}"#
                    )
                }) as fn(&str) -> String,
            ] {
                let (url, mut uploads) =
                    serve(Reply::Json(r#"{"text": "Polished words paste that"}"#)).await;
                let (mut rig, engine, frames) = Rig::new(&config_of(&url));
                let task = capture_task(engine, frames);
                live_words(&mut rig).await;
                assert!(
                    !matches!(rig.state(), EngineState::TranscribingSecondary { .. }),
                    "no secondary working step"
                );
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Paste, "live words")
                );
                assert!(uploads.try_recv().is_err(), "no secondary request");
                assert_eq!(rig.notices().len(), 0);
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
                let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, "openai"));
                let task = capture_task(engine, frames);
                live_words(&mut rig).await;
                assert_eq!(
                    finished(task).await,
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
            let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, "openai"));
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("live words".to_owned())
                }
            );
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
            let (mut rig, engine, frames) = Rig::new(&with_secondary(&url, "openai"));
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
                finished(task).await,
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello paste that".to_owned())
                }
            );
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
                Rig::recording(&with_secondary(&url, "openai"), recorder);
            let task = capture_task(engine, frames);
            live_words(&mut rig).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "Polished words")
            );
            let metrics = read_lines(&dir.join("metrics.jsonl"));
            let seen = events(&metrics);
            for expected in [
                ("desktop_wake_reset", "ok"),
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

            let folder = dictation_dirs(&dir).pop().unwrap();
            assert_eq!(
                std::fs::read_to_string(folder.join("text.txt")).unwrap(),
                "heard: live words paste that\n\
                 second transcription: Polished words paste that\n\
                 accepted (paste): Polished words\n"
            );
            assert_eq!(
                std::fs::metadata(folder.join("speech.wav")).unwrap().len(),
                44 + 2560 * u64::try_from(rig.sent().len()).unwrap(),
                "only audio sent after the cue is recorded"
            );
            assert!(folder.join("wake.wav").exists());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_capture_records_which_kind_of_failure_and_its_calls() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("failed");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            let task = capture_task(engine, frames);
            rig.wake_word().await;
            rig.server
                .fail("Thalvi could not reach speech recognition.");
            assert!(matches!(
                finished(task).await,
                CaptureOutcome::Failed { .. }
            ));
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let flow = lines
                .iter()
                .find(|line| line["event"] == "desktop_capture_flow")
                .unwrap();
            assert_eq!(flow["reason"], "recognizer", "a failure says which kind");
            let seen = events(&lines);
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

const METRICS_ONLY: RecordKinds = RecordKinds {
    metrics: true,
    text: false,
    audio: false,
};

const AUDIO_AND_METRICS: RecordKinds = RecordKinds {
    metrics: true,
    text: false,
    audio: true,
};

/// Listens until a capture or conversation wake, then writes the recordings out.
fn listen_task(
    mut engine: TestEngine,
    mut frames: FakeFrames,
    listens: usize,
) -> JoinHandle<Vec<WakeTrigger>> {
    tokio::task::spawn_local(async move {
        let mut triggers = Vec::new();
        for _ in 0..listens {
            triggers.push(engine.listen(&mut frames).await.unwrap());
        }
        engine.finish().await;
        triggers
    })
}

/// The dictation folders (`<date>/<time>`) under `root`, in name order.
fn dictation_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let subdirs = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut paths: Vec<_> = entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        paths.sort();
        paths
    };
    subdirs(root).iter().flat_map(|day| subdirs(day)).collect()
}

/// Every file under `root` as a path with the date and time elided.
fn files_under(root: &std::path::Path) -> Vec<String> {
    let mut files: Vec<String> = dictation_dirs(root)
        .iter()
        .flat_map(|folder| {
            std::fs::read_dir(folder).unwrap().map(|entry| {
                format!(
                    "<dictation>/{}",
                    entry.unwrap().file_name().to_string_lossy()
                )
            })
        })
        .collect();
    if root.join("metrics.jsonl").exists() {
        files.push("metrics.jsonl".to_owned());
    }
    files.sort();
    files
}

/// The `wake.wav` of each dictation folder as (folder name, samples).
fn wake_clips(root: &std::path::Path) -> Vec<(String, Vec<i16>)> {
    dictation_dirs(root)
        .iter()
        .filter_map(|folder| {
            let bytes = std::fs::read(folder.join("wake.wav")).ok()?;
            let samples = bytes[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair))
                .collect();
            let name = format!(
                "{}/{}",
                folder
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
                folder.file_name().unwrap().to_string_lossy()
            );
            Some((name, samples))
        })
        .collect()
}

/// Sends frames tagged `from..=to`, none of them a wake.
async fn hear(rig: &Rig, from: usize, to: usize) {
    for id in from..=to {
        rig.feed.send(audible(id));
        settle().await;
    }
}

fn wake_lines(dir: &std::path::Path, event: &str) -> Vec<serde_json::Value> {
    read_lines(&dir.join("metrics.jsonl"))
        .into_iter()
        .filter(|line| line["event"] == event)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_near_miss_is_one_line_per_episode_with_its_peak_and_a_crossing_is_none() {
    LocalSet::new()
        .run_until(async {
            // Per frame (capture, conversation); both thresholds are set to 0.5.
            let frames = [
                (0.0, 0.0),
                (0.1, 0.0),
                (0.2, 0.0),
                (0.3004, 0.0),
                (0.2, 0.0),
                (0.0, 0.0),
                (0.15, 0.0),
                (0.1, 0.4),
                (0.0, 0.1234),
                (0.0, 0.0),
                (0.3, 0.0),
            ];
            let dir = record_dir("near-miss");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let config = r#"{"wake":{"capture":{"model":"hey_thalvi","threshold":0.5}}}"#;
            let (rig, engine, frames_in) = Rig::recording(config, recorder);
            let task = listen_task(engine, frames_in, 1);
            for (id, (capture, conversation)) in frames.into_iter().enumerate() {
                rig.wake.score(capture, conversation);
                hear(&rig, id, id).await;
            }
            rig.wake.score(0.6, 0.0);
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 99, 99).await;
            finished(task).await;

            let misses: Vec<_> = wake_lines(&dir, "desktop_wake_near_miss")
                .iter()
                .map(|line| {
                    (
                        line["outcome"].as_str().unwrap().to_owned(),
                        line["peak"].clone(),
                        line["threshold"].clone(),
                    )
                })
                .collect();
            assert_eq!(
                misses,
                [
                    ("capture".to_owned(), json!(0.3), json!(0.5)),
                    ("capture".to_owned(), json!(0.15), json!(0.5)),
                    ("conversation".to_owned(), json!(0.4), json!(0.5)),
                ],
                "a rise that ends in a wake is not a near miss"
            );
            assert!(
                wake_lines(&dir, "desktop_wake_inference")
                    .iter()
                    .all(|line| line["outcome"] == "summary"),
                "no line per frame"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_wake_needs_a_sample_peak_of_1500_within_the_last_16_frames() {
    LocalSet::new()
        .run_until(async {
            // (name, event, peak of the frame k frames before the wake frame (0 = the wake
            // frame), the peak elsewhere, the peak the refusal records or None for a wake)
            type Case = (&'static str, WakeEvent, usize, i16, i16, Option<u16>);
            let cases: [Case; 7] = [
                ("faint", WakeEvent::Capture, 0, 1_499, 0, Some(1_499)),
                ("at the floor", WakeEvent::Capture, 0, 1_500, 0, None),
                (
                    "negative faint conversation",
                    WakeEvent::Conversation,
                    0,
                    -1_499,
                    0,
                    Some(1_499),
                ),
                (
                    "negative at the floor",
                    WakeEvent::Capture,
                    0,
                    -1_500,
                    0,
                    None,
                ),
                (
                    "faint ambiguous",
                    WakeEvent::Ambiguous,
                    0,
                    1_499,
                    0,
                    Some(1_499),
                ),
                (
                    "loud 15 frames back",
                    WakeEvent::Capture,
                    15,
                    20_000,
                    100,
                    None,
                ),
                (
                    "loud 16 frames back",
                    WakeEvent::Capture,
                    16,
                    20_000,
                    100,
                    Some(100),
                ),
            ];
            for (name, event, back, peak, elsewhere, refused) in cases {
                let dir = record_dir("wake-floor");
                let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
                let (rig, engine, frames) = Rig::recording("{}", recorder);
                let task = listen_task(engine, frames, 1);
                rig.wake.score(0.9, 0.1234);
                for id in 0..=20 {
                    let loud = 20 - id == back;
                    rig.feed
                        .send(peaking(id, if loud { peak } else { elsewhere }));
                    if id == 20 {
                        rig.wake.fire(event);
                    }
                    settle().await;
                }
                match refused {
                    None => assert!(finished(task).await.len() == 1, "{name}: the wake counts"),
                    Some(level) => {
                        assert!(!task.is_finished(), "{name}: the wake is refused");
                        assert!(rig.gate.is_enabled(), "{name}: the capture never began");
                        rig.wake.fire(event);
                        rig.feed.send(peaking(21, elsewhere));
                        settle().await;
                        assert!(!task.is_finished(), "{name}: still refused");
                        rig.wake.fire(WakeEvent::Conversation);
                        rig.feed.send(audible(22));
                        finished(task).await;
                        let lines = wake_lines(&dir, "desktop_wake_refused");
                        assert_eq!(lines.len(), 1, "{name}: one line for the run of frames");
                        assert_eq!(lines[0]["level"], json!(level), "{name}");
                        assert_eq!(lines[0]["floor"], json!(1_500), "{name}");
                        assert_eq!(lines[0]["capture"], json!(0.9), "{name}");
                        assert_eq!(lines[0]["conversation"], json!(0.123), "{name}");
                        let head = format!("{event:?}").to_lowercase();
                        assert_eq!(lines[0]["outcome"], head, "{name}");
                    }
                }
                if refused.is_none() {
                    assert!(
                        wake_lines(&dir, "desktop_wake_refused").is_empty(),
                        "{name}: a wake is no refusal"
                    );
                }
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn wake_latency_is_summarized_once_a_minute_and_when_listening_ends() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("latency");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (rig, engine, frames) = Rig::recording("{}", recorder);
            let task = listen_task(engine, frames, 1);
            for (id, took) in [10, 20, 30, 40].into_iter().enumerate() {
                rig.wake.inference(ms(took));
                hear(&rig, id, id).await;
            }
            tokio::time::advance(Duration::from_secs(59)).await;
            rig.wake.inference(ms(99));
            hear(&rig, 4, 4).await;
            rig.wake.inference(ms(50));
            tokio::time::advance(Duration::from_secs(1)).await;
            hear(&rig, 5, 5).await;
            rig.wake.inference(ms(7));
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 6, 6).await;
            finished(task).await;

            let summaries: Vec<_> = wake_lines(&dir, "desktop_wake_inference")
                .iter()
                .map(|line| {
                    (
                        line["count"].as_u64().unwrap(),
                        line["p50_ms"].as_f64().unwrap(),
                        line["p95_ms"].as_f64().unwrap(),
                        line["max_ms"].as_f64().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                summaries,
                [(6, 30.0, 99.0, 99.0), (1, 7.0, 7.0, 7.0)],
                "the first minute's frames, then what the wake cut short"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_minute_line_counts_seconds_of_sound_and_quiet_and_switches_between_them() {
    LocalSet::new()
        .run_until(async {
            // Per 2 s of microphone audio, whether it held sound.
            let cases: [(&str, &[bool], f64, f64, u64); 3] = [
                (
                    "several switches",
                    &[true, false, false, true, false],
                    4.0,
                    6.0,
                    3,
                ),
                ("one quiet stretch", &[false, false, false], 0.0, 6.0, 0),
                ("one loud stretch", &[true, true], 4.0, 0.0, 0),
            ];
            for (name, windows, sound_s, quiet_s, switches) in cases {
                let dir = record_dir("level");
                let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
                let (rig, engine, frames) = Rig::recording("{}", recorder);
                let task = listen_task(engine, frames, 1);
                for (index, sound) in windows.iter().enumerate() {
                    for id in 0..25 {
                        let id = index * 25 + id;
                        rig.feed.send(if *sound { voiced(id) } else { quiet(id) });
                        settle().await;
                    }
                }
                rig.wake.fire(WakeEvent::Capture);
                hear(&rig, 999, 999).await;
                finished(task).await;

                let lines = wake_lines(&dir, "desktop_wake_inference");
                let [summary] = lines.as_slice() else {
                    panic!("{name}: expected one summary line, got {lines:?}");
                };
                assert_eq!(
                    (
                        summary["sound_ms"].as_f64().unwrap(),
                        summary["quiet_ms"].as_f64().unwrap(),
                        summary["switches"].as_u64().unwrap(),
                    ),
                    (sound_s * 1000.0, quiet_s * 1000.0, switches),
                    "{name}"
                );
                assert!(
                    wake_lines(&dir, "desktop_wake_level").is_empty(),
                    "{name}: no line per sound or quiet switch"
                );
            }
        })
        .await;
}

const ALL_KINDS: RecordKinds = RecordKinds {
    metrics: true,
    text: true,
    audio: true,
};

#[tokio::test(start_paused = true)]
async fn each_wake_gets_a_folder_with_the_last_two_seconds_and_a_wake_nothing_ends_a_line() {
    LocalSet::new()
        .run_until(async {
            // (events from frame 31 on, id of the last frame of each folder's clip)
            type Case = (&'static str, &'static [WakeEvent], &'static [usize]);
            let cases: [Case; 3] = [
                ("capture", &[WakeEvent::Capture], &[31]),
                ("conversation", &[WakeEvent::Conversation], &[31]),
                (
                    "ambiguous",
                    &[
                        WakeEvent::Ambiguous,
                        WakeEvent::Ambiguous,
                        WakeEvent::Ambiguous,
                        WakeEvent::Capture,
                    ],
                    &[31, 34],
                ),
            ];
            for (name, events, expected) in cases {
                let dir = record_dir(name);
                let recorder = Recorder::in_dir(AUDIO_AND_METRICS, &dir).unwrap();
                let (rig, engine, frames) = Rig::recording("{}", recorder);
                let task = listen_task(engine, frames, 1);
                hear(&rig, 1, 30).await;
                rig.wake.score(0.8, 0.1234);
                for (offset, event) in events.iter().enumerate() {
                    rig.wake.fire(*event);
                    hear(&rig, 31 + offset, 31 + offset).await;
                }
                finished(task).await;

                let clips = wake_clips(&dir);
                let mut lasts: Vec<_> = clips
                    .iter()
                    .map(|(_, samples)| {
                        let tags: Vec<i16> = samples
                            .as_chunks::<FRAME_SAMPLES>()
                            .0
                            .iter()
                            .map(|frame| frame[0])
                            .collect();
                        assert_eq!(tags.len(), 25, "{name}");
                        assert_eq!(tags[24] - tags[0], 24, "{name}: the last 25 frames");
                        usize::try_from(tags[24]).unwrap()
                    })
                    .collect();
                lasts.sort_unstable();
                assert_eq!(lasts, expected, "{name}");

                let nothing = wake_lines(&dir, "desktop_wake_nothing");
                if name == "ambiguous" {
                    assert_eq!(nothing.len(), 1, "{name}: once per run of ambiguous frames");
                    assert_eq!(nothing[0]["outcome"], "ambiguous");
                    assert_eq!(nothing[0]["capture"], json!(0.8));
                    assert_eq!(nothing[0]["conversation"], json!(0.123));
                    assert!(
                        clips
                            .iter()
                            .any(|(folder, _)| nothing[0]["dictation"] == folder.as_str()),
                        "{name}: the line names the folder of its clip"
                    );
                } else {
                    assert!(
                        nothing.is_empty(),
                        "{name}: the wake fired, so it is no line"
                    );
                }
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_clip_holds_only_what_was_heard_since_listening_began() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("fresh-clip");
            let recorder = Recorder::in_dir(AUDIO_AND_METRICS, &dir).unwrap();
            let (rig, engine, frames) = Rig::recording("{}", recorder);
            let task = listen_task(engine, frames, 2);
            hear(&rig, 1, 10).await;
            rig.wake.fire(WakeEvent::Conversation);
            hear(&rig, 11, 11).await;
            // Audio between two listens is never scored, so it must not join the next clip.
            hear(&rig, 20, 21).await;
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 22, 22).await;
            finished(task).await;

            let mut lengths: Vec<_> = wake_clips(&dir)
                .iter()
                .map(|(_, samples)| samples.len())
                .collect();
            lengths.sort_unstable();
            assert_eq!(lengths, [3 * FRAME_SAMPLES, 11 * FRAME_SAMPLES]);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn recordings_changed_between_listens_apply_to_the_next_one() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("switched");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (rig, mut engine, mut frames) = Rig::recording("{}", recorder);
            let task = tokio::task::spawn_local(async move {
                engine.listen(&mut frames).await.unwrap();
                engine.set_recordings(AUDIO_AND_METRICS);
                engine.listen(&mut frames).await.unwrap();
                engine.finish().await;
            });
            hear(&rig, 1, 10).await;
            rig.wake.fire(WakeEvent::Conversation);
            hear(&rig, 11, 11).await;
            hear(&rig, 20, 21).await;
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 22, 22).await;
            finished(task).await;

            assert_eq!(
                wake_clips(&dir).len(),
                1,
                "only the listen after the change has a clip"
            );
        })
        .await;
}

/// A capture that is delivered, as `Run::Dictation` with every kind on: the files it leaves.
async fn files_of(name: &str, kinds: RecordKinds) -> Vec<String> {
    let dir = record_dir(name);
    let (mut rig, engine, frames) = Rig::recording("{}", Recorder::in_dir(kinds, &dir).unwrap());
    let task = capture_task(engine, frames);
    live_words(&mut rig).await;
    finished(task).await;
    files_under(&dir)
}

#[tokio::test(start_paused = true)]
async fn each_kind_writes_only_its_own_files_into_the_dictations_folder() {
    LocalSet::new()
        .run_until(async {
            let only = |text, audio| RecordKinds {
                metrics: false,
                text,
                audio,
            };
            for (name, kinds, expected) in [
                ("none", RecordKinds::default(), &[][..]),
                ("text", only(true, false), &["<dictation>/text.txt"][..]),
                (
                    "audio",
                    only(false, true),
                    &["<dictation>/speech.wav", "<dictation>/wake.wav"][..],
                ),
                (
                    "all",
                    ALL_KINDS,
                    &[
                        "<dictation>/speech.wav",
                        "<dictation>/text.txt",
                        "<dictation>/wake.wav",
                        "metrics.jsonl",
                    ][..],
                ),
            ] {
                assert_eq!(
                    files_of(&format!("kinds-{name}"), kinds).await,
                    expected,
                    "{name}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_wake_that_delivers_nothing_is_one_line_naming_the_reason_and_the_folder() {
    LocalSet::new()
        .run_until(async {
            const IDLE: &str = r#"{"capture_limits": {"idle": 20, "max": 90}}"#;
            // (name, config, what the user does, reason, last line of text.txt)
            type Script = for<'a> fn(&'a mut Rig) -> futures_util::future::LocalBoxFuture<'a, ()>;
            let cases: [(&str, &str, Script, &str, &str); 4] = [
                (
                    "cancelled",
                    "{}",
                    |rig| {
                        Box::pin(async {
                            rig.open().await;
                            rig.finals("never mind").await;
                        })
                    },
                    "cancelled",
                    "cancelled",
                ),
                (
                    "empty",
                    SLOW_PASTE,
                    |rig| {
                        Box::pin(async {
                            rig.open().await;
                            rig.feed_until(1000, Kind::Quiet).await;
                            rig.finals("paste that").await;
                            rig.feed_until(3499, Kind::Quiet).await;
                            rig.at(3500).await;
                        })
                    },
                    "empty",
                    "accepted (paste): ",
                ),
                (
                    "timed-out",
                    IDLE,
                    |rig| {
                        Box::pin(async {
                            rig.open().await;
                            rig.feed_until(20_300, Kind::Quiet).await;
                        })
                    },
                    "no_ending",
                    "failed: No words heard for 20 seconds.",
                ),
                (
                    "delivered",
                    SLOW_PASTE,
                    |rig| Box::pin(live_words(rig)),
                    "",
                    "accepted (paste): live words",
                ),
            ];
            for (name, config, act, reason, last_line) in cases {
                let dir = record_dir(&format!("nothing-{name}"));
                let recorder = Recorder::in_dir(ALL_KINDS, &dir).unwrap();
                let (mut rig, engine, frames) = Rig::recording(config, recorder);
                let task = capture_task(engine, frames);
                act(&mut rig).await;
                finished(task).await;

                let folder = dictation_dirs(&dir).pop().unwrap();
                let text = std::fs::read_to_string(folder.join("text.txt")).unwrap();
                assert_eq!(text.lines().last(), Some(last_line), "{name}");
                let nothing = wake_lines(&dir, "desktop_wake_nothing");
                if reason.is_empty() {
                    assert!(nothing.is_empty(), "{name}: something was delivered");
                    continue;
                }
                assert_eq!(nothing.len(), 1, "{name}");
                assert_eq!(nothing[0]["outcome"], reason, "{name}");
                let day = folder
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy();
                let time = folder.file_name().unwrap().to_string_lossy();
                assert_eq!(nothing[0]["dictation"], format!("{day}/{time}").as_str());
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn without_audio_recording_a_wake_buffers_and_writes_no_clip() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("no-clip");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (rig, engine, frames) = Rig::recording("{}", recorder);
            let task = listen_task(engine, frames, 1);
            hear(&rig, 1, 30).await;
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 31, 31).await;
            finished(task).await;
            assert!(
                dictation_dirs(&dir).is_empty(),
                "no dictation folder appears"
            );

            let dir = record_dir("clip-only");
            let audio = RecordKinds {
                audio: true,
                ..RecordKinds::default()
            };
            let (rig, engine, frames) =
                Rig::recording("{}", Recorder::in_dir(audio, &dir).unwrap());
            let task = listen_task(engine, frames, 1);
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 1, 1).await;
            finished(task).await;
            assert_eq!(wake_clips(&dir).len(), 1);
            assert!(!dir.join("metrics.jsonl").exists(), "no metrics asked for");
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello submit".to_owned())
                }
            );
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
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello".to_owned())
                }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_final_that_continues_after_an_ending_cancels_it_with_a_cue() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello submit").await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.finals("and then some more").await;
            assert_eq!(rig.started(Cue::Cancelled), [1500]);
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: "Hello submit and then some more".to_owned(),
                    partial: None
                }
            );
            rig.feed_until(6000, Kind::Quiet).await;
            assert!(!task.is_finished(), "the cancelled ending is not delivered");
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello submit and then some more".to_owned())
                }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_works_while_the_recognizer_has_stopped_taking_audio() {
    LocalSet::new()
        .run_until(async {
            for (name, in_backlog) in [("a live frame", false), ("the startup backlog", true)] {
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = capture_task(engine, frames);
                if in_backlog {
                    rig.server.stall_sends();
                    rig.wake_word().await;
                    rig.at(230).await;
                    rig.feed.send(quiet(5));
                    settle().await;
                    rig.server.ready();
                    settle().await;
                } else {
                    rig.open().await;
                    rig.feed_until(400, Kind::Quiet).await;
                    rig.server.stall_sends();
                    rig.feed_until(480, Kind::Quiet).await;
                }
                assert!(!task.is_finished(), "{name}: the send is stuck");
                rig.handle.cancel();
                settle().await;
                assert!(
                    task.is_finished(),
                    "{name}: cancel is not waiting for the send"
                );
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Cancelled { dictated: None },
                    "{name}"
                );
                assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel), "{name}");
                assert!(rig.gate.is_enabled(), "{name}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_capture_limits_still_end_a_capture_whose_recognizer_stopped_taking_audio() {
    LocalSet::new()
        .run_until(async {
            // Words at 5 s, then the server stops reading: idle 10 s ends it at 15 s, long
            // before the 60 s maximum. Without words the maximum is the only limit that can fire.
            for (name, limit, words, at, error) in [
                (
                    "idle",
                    r#"{"idle": 10, "max": 600}"#,
                    true,
                    15_000,
                    CaptureError::NoEnding(Duration::from_secs(10)),
                ),
                (
                    "max",
                    r#"{"idle": 600, "max": 60}"#,
                    false,
                    CAPTURE_OPENS + 60_000,
                    CaptureError::TooLong(Duration::from_secs(60)),
                ),
            ] {
                let (mut rig, engine, frames) =
                    Rig::new(&format!(r#"{{"capture_limits": {limit}}}"#));
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(5_000, Kind::Quiet).await;
                if words {
                    rig.finals("Hello").await;
                }
                rig.server.stall_sends();
                rig.feed_until(5_160, Kind::Quiet).await;
                rig.at(at - 1).await;
                assert!(!task.is_finished(), "{name}: not before the limit");
                rig.at(at).await;
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Failed {
                        error,
                        recoverable_text: words.then(|| "Hello".to_owned())
                    },
                    "{name}"
                );
                assert_eq!(rig.started(Cue::Error), [at], "{name}");
                assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel), "{name}");
                assert!(rig.gate.is_enabled(), "{name}");
            }
        })
        .await;
}

/// What the user does in a recorded run of cues.
#[derive(Clone, Copy)]
enum Run {
    /// Wake, an ending, the user continuing, a second ending, and its acceptance.
    Dictation,
    /// Wake, then the recognizer fails.
    Failure,
}

/// The `desktop_*cue` metric lines of `run`, in order, with every kind recorded.
async fn cue_lines(name: &str, timing: CueTiming, run: Run) -> Vec<serde_json::Value> {
    let dir = record_dir(name);
    let all = RecordKinds {
        metrics: true,
        text: true,
        audio: true,
    };
    let (mut rig, engine, frames) = Rig::recording("{}", Recorder::in_dir(all, &dir).unwrap());
    rig.cues.set_timing(timing);
    let task = capture_task(engine, frames);
    match run {
        Run::Dictation => {
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello submit").await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.partial("more words").await;
            rig.feed_until(3000, Kind::Quiet).await;
            rig.finals("more words submit").await;
            rig.feed_until(5499, Kind::Quiet).await;
            rig.at(5500).await;
        }
        Run::Failure => {
            rig.open().await;
            rig.server
                .fail("Thalvi could not reach speech recognition.");
            settle().await;
        }
    }
    finished(task).await;
    read_lines(&dir.join("metrics.jsonl"))
        .into_iter()
        .filter(|line| line["cue"].is_string())
        .collect()
}

fn cue_names(lines: &[serde_json::Value]) -> Vec<&str> {
    lines
        .iter()
        .map(|line| line["cue"].as_str().unwrap())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn every_cue_records_when_it_started_when_it_is_audible_and_what_the_user_waited() {
    LocalSet::new()
        .run_until(async {
            let timing = CueTiming {
                start: ms(40),
                output: Some(ms(15)),
            };
            let mut lines = cue_lines("cue-timing-dictation", timing, Run::Dictation).await;
            lines.extend(cue_lines("cue-timing-failure", timing, Run::Failure).await);
            let mut kinds = cue_names(&lines);
            kinds.sort_unstable();
            kinds.dedup();
            assert_eq!(
                kinds,
                ["accepted", "cancelled", "error", "provisional", "wake"]
            );
            for line in &lines {
                let cue = line["cue"].as_str().unwrap();
                let field = |key: &str| {
                    line[key]
                        .as_f64()
                        .unwrap_or_else(|| panic!("{cue}: {key} missing from {line}"))
                };
                assert!((field("start_ms") - 40.0).abs() < 1e-6, "{cue}");
                assert!((field("output_ms") - 15.0).abs() < 1e-6, "{cue}");
                assert!((field("audible_ms") - 55.0).abs() < 1e-6, "{cue}");
                assert!(field("start_ms") <= field("audible_ms"), "{cue}");
                assert!(field("audible_ms") <= field("perceived_ms"), "{cue}");
                assert!(
                    field("perceived_ms") < field("duration_ms"),
                    "{cue}: the wait ends when the cue starts, not when it drains"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_wake_cue_wait_is_counted_from_the_wake_word_not_from_the_cue_starting() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("cue-timing-wake");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, mut engine, mut frames) = Rig::recording("{}", recorder);
            let task = tokio::task::spawn_local(async move {
                engine.listen(&mut frames).await.unwrap();
                // The caller takes 70 ms to get from the wake word to the capture.
                tokio::time::sleep(ms(70)).await;
                let outcome = engine.capture(&mut frames).await;
                engine.finish().await;
                outcome
            });
            rig.wake_word().await;
            rig.server.ready();
            rig.at(70).await;
            rig.at(70 + 230).await;
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let wake = lines
                .iter()
                .find(|line| line["event"] == "desktop_ready_cue")
                .unwrap();
            assert!((wake["start_ms"].as_f64().unwrap() - 25.0).abs() < 1e-6);
            assert!((wake["audible_ms"].as_f64().unwrap() - 40.0).abs() < 1e-6);
            assert!(
                (wake["perceived_ms"].as_f64().unwrap() - 110.0).abs() < 1e-6,
                "70 ms before the cue started plus 40 ms until it was audible: {wake}"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_device_that_reports_no_delay_leaves_output_empty_and_a_failed_cue_has_no_timing() {
    LocalSet::new()
        .run_until(async {
            let silent = CueTiming {
                start: ms(30),
                output: None,
            };
            let lines = cue_lines("cue-timing-silent", silent, Run::Dictation).await;
            let wake = &lines[0];
            assert_eq!(wake["cue"], "wake");
            assert!(wake["output_ms"].is_null() && wake.get("output_ms").is_some());
            assert!((wake["start_ms"].as_f64().unwrap() - 30.0).abs() < 1e-6);
            for key in ["audible_ms", "perceived_ms"] {
                assert!(
                    wake[key].is_null() && wake.get(key).is_some(),
                    "{key} without a device delay"
                );
            }

            let dir = record_dir("cue-timing-failed");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            rig.cues.fail(Cue::Wake);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.handle.cancel();
            finished(task).await;
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let wake = lines
                .iter()
                .find(|line| line["event"] == "desktop_ready_cue")
                .unwrap();
            assert_eq!(wake["outcome"], "error");
            for key in ["start_ms", "output_ms", "audible_ms", "perceived_ms"] {
                assert!(wake.get(key).is_none(), "{key} on a cue that failed");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cue_lines_hold_timings_only_never_what_was_said_or_heard() {
    LocalSet::new()
        .run_until(async {
            let timing = CueTiming {
                start: ms(40),
                output: Some(ms(15)),
            };
            let lines = cue_lines("cue-timing-privacy", timing, Run::Dictation).await;
            assert_eq!(
                cue_names(&lines),
                [
                    "wake",
                    "provisional",
                    "cancelled",
                    "provisional",
                    "accepted"
                ]
            );
            for line in &lines {
                let mut keys: Vec<&str> = line
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                keys.sort_unstable();
                assert_eq!(
                    keys,
                    [
                        "audible_ms",
                        "cue",
                        "duration_ms",
                        "event",
                        "outcome",
                        "output_ms",
                        "perceived_ms",
                        "start_ms",
                        "ts_ms"
                    ],
                    "{line}"
                );
                assert!(!line.to_string().contains("more words"), "{line}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cue_timings_are_not_written_when_metrics_are_off() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("cue-timing-off");
            let text_only = RecordKinds {
                metrics: false,
                text: true,
                audio: false,
            };
            let recorder = Recorder::in_dir(text_only, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            let task = capture_task(engine, frames);
            live_words(&mut rig).await;
            finished(task).await;
            assert!(!dictation_dirs(&dir).is_empty(), "text was recorded");
            assert!(!dir.join("metrics.jsonl").exists());
        })
        .await;
}

/// Runs a hold then its capture on the engine, as a caller would.
fn hold_task(mut engine: TestEngine, mut frames: FakeFrames) -> JoinHandle<CaptureOutcome> {
    tokio::task::spawn_local(async move {
        let trigger = engine.listen(&mut frames).await.unwrap();
        assert_eq!(trigger, WakeTrigger::Hold);
        let outcome = engine.capture(&mut frames).await;
        engine.finish().await;
        outcome
    })
}

impl Rig {
    /// The key goes down at the current time.
    async fn press(&self) {
        settle().await;
        self.key.set(true);
        settle().await;
    }

    async fn release(&self, at: u64) {
        self.at(at).await;
        self.key.set(false);
        settle().await;
    }

    /// The key goes down at 0 and the recognizer is ready at once: audio flows from the end of
    /// the start cue.
    async fn hold(&mut self) {
        self.press().await;
        self.server.ready();
        settle().await;
        self.at(230).await;
    }
}

#[derive(Clone, Copy)]
enum Say {
    Partial(&'static str),
    Final(&'static str),
}

impl Rig {
    async fn say(&self, at: u64, say: Say) {
        self.feed_until(at, Kind::Voice).await;
        match say {
            Say::Partial(text) => self.partial(text).await,
            Say::Final(text) => self.finals(text).await,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn releasing_the_key_pastes_what_was_said_without_a_countdown_or_enter() {
    use Say::{Final, Partial};
    // (what was said and when, release, expected outcome, when the cue that ends the capture starts)
    type Case = (&'static [(u64, Say)], u64, CaptureOutcome, Cue, u64);
    let cases: [Case; 7] = [
        (
            &[(1000, Partial("hello world"))],
            1200,
            accepted(CaptureRoute::Paste, "hello world"),
            Cue::Accepted,
            1450,
        ),
        (
            &[(1000, Partial("hello world")), (1250, Final("hello world"))],
            1200,
            accepted(CaptureRoute::Paste, "hello world"),
            Cue::Accepted,
            1250,
        ),
        (
            &[(800, Final("first line")), (1250, Final("second line"))],
            1200,
            accepted(CaptureRoute::Paste, "first line second line"),
            Cue::Accepted,
            1250,
        ),
        (
            &[(800, Final("first line")), (1000, Partial("second line"))],
            1200,
            accepted(CaptureRoute::Paste, "first line second line"),
            Cue::Accepted,
            1450,
        ),
        (
            &[(1000, Partial("hello never mind"))],
            1200,
            accepted(CaptureRoute::Paste, "hello never mind"),
            Cue::Accepted,
            1450,
        ),
        (
            &[(800, Final("first line")), (1250, Final("never mind"))],
            1200,
            accepted(CaptureRoute::Paste, "first line never mind"),
            Cue::Accepted,
            1250,
        ),
        // The shortest hold that dictates is MIN_HOLD, 300 ms.
        (
            &[(250, Partial("ok"))],
            300,
            accepted(CaptureRoute::Paste, "ok"),
            Cue::Accepted,
            800,
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (case, (said, release, expected, cue, cue_at)) in cases.into_iter().enumerate() {
                let label = format!("case {case}, released at {release}");
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = hold_task(engine, frames);
                rig.hold().await;
                for &(at, say) in said.iter().filter(|(at, _)| *at < release) {
                    rig.say(at, say).await;
                }
                rig.release(release).await;
                for &(at, say) in said.iter().filter(|(at, _)| *at >= release) {
                    rig.say(at, say).await;
                }
                assert!(
                    !task.is_finished(),
                    "{label}: the capture is still open at the release"
                );
                rig.at(cue_at).await;
                assert_eq!(rig.started(cue), [cue_at], "{label}");
                assert_eq!(rig.started(Cue::Provisional).len(), 0, "{label}");
                assert_eq!(finished(task).await, expected, "{label}");
                assert_eq!(rig.notices(), Vec::<String>::new(), "{label}");
                assert!(rig.gate.is_enabled(), "{label}");
                assert_eq!(rig.state(), EngineState::Listening, "{label}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_with_no_speech_or_under_the_minimum_pastes_nothing_and_says_why() {
    // (recognizer ready, speech, release, what the user is told)
    type Case = (bool, Option<(u64, Say)>, u64, CaptureError);
    let cases: [Case; 5] = [
        (false, None, 150, CaptureError::HoldTooShort),
        (
            true,
            Some((250, Say::Partial("ok"))),
            299,
            CaptureError::HoldTooShort,
        ),
        (true, None, 1000, CaptureError::NothingHeard),
        (
            true,
            Some((500, Say::Partial("  "))),
            1000,
            CaptureError::NothingHeard,
        ),
        (false, None, 400, CaptureError::NothingHeard),
    ];
    LocalSet::new()
        .run_until(async {
            for (case, (ready, said, release, error)) in cases.into_iter().enumerate() {
                let label = format!("case {case}");
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = hold_task(engine, frames);
                rig.press().await;
                if ready {
                    rig.server.ready();
                    settle().await;
                    rig.at(230).await;
                }
                if let Some((at, say)) = said {
                    rig.say(at, say).await;
                }
                rig.release(release).await;
                // Words still being recognized get the tail; with none, the capture ends at once
                // or when the tail is over.
                let ended = if ready && release >= 300 {
                    release + 500
                } else {
                    release
                };
                rig.at(ended).await;
                assert_eq!(rig.started(Cue::Error), [ended], "{label}");
                assert_eq!(rig.started(Cue::Accepted).len(), 0, "{label}");
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Failed {
                        error: error.clone(),
                        recoverable_text: None
                    },
                    "{label}"
                );
                assert!(rig.gate.is_enabled(), "{label}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_hold_from_outside_ends_it_cancelled_and_the_release_does_nothing_more() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, mut engine, mut frames) = Rig::new("{}");
            let task = tokio::task::spawn_local(async move {
                let trigger = engine.listen(&mut frames).await.unwrap();
                assert_eq!(trigger, WakeTrigger::Hold);
                let outcome = engine.capture(&mut frames).await;
                // The key is still down; listening goes on without starting anything.
                let next =
                    tokio::time::timeout(Duration::from_secs(30), engine.listen(&mut frames)).await;
                (outcome, next.is_err())
            });
            rig.hold().await;
            rig.say(800, Say::Final("draft words")).await;
            rig.say(1000, Say::Final("never mind")).await;
            assert!(
                !task.is_finished(),
                "never mind is dictated text during a hold"
            );
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: "draft words never mind".to_owned(),
                    partial: None
                }
            );
            rig.handle.cancel();
            settle().await;
            rig.release(2000).await;
            let (outcome, kept_listening) = finished(task).await;
            assert_eq!(
                outcome,
                CaptureOutcome::Cancelled {
                    dictated: Some("draft words never mind".to_owned())
                }
            );
            assert!(
                kept_listening,
                "neither the release nor the held key started a capture"
            );
            assert_eq!(rig.started(Cue::Accepted).len(), 0);
            assert_eq!(rig.started(Cue::Provisional).len(), 0);
            assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel));
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn spoken_endings_are_text_during_a_hold_and_act_after_the_wake_word() {
    const CUSTOM: &str = r#"{"endings": {"paste": "paste", "send it": "submit",
        "make it so": "task", "drop last": "scratch", "forget it": "cancel"}}"#;
    // (config, what is said, what the same words do after the wake word; a hold
    // always pastes everything that was said)
    let cases: [(&str, &str, CaptureOutcome); 7] = [
        (
            "{}",
            "fix the bug never mind",
            CaptureOutcome::Cancelled {
                dictated: Some("fix the bug".to_owned()),
            },
        ),
        (
            "{}",
            "run the tests do that",
            accepted(CaptureRoute::Task, "run the tests"),
        ),
        (
            "{}",
            "send the note submit",
            accepted(CaptureRoute::Submit, "send the note"),
        ),
        (
            "{}",
            "keep this paste that",
            accepted(CaptureRoute::Paste, "keep this"),
        ),
        (
            CUSTOM,
            "fix the bug forget it",
            CaptureOutcome::Cancelled {
                dictated: Some("fix the bug".to_owned()),
            },
        ),
        (
            CUSTOM,
            "run the tests make it so",
            accepted(CaptureRoute::Task, "run the tests"),
        ),
        (
            "{}",
            "open google.com",
            accepted(CaptureRoute::Task, "open google.com"),
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (config, said, wake_outcome) in cases {
                let (mut rig, engine, frames) = Rig::new(config);
                let task = hold_task(engine, frames);
                rig.hold().await;
                rig.say(1000, Say::Final(said)).await;
                rig.release(1200).await;
                rig.at(1450).await;
                assert_eq!(rig.started(Cue::Accepted), [1450], "hold: {said}");
                assert_eq!(rig.started(Cue::Provisional).len(), 0, "hold: {said}");
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Paste, said),
                    "hold: {said}"
                );

                let (mut rig, engine, frames) = Rig::new(config);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals(said).await;
                rig.feed_until(4000, Kind::Quiet).await;
                assert_eq!(finished(task).await, wake_outcome, "wake word: {said}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn key_auto_repeat_during_a_hold_starts_nothing_and_a_press_during_a_wake_dictation_is_refused()
 {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Partial("hello")).await;
            for _ in 0..5 {
                rig.key.set(true);
                settle().await;
            }
            assert_eq!(
                rig.notices(),
                Vec::<String>::new(),
                "a repeat is not a press"
            );
            assert!(matches!(rig.state(), EngineState::Capturing { .. }));
            rig.release(1200).await;
            rig.at(1450).await;
            assert_eq!(finished(task).await, accepted(CaptureRoute::Paste, "hello"));
            assert_eq!(rig.started(Cue::Wake).len(), 1, "no second capture started");

            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello").await;
            rig.key.set(true);
            settle().await;
            assert_eq!(
                rig.notices(),
                ["A dictation is already running, so pressing the key started nothing."]
            );
            rig.key.set(false);
            settle().await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.finals("world paste that").await;
            rig.feed_until(3000, Kind::Quiet).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "Hello world")
            );
            assert_eq!(rig.notices().len(), 1, "the release said nothing more");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_listener_keeps_a_hold_or_reports_a_tap_before_it_resumes() {
    for (tapped, stopped, resumed) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, false),
        (false, true, true),
        (true, true, true),
    ] {
        let (rig, mut engine, mut frames) = Rig::new("{}");
        let mut first = Box::pin(engine.listen(&mut frames));
        tokio::select! {
            biased;
            result = &mut first => panic!("listener ended: {result:?}"),
            () = std::future::ready(()) => {}
        }
        assert_eq!(rig.wake.resets(), 1);
        drop(first);
        rig.key.set(true);
        if tapped {
            rig.key.set(false);
        }
        if stopped {
            let mut presses = engine.stop_listening();
            assert_eq!(
                u64::from(presses.take().is_some()) + presses.take_lost(),
                1,
                "closing the microphone hands its pending press to the caller"
            );
            if resumed {
                engine.resume_listening(presses);
            }
        }
        let result = tokio::time::timeout(ms(5), engine.listen(&mut frames)).await;
        if tapped || stopped {
            assert!(result.is_err(), "a released tap must not capture");
            assert_eq!(
                engine.take_ignored(),
                if stopped {
                    vec![]
                } else {
                    vec![Refusal::TooQuick]
                }
            );
            if !tapped {
                rig.key.set(false);
            }
            rig.key.set(true);
            assert_eq!(
                tokio::time::timeout(ms(5), engine.listen(&mut frames))
                    .await
                    .unwrap()
                    .unwrap(),
                WakeTrigger::Hold,
                "the next fresh press still captures"
            );
        } else {
            assert_eq!(
                result.unwrap().unwrap(),
                WakeTrigger::Hold,
                "HANDOFF_PRESS_SILENTLY_DROPPED"
            );
            assert_eq!(engine.take_ignored(), []);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_key_already_down_when_listening_starts_waits_for_its_next_press() {
    LocalSet::new()
        .run_until(async {
            let (rig, mut engine, mut frames) = Rig::new("{}");
            // Pressed while a delivery ran: the caller reported it, and it must not start later.
            rig.key.set(true);
            let task = tokio::task::spawn_local(async move { engine.listen(&mut frames).await });
            settle().await;
            assert!(!task.is_finished(), "a key held from before starts nothing");
            rig.key.set(false);
            settle().await;
            assert!(!task.is_finished(), "its release starts nothing");
            rig.key.set(true);
            settle().await;
            assert_eq!(finished(task).await.unwrap(), WakeTrigger::Hold);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn holds_record_their_start_end_and_how_the_key_decided() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("hold");
            let recorder = Recorder::in_dir(AUDIO_AND_METRICS, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.feed_until(960, Kind::Voice).await;
            rig.say(1000, Say::Partial("hello")).await;
            let spoken = rig.sent().len();
            rig.release(1200).await;
            assert!(rig.sent().len() > spoken, "the release sends silence");
            rig.at(1450).await;
            assert_eq!(finished(task).await, accepted(CaptureRoute::Paste, "hello"));
            assert_eq!(
                std::fs::metadata(dictation_dirs(&dir).pop().unwrap().join("speech.wav"))
                    .unwrap()
                    .len(),
                44 + 2560 * u64::try_from(spoken).unwrap(),
                "the silence sent at the release is not recorded as speech"
            );
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let seen = events(&lines);
            for expected in [
                ("desktop_hold_start", "ok"),
                ("desktop_hold_end", "released"),
                ("desktop_capture_flow", "ok"),
            ] {
                assert!(
                    seen.contains(&(expected.0.to_owned(), expected.1.to_owned())),
                    "{expected:?} missing from {seen:?}"
                );
            }
            assert!((duration_of(&lines, "desktop_hold_end") - 1200.0).abs() < 0.5);

            let dir = record_dir("hold-short");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (rig, engine, frames) = Rig::recording("{}", recorder);
            let task = hold_task(engine, frames);
            rig.press().await;
            rig.release(100).await;
            assert!(matches!(
                finished(task).await,
                CaptureOutcome::Failed { .. }
            ));
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let seen = events(&lines);
            for expected in [
                ("desktop_hold_end", "too_short"),
                ("desktop_wake_nothing", "hold_too_short"),
            ] {
                assert!(
                    seen.contains(&(expected.0.to_owned(), expected.1.to_owned())),
                    "{expected:?} missing from {seen:?}"
                );
            }

            let dir = record_dir("hold-refused");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.key.set(true);
            settle().await;
            rig.finals("never mind").await;
            finished(task).await;
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let refused = lines
                .iter()
                .find(|line| line["event"] == "desktop_hold_start")
                .unwrap();
            assert_eq!(refused["outcome"], "refused");
            assert_eq!(refused["reason"], "capturing");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_sends_only_the_audio_that_comes_after_its_start_cue() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            rig.press().await;
            rig.server.ready();
            settle().await;
            // Said while the cue plays, so the cue could reach the recognizer.
            rig.at(100).await;
            rig.feed.send(quiet(5));
            settle().await;
            rig.at(230).await;
            rig.at(240).await;
            rig.feed.send(quiet(9));
            settle().await;
            assert_eq!(tags(&rig.sent()), [9]);
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn nothing_spoken_after_the_release_reaches_the_recognizer_but_the_words_already_sent_finish()
{
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Partial("hello")).await;
            rig.release(1200).await;
            let before = rig.sent().len();
            // An aside: the recognizer must never hear it.
            rig.feed_until(1400, Kind::Voice).await;
            assert_eq!(rig.sent().len(), before, "audio after the release was sent");
            rig.finals("hello there").await;
            rig.at(1450).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "hello there")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_released_before_the_recognizer_connects_still_sends_its_speech_within_the_wait() {
    // (speech after the start cue, recognizer ready at, what comes of it)
    type Case = (Kind, Option<u64>, Option<CaptureOutcome>);
    let cases: [Case; 3] = [
        (
            Kind::Voice,
            Some(1000),
            Some(accepted(CaptureRoute::Paste, "hello world")),
        ),
        // Nothing but silence was held: the same honest failure as before, without waiting.
        (
            Kind::Quiet,
            Some(1000),
            Some(CaptureOutcome::Failed {
                error: CaptureError::NothingHeard,
                recoverable_text: None,
            }),
        ),
        // The connection never comes within the bounded wait.
        (Kind::Voice, None, None),
    ];
    LocalSet::new()
        .run_until(async {
            for (case, (kind, ready, expected)) in cases.into_iter().enumerate() {
                let label = format!("case {case}");
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = hold_task(engine, frames);
                rig.press().await;
                rig.at(230).await;
                rig.feed_until(600, kind).await;
                let spoken = tags(&(3..8).map(|id| kind.frame(id)).collect::<Vec<Frame>>());
                rig.release(600).await;
                if !matches!(kind, Kind::Quiet) {
                    // An aside after the release stays out of what the recognizer is sent.
                    rig.feed_until(900, kind).await;
                }
                let Some(ready) = ready else {
                    rig.at(8000).await;
                    assert!(
                        matches!(
                            finished(task).await,
                            CaptureOutcome::Failed {
                                error: CaptureError::NotReady { .. },
                                ..
                            }
                        ),
                        "{label}"
                    );
                    assert_eq!(rig.sent().len(), 0, "{label}");
                    continue;
                };
                if matches!(kind, Kind::Quiet) {
                    assert_eq!(
                        rig.started(Cue::Error),
                        [600],
                        "{label}: ends at the release"
                    );
                    assert_eq!(rig.sent().len(), 0, "{label}");
                    assert_eq!(finished(task).await, expected.unwrap(), "{label}");
                    continue;
                }
                rig.at(ready).await;
                assert!(!task.is_finished(), "{label}: waiting for the connection");
                rig.server.ready();
                settle().await;
                let mut expected_sent = spoken.clone();
                expected_sent.extend(std::iter::repeat_n(0, 10));
                assert_eq!(
                    tags(&rig.sent()),
                    expected_sent,
                    "{label}: the buffered speech arrived"
                );
                rig.partial("hello world").await;
                // The words get the tail from the moment they were sent.
                rig.at(ready + 500).await;
                assert_eq!(rig.started(Cue::Accepted), [ready + 500], "{label}");
                assert_eq!(finished(task).await, expected.unwrap(), "{label}");
                assert_eq!(rig.notices(), Vec::<String>::new(), "{label}");
            }
        })
        .await;
}

#[derive(Clone, Copy)]
enum Connect {
    /// The recognizer is still connecting when the key comes up, and is ready long after.
    Pending,
    /// The connection completes before the engine has looked at the frames or the release.
    SamePoll,
    /// The recognizer was ready before the speech began.
    AlreadyReady,
}

#[tokio::test(start_paused = true)]
async fn a_hold_sends_what_was_queued_before_its_release_and_nothing_queued_after_it() {
    use Connect::{AlreadyReady, Pending, SamePoll};
    use Kind::{Quiet, Voice};
    // (connect, queued before the release, queued after it before the engine looked,
    // whether the recognizer is sent the former, whether the words are pasted)
    type Case = (Connect, Kind, Kind, bool, bool);
    let cases: [Case; 12] = [
        (Pending, Voice, Quiet, true, true),
        (Pending, Voice, Voice, true, true),
        (Pending, Quiet, Quiet, false, false),
        (Pending, Quiet, Voice, false, false),
        (SamePoll, Voice, Quiet, true, true),
        (SamePoll, Voice, Voice, true, true),
        (SamePoll, Quiet, Quiet, false, false),
        (SamePoll, Quiet, Voice, false, false),
        (AlreadyReady, Voice, Quiet, true, true),
        (AlreadyReady, Voice, Voice, true, true),
        (AlreadyReady, Quiet, Quiet, true, false),
        (AlreadyReady, Quiet, Voice, true, false),
    ];
    LocalSet::new()
        .run_until(async {
            for tap in [false, true] {
                for (case, (connect, before, after, sent, pasted)) in cases.into_iter().enumerate()
                {
                    let label = format!("case {case}, with a full tap while pending: {tap}");
                    let (mut rig, mut engine, mut frames) = Rig::new("{}");
                    let task = tokio::task::spawn_local(async move {
                        assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                        let outcome = engine.capture(&mut frames).await;
                        let ignored = engine.take_ignored();
                        engine.finish().await;
                        (outcome, ignored)
                    });
                    rig.press().await;
                    if matches!(connect, AlreadyReady) {
                        rig.server.ready();
                        settle().await;
                    }
                    rig.at(600).await;
                    for id in 3..8 {
                        rig.feed.send(before.frame(id));
                    }
                    rig.key.set(false);
                    for id in 8..13 {
                        rig.feed.send(after.frame(id));
                    }
                    if tap {
                        rig.key.set(true);
                        rig.key.set(false);
                    }
                    match connect {
                        Pending => {
                            settle().await;
                            rig.at(1000).await;
                            rig.server.ready();
                        }
                        SamePoll => rig.server.ready(),
                        AlreadyReady => {}
                    }
                    settle().await;
                    let queued: Vec<Frame> = (3..8).map(|id| before.frame(id)).collect();
                    let mut expected = if sent { tags(&queued) } else { vec![] };
                    if sent {
                        expected.extend(std::iter::repeat_n(0, 10));
                    }
                    assert_eq!(tags(&rig.sent()), expected, "{label}: frames sent");
                    if pasted {
                        rig.partial("hello world").await;
                    }
                    rig.at(1500).await;
                    let (outcome, ignored) = finished(task).await;
                    let expected_outcome = if pasted {
                        accepted(CaptureRoute::Paste, "hello world")
                    } else {
                        CaptureOutcome::Failed {
                            error: CaptureError::NothingHeard,
                            recoverable_text: None,
                        }
                    };
                    assert_eq!(outcome, expected_outcome, "{label}");
                    assert_eq!(
                        tags(&rig.sent()),
                        expected,
                        "{label}: frames sent by the end"
                    );
                    let refused: Vec<Refusal> = if tap {
                        vec![Refusal::Capturing]
                    } else {
                        vec![]
                    };
                    assert_eq!(ignored, refused, "{label}");
                    let told: Vec<&str> = refused.iter().map(|r| r.notice()).collect();
                    assert_eq!(rig.notices(), told, "{label}");
                }
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_abandoned_before_its_capture_leaves_nothing_for_the_next_wake_capture() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, mut engine, mut frames) = Rig::new("{}");
            let task = tokio::task::spawn_local(async move {
                assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                // A cancel landed while the window was bound: the capture never ran.
                assert_eq!(
                    engine.listen(&mut frames).await.unwrap(),
                    WakeTrigger::Capture
                );
                let outcome = engine.capture(&mut frames).await;
                engine.finish().await;
                outcome
            });
            rig.press().await;
            rig.key.set(false);
            settle().await;
            // The caller reopens the gate the hold's start shut.
            rig.gate.set_enabled(true);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello").await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.finals("world paste that").await;
            rig.feed_until(3000, Kind::Quiet).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "Hello world")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn presses_that_start_nothing_are_told_counted_and_left_to_the_caller() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("presses-ignored");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, mut engine, mut frames) = Rig::recording("{}", recorder);
            let task = tokio::task::spawn_local(async move {
                let trigger = engine.listen(&mut frames).await.unwrap();
                assert_eq!(trigger, WakeTrigger::Capture);
                let outcome = engine.capture(&mut frames).await;
                let ignored = engine.take_ignored();
                engine.finish().await;
                (outcome, ignored)
            });
            // Pressed and released before the engine looked: no hold, and no silence either.
            settle().await;
            rig.key.set(true);
            rig.key.set(false);
            settle().await;
            assert_eq!(rig.notices(), [Refusal::TooQuick.notice()]);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello").await;
            // During a dictation by wake word, a press is refused; so is one that was over already.
            rig.key.set(true);
            settle().await;
            rig.key.set(false);
            rig.key.set(true);
            rig.key.set(false);
            settle().await;
            rig.finals("never mind").await;
            let (outcome, ignored) = finished(task).await;
            assert_eq!(
                outcome,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello".to_owned())
                }
            );
            assert_eq!(
                ignored,
                [Refusal::TooQuick, Refusal::Capturing, Refusal::Capturing]
            );
            assert_eq!(
                rig.notices(),
                [
                    Refusal::TooQuick.notice(),
                    Refusal::Capturing.notice(),
                    Refusal::Capturing.notice()
                ]
            );
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let refused: Vec<(String, u64)> = lines
                .iter()
                .filter(|line| line["event"] == "desktop_hold_start")
                .map(|line| {
                    (
                        line["reason"].as_str().unwrap().to_owned(),
                        line["count"].as_u64().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                refused,
                [
                    ("too_quick".to_owned(), 1),
                    ("capturing".to_owned(), 1),
                    ("capturing".to_owned(), 1)
                ]
            );
        })
        .await;
}

enum Publication {
    BeforeRelease,
    Continuation { release: bool },
}

struct PublishDuringPoll {
    inner: FakeFrames,
    feed: FrameFeed,
    key: HoldInput,
    arm: Rc<std::cell::Cell<bool>>,
    waker: Rc<RefCell<Option<std::task::Waker>>>,
    publication: Publication,
}
impl thalvi_desktop::engine::FrameSource for PublishDuringPoll {
    async fn next(&mut self) -> Result<thalvi_desktop::frame::Queued, AudioError> {
        use std::future::Future;
        let Self {
            inner,
            feed,
            key,
            arm,
            waker,
            publication,
        } = self;
        let mut next = std::pin::pin!(thalvi_desktop::engine::FrameSource::next(inner));
        std::future::poll_fn(|cx| {
            *waker.borrow_mut() = Some(cx.waker().clone());
            let result = next.as_mut().poll(cx);
            if result.is_pending() && arm.replace(false) {
                match publication {
                    Publication::BeforeRelease => {
                        for id in 3..8 {
                            feed.send(voiced(id));
                        }
                        key.set(false);
                        for id in 8..13 {
                            feed.send(voiced(id));
                        }
                    }
                    Publication::Continuation { release } => {
                        if *release {
                            key.set(false);
                        }
                        for id in 20..23 {
                            feed.send(voiced(id));
                        }
                        return next.as_mut().poll(cx);
                    }
                }
            }
            result
        })
        .await
    }
}
#[tokio::test(start_paused = true)]
async fn a_frame_published_after_pending_poll_but_before_release_is_not_lost() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, mut engine, frames) = Rig::new("{}");
            let (_unused, dummy) = FakeFrames::new(&rig.gate);
            let feed = std::mem::replace(&mut rig.feed, dummy);
            let arm = Rc::new(std::cell::Cell::new(false));
            let waker = Rc::new(RefCell::new(None));
            let mut frames = PublishDuringPoll {
                inner: frames,
                feed,
                key: rig.key.clone(),
                arm: Rc::clone(&arm),
                waker: Rc::clone(&waker),
                publication: Publication::BeforeRelease,
            };
            let task = tokio::task::spawn_local(async move {
                assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                let outcome = engine.capture(&mut frames).await;
                engine.finish().await;
                outcome
            });
            rig.press().await;
            rig.at(600).await;
            arm.set(true);
            waker.borrow().as_ref().unwrap().wake_by_ref();
            settle().await;
            rig.at(1000).await;
            rig.server.ready();
            settle().await;
            assert_eq!(
                rig.sent().len(),
                15,
                "pre-release frames arrived between the pending audio poll and the release poll"
            );
            rig.partial("hello world").await;
            rig.at(1500).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "hello world")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn continuation_published_during_a_poll_reaches_the_recognizer_only_while_the_key_is_down() {
    LocalSet::new()
        .run_until(async {
            for release in [false, true] {
                let (mut rig, mut engine, frames) = Rig::new(SLOW_PASTE);
                let (_unused, dummy) = FakeFrames::new(&rig.gate);
                let feed = std::mem::replace(&mut rig.feed, dummy);
                let arm = Rc::new(std::cell::Cell::new(false));
                let waker = Rc::new(RefCell::new(None));
                let mut frames = PublishDuringPoll {
                    inner: frames,
                    feed,
                    key: rig.key.clone(),
                    arm: Rc::clone(&arm),
                    waker: Rc::clone(&waker),
                    publication: Publication::Continuation { release },
                };
                let task = tokio::task::spawn_local(async move {
                    assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                    engine.capture(&mut frames).await
                });
                rig.press().await;
                rig.server.ready();
                rig.at(230).await;
                rig.finals("hello paste that").await;
                arm.set(true);
                waker.borrow().as_ref().unwrap().wake_by_ref();
                settle().await;
                let expected: Vec<Frame> = if release {
                    Vec::new()
                } else {
                    (20..23).map(voiced).collect()
                };
                assert_eq!(
                    tags(&rig.sent()),
                    tags(&expected),
                    "release={release}: continuation reaches recognition only while the key is down"
                );
                task.abort();
                let _ = task.await;
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_release_triggers_a_silence_flush_and_pastes_as_soon_as_the_final_arrives() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Partial("hello world")).await;
            rig.release(1200).await;
            let sent = rig.sent();
            assert_eq!(
                tags(&sent[sent.len() - 10..]),
                vec![0; 10],
                "a release flushes silence frames at once"
            );
            assert!(!task.is_finished(), "waiting for the final result");
            rig.say(1250, Say::Final("hello world")).await;
            rig.at(1250).await;
            assert_eq!(
                rig.started(Cue::Accepted),
                [1250],
                "the arrival of the final pastes at once without waiting the limit"
            );
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "hello world")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_with_no_final_within_the_limit_pastes_what_is_there_at_the_limit() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Partial("partial text")).await;
            rig.release(1200).await;
            rig.at(1449).await;
            assert!(
                !task.is_finished(),
                "at 1449 ms the limit has not yet expired"
            );
            assert!(
                rig.started(Cue::Accepted).is_empty(),
                "no paste before the limit"
            );
            rig.at(1450).await;
            assert_eq!(
                rig.started(Cue::Accepted),
                [1450],
                "pastes at the 250 ms limit"
            );
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "partial text")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_the_post_release_wait_cancels_at_once_and_delivers_nothing() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Final("hello")).await;
            rig.release(1200).await;
            rig.at(1250).await;
            rig.handle.cancel();
            settle().await;
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("hello".to_owned())
                }
            );
            assert_eq!(rig.started(Cue::Accepted).len(), 0);
            assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel));
        })
        .await;
}

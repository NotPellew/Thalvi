//! The capture engine on a paused clock, with a fake microphone, wake word, cues and recognizer.

#![cfg(test)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use thalvi_desktop::audio::{AudioError, InputGate};
use thalvi_desktop::capture::{CaptureRequest, CaptureRoute, Ending};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::cues::{Cue, CueTiming};
use thalvi_desktop::device::{Download, Export, Step};
use thalvi_desktop::engine::fakes::{
    CueControl, CueLog, FakeCuePlayer, FakeFrames, FakeSpotter, FakeWake, FrameFeed,
    SpotterControl, WakeControl,
};
use thalvi_desktop::engine::{
    CaptureError, CaptureOutcome, EndingListener, Engine, EngineHandle, EngineState, FrameSource,
    ListenError, Parts, PreviewError, SubmitPreview, WakeTrigger,
};
use thalvi_desktop::feed::{Clip, Fed, Feed, FeedEnd, Feeder};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::hold::{HoldInput, HoldKind, Refusal};
use thalvi_desktop::host::WindowApp;
use thalvi_desktop::metrics::{RecordKinds, Recorder};
use thalvi_desktop::models::CatalogSpeech;
use thalvi_desktop::recognizer::{
    FakeCall, FakeControl, FakeRecognizer, RecognizerError, RecognizerEvent, SpeechSource,
};
use thalvi_desktop::report::UserError;
use thalvi_desktop::secondary::Language;
use thalvi_desktop::spotter::KeywordSpotter;
use thalvi_desktop::wake::WakeEvent;
use thalvi_desktop::writing::{CLEANUP_PROMPT, DEFAULT_PROMPT, WritingError};
use tokio::sync::{oneshot, watch};
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
        let (rig, engine, frames) = Self::build(config, recorder, None, None);
        (rig, engine, frames)
    }

    /// A rig whose writing endpoint is the app-owned chat runtime.
    fn with_chat(
        config: &DesktopConfig,
        recorder: Recorder,
        chat: thalvi_desktop::chat::Chat,
    ) -> (Self, TestEngine, FakeFrames) {
        let gate = InputGate::default();
        let (frames, feed) = FakeFrames::new(&gate);
        let (wake_detector, wake) = FakeWake::new();
        let (cue_player, cues) = FakeCuePlayer::new();
        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&notices);
        let (key, hold) = HoldInput::new();
        let (recognizer, server) = FakeRecognizer::new();
        let (engine, handle) = Engine::new(
            Parts {
                recognizer,
                wake: wake_detector,
                cues: cue_player,
                gate: gate.clone(),
                chat: Some(chat),
                http: reqwest::Client::new(),
                hold,
                notice: Box::new(move |notice| sink.borrow_mut().push(notice.to_owned())),
                spotter: None,
                spotter_off: None,
            },
            config,
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

    /// A rig whose recognizer loads a model on this computer, which the test steps through.
    fn loading(config: &str, recorder: Recorder) -> (Self, TestEngine, FakeFrames) {
        Self::assemble(
            config,
            recorder,
            (None, None),
            FakeRecognizer::on_device(),
            None,
        )
    }

    /// Like [`loading`](Self::loading), but the microphone keeps only `limit` unread frames.
    fn loading_with_queue(
        config: &str,
        recorder: Recorder,
        limit: usize,
    ) -> (Self, TestEngine, FakeFrames) {
        Self::assemble(
            config,
            recorder,
            (None, None),
            FakeRecognizer::on_device(),
            Some(limit),
        )
    }

    /// A rig whose engine also has an ending spotter that hears what the test scripts.
    fn spotting(
        config: &str,
        recorder: Recorder,
    ) -> (Self, SpotterControl, TestEngine, FakeFrames) {
        let (spotter, control) = FakeSpotter::new();
        let (rig, engine, frames) = Self::build(config, recorder, Some(spotter), None);
        (rig, control, engine, frames)
    }

    fn build(
        config: &str,
        recorder: Recorder,
        spotter: Option<FakeSpotter>,
        spotter_off: Option<&str>,
    ) -> (Self, TestEngine, FakeFrames) {
        Self::assemble(
            config,
            recorder,
            (spotter, spotter_off),
            FakeRecognizer::new(),
            None,
        )
    }

    fn assemble(
        config: &str,
        recorder: Recorder,
        (spotter, spotter_off): (Option<FakeSpotter>, Option<&str>),
        (recognizer, server): (FakeRecognizer, FakeControl),
        queue: Option<usize>,
    ) -> (Self, TestEngine, FakeFrames) {
        let config = DesktopConfig::from_json_str(config).unwrap();
        let gate = InputGate::default();
        let (frames, feed) = match queue {
            Some(limit) => FakeFrames::capped(&gate, limit),
            None => FakeFrames::new(&gate),
        };
        let (wake_detector, wake) = FakeWake::new();
        let (cue_player, cues) = FakeCuePlayer::new();
        let notices = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&notices);
        let (key, hold) = HoldInput::new();
        let (engine, handle) = Engine::new(
            Parts {
                recognizer,
                wake: wake_detector,
                cues: cue_player,
                gate: gate.clone(),
                chat: None,
                http: reqwest::Client::new(),
                hold,
                notice: Box::new(move |notice| sink.borrow_mut().push(notice.to_owned())),
                spotter: spotter.map(|spotter| Box::new(spotter) as Box<dyn KeywordSpotter>),
                spotter_off: spotter_off.map(str::to_owned),
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
        frames_of(self.server.calls())
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

fn frames_of(calls: Vec<FakeCall>) -> Vec<Frame> {
    calls
        .into_iter()
        .filter_map(|call| match call {
            FakeCall::Frame(frame) => Some(*frame),
            _ => None,
        })
        .collect()
}

/// Runs a wake then a capture on the engine, as a caller would.
fn capture_task(engine: TestEngine, frames: FakeFrames) -> JoinHandle<CaptureOutcome> {
    capture_in(engine, frames, WakeTrigger::Capture, None)
}

fn capture_in(
    mut engine: TestEngine,
    mut frames: FakeFrames,
    trigger: WakeTrigger,
    window: Option<WindowApp>,
) -> JoinHandle<CaptureOutcome> {
    tokio::task::spawn_local(async move {
        assert_eq!(engine.listen(&mut frames).await.unwrap(), trigger);
        let outcome = engine.capture(&mut frames, window).await;
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

fn failed(error: CaptureError) -> CaptureOutcome {
    CaptureOutcome::Failed {
        error,
        recoverable_text: None,
    }
}

fn accepted(route: CaptureRoute, text: &str) -> CaptureOutcome {
    CaptureOutcome::Accepted(CaptureRequest {
        route,
        text: text.to_owned(),
    })
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
async fn a_buffered_wake_capture_uploads_after_the_voice_pause() {
    LocalSet::new()
        .run_until(async {
            let config = r#"{"ending_pause":{"paste":1.0}}"#;
            let (mut rig, engine, frames) = Rig::new(config);
            rig.server.buffer_audio();
            let task = capture_task(engine, frames);
            rig.open().await;

            rig.feed_until(1200, Kind::Voice).await;
            rig.feed_until(2120, Kind::Quiet).await;
            assert!(matches!(rig.state(), EngineState::Capturing { .. }));
            assert!(!rig.server.calls().contains(&FakeCall::FinishAudio));

            rig.feed_until(3500, Kind::Quiet).await;
            assert_eq!(
                rig.state(),
                EngineState::TranscribingLive {
                    route: CaptureRoute::Paste
                }
            );
            assert_eq!(
                rig.server
                    .calls()
                    .iter()
                    .filter(|call| matches!(call, FakeCall::FinishAudio))
                    .count(),
                1
            );
            rig.at(8_000).await;
            assert_eq!(
                rig.state(),
                EngineState::TranscribingLive {
                    route: CaptureRoute::Paste
                },
                "a buffered upload does not use the microphone watchdog"
            );
            assert!(!task.is_finished(), "the pending upload can still answer");
            rig.finals("voice after a pause").await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "voice after a pause")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_buffered_hold_reports_a_microphone_stall_before_release() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            rig.server.buffer_audio();
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.feed_until(1000, Kind::Voice).await;
            rig.at(3960).await;
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error: CaptureError::MicStopped,
                    recoverable_text: None,
                },
                "a buffered upload starts only after release and cannot disable the live microphone watchdog"
            );
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
                    partial: None,
                    notice: None,
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

const LOAD_FAILED: &str = "Could not load the speech model.";

#[tokio::test(start_paused = true)]
async fn a_dictation_waits_for_a_model_read_from_disk_and_sends_what_was_said_meanwhile() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::loading("{}", Recorder::disabled());
            let task = capture_task(engine, frames);
            rig.wake_word().await;
            rig.at(230).await;
            assert_eq!(rig.state(), EngineState::LoadingModel);
            rig.feed_until(20_000, Kind::Quiet).await;
            assert!(
                !task.is_finished(),
                "no fixed limit while the model is read"
            );
            assert_eq!(rig.state(), EngineState::LoadingModel);
            assert_eq!(rig.sent().len(), 0);

            rig.server.ready();
            settle().await;
            let said: Vec<Frame> = (3..=250).map(quiet).collect();
            assert_eq!(tags(&rig.sent()), tags(&said));
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: String::new(),
                    partial: None,
                    notice: None,
                }
            );
            rig.finals("Hello paste that").await;
            rig.feed_until(21_500, Kind::Quiet).await;
            assert_eq!(finished(task).await, accepted(CaptureRoute::Paste, "Hello"));
        })
        .await;
}

fn speech_download(export: Export) -> Download {
    let model = CatalogSpeech::Unified160ms;
    Download {
        export,
        size: match export {
            Export::Gpu => model.gpu_assets().expect("unified gpu files").size(),
            Export::Cpu => model.assets().size(),
        },
    }
}

#[tokio::test(start_paused = true)]
async fn a_dictation_that_cannot_wait_for_the_model_ends_when_it_does_and_says_why() {
    #[derive(Clone, Copy)]
    enum Happens {
        /// The load is already at this step when the wake word comes.
        DownloadingAtWake(Step),
        /// The load reads from disk for 12 s, then this happens.
        Download(Step),
        LoadFails,
        Cancel,
    }
    let downloading = |percent| CaptureError::ModelDownloading {
        speech: SpeechSource::on_device("On this computer", "nemotron_160ms".to_owned()),
        percent,
    };
    // What happens, how the dictation ends, and when the error cue plays.
    let cases = [
        (
            Happens::DownloadingAtWake(Step::Fetching(speech_download(Export::Gpu))),
            failed(downloading(0)),
            Some(0),
        ),
        (
            Happens::DownloadingAtWake(Step::Downloading(speech_download(Export::Cpu), 45)),
            failed(downloading(45)),
            Some(0),
        ),
        (
            Happens::Download(Step::Downloading(speech_download(Export::Gpu), 7)),
            failed(downloading(7)),
            Some(12_000),
        ),
        (
            Happens::LoadFails,
            failed(CaptureError::Recognizer(RecognizerError::said(LOAD_FAILED))),
            Some(12_000),
        ),
        (
            Happens::Cancel,
            CaptureOutcome::Cancelled { dictated: None },
            None,
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (case, (happens, expected, error_cue)) in cases.into_iter().enumerate() {
                let label = format!("case {case}");
                let dir = record_dir(&format!("loading-{case}"));
                let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
                let (mut rig, engine, frames) = Rig::loading("{}", recorder);
                let task = capture_task(engine, frames);
                if let Happens::DownloadingAtWake(step) = happens {
                    rig.server.step(step);
                }
                rig.wake_word().await;
                if !matches!(happens, Happens::DownloadingAtWake(_)) {
                    rig.at(11_999).await;
                    assert!(!task.is_finished(), "{label}: still waiting");
                    rig.at(12_000).await;
                }
                match happens {
                    Happens::DownloadingAtWake(_) => {}
                    Happens::Download(step) => rig.server.step(step),
                    Happens::LoadFails => rig.server.fail(LOAD_FAILED),
                    Happens::Cancel => rig.handle.cancel(),
                }
                settle().await;
                assert_eq!(finished(task).await, expected, "{label}");
                assert_eq!(
                    rig.started(Cue::Error).first().copied(),
                    error_cue,
                    "{label}"
                );
                assert!(rig.gate.is_enabled(), "{label}");
                assert_eq!(rig.state(), EngineState::Listening, "{label}");
                let lines = read_lines(&dir.join("metrics.jsonl"));
                let ready = lines
                    .iter()
                    .find(|line| line["event"] == "desktop_transcription_ready")
                    .unwrap();
                let (outcome, waited) = match happens {
                    Happens::DownloadingAtWake(_) => ("downloading", 0.0),
                    Happens::Download(_) => ("downloading", 12_000.0),
                    Happens::LoadFails => ("error", 12_000.0),
                    Happens::Cancel => ("cancelled", 12_000.0),
                };
                assert_eq!(ready["outcome"], outcome, "{label}");
                assert!(
                    (ready["duration_ms"].as_f64().unwrap() - waited).abs() < 0.5,
                    "{label}: the wait is what the user waited"
                );
            }
        })
        .await;
}

/// Unread 80 ms frames the microphone queue holds before the next one is dropped: 10 s.
const MICROPHONE_QUEUE: u64 = 125;

#[tokio::test(start_paused = true)]
async fn a_hold_released_during_a_long_model_load_is_delivered_and_the_microphone_is_not_dropped() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::loading_with_queue(
                "{}",
                Recorder::disabled(),
                usize::try_from(MICROPHONE_QUEUE).unwrap(),
            );
            let task = hold_task(engine, frames);
            rig.press().await;
            rig.feed_until(1_000, Kind::Voice).await;
            rig.release(1_000).await;
            // 1040 ms is the first frame after the release. One past the queue, still unread
            // while the model loads, is what stops the capture.
            let overflow_at = 1_040 + MICROPHONE_QUEUE * FRAME_MS;
            rig.feed_until(overflow_at, Kind::Quiet).await;
            assert!(
                !task.is_finished(),
                "the load is still running when the queue would have overflowed"
            );
            rig.server.ready();
            settle().await;
            rig.partial("hello world").await;
            rig.at(overflow_at + 250).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "hello world")
            );
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
                let _previews = rig.show_live_submit_previews();
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
            rig.show_live_submit().await;
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
                    partial: Some("more words".to_owned()),
                    notice: None,
                }
            );

            rig.feed_until(3000, Kind::Quiet).await;
            rig.finals("more words submit").await;
            rig.show_live_submit().await;
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
    let paste = (SLOW_PASTE, "Hello paste that", CaptureRoute::Paste, "Hello");
    let command = (
        COMMANDS,
        "open google.com",
        CaptureRoute::Task,
        "open google.com",
    );
    // What the user does after the ending at 1000, and, in its last step, 1 ms before it is accepted.
    let cases: [(&str, _, &[(u64, Kind)]); 4] = [
        // Observed on Hyprland: talking from 1.5 s, no partial before the old 2.5 s countdown ended.
        (
            "talking",
            paste,
            &[
                (2480, Kind::Quiet),
                (3400, Kind::Voice),
                (5939, Kind::Quiet),
            ],
        ),
        // A cough: three speech frames, no words.
        (
            "cough",
            paste,
            &[
                (2000, Kind::Quiet),
                (2240, Kind::Voice),
                (4819, Kind::Quiet),
            ],
        ),
        // Speaking again after a short quiet gap pauses the countdown again.
        (
            "resuming",
            paste,
            &[
                (2000, Kind::Quiet),
                (2240, Kind::Voice),
                (3000, Kind::Quiet),
                (3400, Kind::Voice),
                (5939, Kind::Quiet),
            ],
        ),
        (
            "cough during a command",
            command,
            &[
                (2000, Kind::Quiet),
                (2240, Kind::Voice),
                (4819, Kind::Quiet),
            ],
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (name, (config, said, route, text), script) in cases {
                let (mut rig, engine, frames) = Rig::new(config);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals(said).await;
                let pending = |waiting_for_quiet| EngineState::PendingEnding {
                    route,
                    text: text.to_owned(),
                    waiting_for_quiet,
                    preview: None,
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
                assert_eq!(finished(task).await, accepted(route, text), "{name}");
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
                    partial: Some("and also".to_owned()),
                    notice: None,
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
async fn a_spoken_never_mind_cancels_at_once_and_keeps_what_was_dictated_before_it_without_the_phrase()
 {
    LocalSet::new()
        .run_until(async {
            // (config, finals, what is kept, provisional cues played before "never mind")
            for (config, finals, dictated, provisional) in [
                (
                    "{}",
                    &["Second check that I drop. Never mind."][..],
                    Some("Second check that I drop."),
                    0,
                ),
                ("{}", &["Draft words", "never mind"], Some("Draft words"), 0),
                (
                    "{}",
                    &["First part.", "Second part. Never mind."],
                    Some("First part. Second part."),
                    0,
                ),
                ("{}", &["never mind"], None, 0),
                // A command waiting out its pause.
                (
                    COMMANDS,
                    &["open google.com", "never mind"],
                    Some("open google.com"),
                    1,
                ),
            ] {
                let (mut rig, engine, frames) = Rig::new(config);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                for words in finals {
                    rig.finals(words).await;
                }
                assert_eq!(rig.started(Cue::Cancelled), [1000], "{finals:?}");
                rig.at(1100).await;
                assert!(task.is_finished(), "no pause is waited out: {finals:?}");
                assert_eq!(
                    rig.started(Cue::Provisional).len(),
                    provisional,
                    "{finals:?}"
                );
                assert_eq!(rig.server.calls().last(), Some(&FakeCall::Cancel));
                assert!(rig.gate.is_enabled(), "{finals:?}");
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
        preview: None,
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
            &[(1000, "Open Google dot com.")],
            CaptureRoute::Task,
            "Open Google dot com.",
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
                rig.show_live_submit().await;
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
async fn a_command_without_a_spoken_ending_does_not_agree_with_a_spotter_hit_and_is_recorded_apart()
{
    LocalSet::new()
        .run_until(async {
            for (said, expected, states) in [
                (
                    "open google.com",
                    "ended",
                    ["pending_without_ending", "accepted_without_ending"],
                ),
                ("open google.com do that", "agreed", ["pending", "accepted"]),
            ] {
                let dir = record_dir("command-spotter");
                let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
                let (mut rig, spot, engine, frames) = Rig::spotting(COMMANDS, recorder);
                spot.hear(HEARD_AT_FRAME, "do that", Ending::Task);
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
                let rows = read_lines(&dir.join("metrics.jsonl"));
                let hit = rows
                    .iter()
                    .find(|row| row["event"] == "desktop_ending_spotter" && row["call"] == "hit")
                    .unwrap();
                assert_eq!(hit["outcome"], expected, "{said}");
                let seen: Vec<String> = events(&rows)
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
                    partial: Some("and then".to_owned()),
                    notice: None,
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
                        partial: None,
                        notice: None,
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
                    match moment {
                        Moment::AcceptedCue => accepted(CaptureRoute::Paste, "Hello"),
                        _ => CaptureOutcome::Cancelled {
                            dictated: (!matches!(moment, Moment::Starting))
                                .then(|| "Hello".to_owned()),
                        },
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
        speech: SpeechSource::direct("Realtime speech server", "ws://127.0.0.1:8080".to_owned()),
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
                let (mut rig, engine, frames) =
                    Rig::new(
                    r#"{"stt":{"entries":[{"kind":"realtime","id":"s","name":"Realtime speech server","active":true,"address":"ws://127.0.0.1:8080","model":"m"}]}}"#,
                );
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
    for (name, config, listener, message) in [
        (
            "Nemotron",
            "{}",
            EndingListener::Other,
            "Didn't hear an ending for 1 minute.",
        ),
        (
            "Parakeet",
            r#"{"stt": {"engine": "local", "model": "parakeet_tdt_v3"}}"#,
            EndingListener::Parakeet,
            "Didn't hear an ending for 1 minute. Parakeet may miss an English ending after other languages; add one in your language in Settings → Endings.",
        ),
    ] {
        LocalSet::new()
            .run_until(async {
                let (mut rig, engine, frames) = Rig::new(config);
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
                    "{name}: words at 40 s restart the minute; noise does not"
                );
                rig.feed_until(100_080, Kind::Quiet).await;
                let error = CaptureError::NoEnding {
                    idle: Duration::from_secs(60),
                    listener,
                };
                assert_eq!(error.to_string(), message, "{name}");
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Failed {
                        error,
                        recoverable_text: Some("Hello world".to_owned())
                    },
                    "{name}"
                );
            })
            .await;
    }
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
            rig.show_live_submit().await;
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
            let idle = CaptureError::NoEnding {
                idle: Duration::from_secs(20),
                listener: EndingListener::Other,
            };
            assert_eq!(idle.to_string(), "Didn't hear an ending for 20 seconds.");
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
        (0, "0 seconds"),
        (10, "10 seconds"),
        (60, "1 minute"),
        (90, "90 seconds"),
        (120, "2 minutes"),
        (600, "10 minutes"),
        (3600, "60 minutes"),
    ] {
        let seconds = Duration::from_secs(seconds);
        assert_eq!(
            CaptureError::NoWords(seconds).to_string(),
            format!("No words heard for {words}.")
        );
        assert_eq!(
            CaptureError::NoEnding {
                idle: seconds,
                listener: EndingListener::Other,
            }
            .to_string(),
            format!("Didn't hear an ending for {words}.")
        );
        assert_eq!(
            CaptureError::TooLong(seconds).to_string(),
            format!("Capture stopped after {words} without an ending.")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_dictation_wake_is_reported_and_closes_the_microphone_gate() {
    LocalSet::new()
        .run_until(async {
            let (rig, mut engine, mut frames) = Rig::new("{}");
            let task = tokio::task::spawn_local(async move { engine.listen(&mut frames).await });
            rig.wake.score_and_detect(0.8, 0.7);
            rig.feed.send(audible(1));
            assert_eq!(finished(task).await.unwrap(), WakeTrigger::Capture);
            assert!(!rig.gate.is_enabled(), "a capture wake closes the gate");
            assert_eq!(rig.notices(), Vec::<String>::new());
            assert_eq!(rig.wake.resets(), 1);
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

/// A capture that says "live words paste that" at 1000 and is delivered at 3500.
async fn live_words(rig: &mut Rig) {
    rig.open().await;
    live_words_after_open(rig).await;
}

/// The rest of [`live_words`], for a capture the test opened itself.
async fn live_words_after_open(rig: &mut Rig) {
    rig.feed_until(1000, Kind::Quiet).await;
    rig.finals("live words paste that").await;
    rig.feed_until(3499, Kind::Quiet).await;
    rig.at(3500).await;
}

const ALL: RecordKinds = RecordKinds {
    metrics: true,
    text: true,
    audio: false,
};

/// A chat answer that streams `$text` and ends.
macro_rules! says {
    ($text:literal) => {
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"",
            $text,
            "\"}}]}\n\ndata: [DONE]\n\n"
        )
    };
}

#[tokio::test(start_paused = true)]
async fn the_recognizer_text_is_delivered_without_asking_a_configured_chat_model() {
    LocalSet::new()
        .run_until(async {
            let (chat_url, mut asked, _feed) =
                chat(Chat::After(0, says!("merged words paste that"))).await;
            let (mut rig, engine, frames) = Rig::new(&with_writing(&chat_url));
            let task = capture_task(engine, frames);
            live_words(&mut rig).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "live words"),
                "with cleanup off, the recognizer's text is delivered as heard"
            );
            assert!(
                asked.try_recv().is_err(),
                "the chat model must not be asked: cleanup is off"
            );
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
                notice: None,
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
                preview: None,
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
            let (chat_url, _asked, _feed) = chat(Chat::Stream(says!("Polished words"))).await;
            let recorder = Recorder::in_dir(all, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording(&cleaning(&chat_url), recorder);
            let task = capture_task(engine, frames);
            live_words(&mut rig).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "Polished words")
            );
            let metrics = read_lines(&dir.join("metrics.jsonl"));
            assert_eq!(capture_flow_sources(&dir), ["microphone"]);
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
                ("desktop_cleanup", "cleaned"),
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
            let ready = metrics
                .iter()
                .find(|line| line["event"] == "desktop_transcription_ready")
                .unwrap();
            assert_eq!(ready["entry"], "On this computer");

            let folder = dictation_dirs(&dir).pop().unwrap();
            let text = std::fs::read_to_string(folder.join("text.txt")).unwrap();
            assert!(text.starts_with("heard: live words paste that\n"), "{text}");
            assert!(text.contains("Polished words"), "{text}");
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
async fn the_delay_from_speech_to_the_recognizers_words_is_recorded_once_for_partials_and_per_final()
 {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("lags");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, spot, engine, frames) = Rig::spotting(
                r#"{"stt":{"entries":[{"kind":"realtime","id":"test","name":"Metrics recognizer","active":true,"address":"ws://127.0.0.1:8080","model":"m"}]}}"#,
                recorder,
            );
            spot.hear(HEARD_AT_FRAME, "paste that", Ending::Paste);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.feed_until(1500, Kind::Voice).await;
            rig.partial("live").await;
            rig.feed_until(1900, Kind::Voice).await;
            rig.partial("live words").await;
            rig.feed_until(2200, Kind::Quiet).await;
            rig.finals("live words paste that").await;
            rig.feed_until(3499, Kind::Quiet).await;
            rig.at(3500).await;
            finished(task).await;
            let lines = read_lines(&dir.join("metrics.jsonl"));
            let ready = lines
                .iter()
                .find(|line| line["event"] == "desktop_transcription_ready")
                .unwrap();
            assert_eq!(ready["entry"], "Metrics recognizer");
            let lags = |event: &str| -> Vec<f64> {
                lines
                    .iter()
                    .filter(|line| line["event"] == event)
                    .map(|line| line["duration_ms"].as_f64().unwrap())
                    .collect()
            };
            // The detector flags speech by its own rules, so only the span it can lie in is known.
            let first_voice = (1000 / FRAME_MS + 1) * FRAME_MS;
            let partial = lags("desktop_partial_lag");
            let last = lags("desktop_final_lag");
            assert_eq!(partial.len(), 1, "two partials, one lag: {partial:?}");
            assert_eq!(last.len(), 1, "{last:?}");
            #[expect(clippy::cast_precision_loss, reason = "small test times")]
            let ms = |at: u64| at as f64;
            assert!(
                (0.0..=ms(1500 - first_voice)).contains(&partial[0]),
                "{partial:?}"
            );
            assert!(
                (0.0..=ms(2200 - first_voice)).contains(&last[0]) && last[0] > 0.0,
                "{last:?}"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_capture_records_which_kind_of_failure_and_its_calls() {
    LocalSet::new()
        .run_until(async {
            for (behind, reason) in [(false, "recognizer"), (true, "cant_keep_up")] {
                let dir = record_dir(if behind { "cant-keep-up" } else { "failed" });
                let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
                let (mut rig, engine, frames) = Rig::recording("{}", recorder);
                let task = capture_task(engine, frames);
                if behind {
                    rig.open().await;
                    rig.feed_until(1_000, Kind::Quiet).await;
                    rig.server.emit(Err(RecognizerError::Behind {
                        model: "Nemotron 160ms".to_owned(),
                        lag: Duration::from_millis(3_040),
                    }));
                } else {
                    rig.wake_word().await;
                    rig.server
                        .fail("Thalvi could not reach speech recognition.");
                }
                let outcome = finished(task).await;
                let CaptureOutcome::Failed { error, .. } = &outcome else {
                    panic!("the capture fails: {outcome:?}");
                };
                let lines = read_lines(&dir.join("metrics.jsonl"));
                let flow = lines
                    .iter()
                    .find(|line| line["event"] == "desktop_capture_flow")
                    .unwrap();
                assert_eq!(
                    (flow["outcome"].as_str(), flow["reason"].as_str()),
                    (Some("failed"), Some(reason)),
                    "a failure says which kind"
                );
                if behind {
                    assert_eq!(
                        error.to_string(),
                        "This computer can't keep up with Nemotron 160ms. Nothing was delivered."
                    );
                    assert_eq!(error.headline(), "Nemotron 160ms is too slow");
                    assert!(rig.gate.is_enabled(), "listening goes on");
                    continue;
                }
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

/// Listens until a dictation wake, then writes the recordings out.
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
            // Per-frame dictation scores; the threshold is set to 0.5.
            let frames = [0.0, 0.1, 0.2, 0.3004, 0.2, 0.0, 0.15, 0.1, 0.0, 0.0, 0.3];
            let dir = record_dir("near-miss");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let config = r#"{"wake":{"capture":{"model":"hey_thalvi","threshold":0.5}}}"#;
            let (rig, engine, frames_in) = Rig::recording(config, recorder);
            let task = listen_task(engine, frames_in, 1);
            for (id, score) in frames.into_iter().enumerate() {
                rig.wake.score(score);
                hear(&rig, id, id).await;
            }
            rig.wake.score(0.6);
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
            let cases: [Case; 6] = [
                ("faint", WakeEvent::Capture, 0, 1_499, 0, Some(1_499)),
                ("at the floor", WakeEvent::Capture, 0, 1_500, 0, None),
                (
                    "negative faint",
                    WakeEvent::Capture,
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
                rig.wake.score(0.9);
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
                        rig.wake.score(0.0);
                        rig.feed.send(audible(22));
                        settle().await;
                        rig.wake.score_and_detect(0.9, 0.7);
                        rig.feed.send(audible(23));
                        finished(task).await;
                        let lines = wake_lines(&dir, "desktop_wake_refused");
                        assert_eq!(lines.len(), 1, "{name}: one line for the run of frames");
                        assert_eq!(lines[0]["level"], json!(level), "{name}");
                        assert_eq!(lines[0]["floor"], json!(1_500), "{name}");
                        assert_eq!(lines[0]["capture"], json!(0.9), "{name}");
                        assert_eq!(lines[0]["outcome"], "capture", "{name}");
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
async fn a_dictation_wake_gets_a_folder_with_the_last_two_seconds() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("dictation-wake-clip");
            let recorder = Recorder::in_dir(AUDIO_AND_METRICS, &dir).unwrap();
            let (rig, engine, frames) = Rig::recording("{}", recorder);
            let task = listen_task(engine, frames, 1);
            hear(&rig, 1, 30).await;
            rig.wake.score(0.8);
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 31, 31).await;
            finished(task).await;

            let clips = wake_clips(&dir);
            let [(_, samples)] = clips.as_slice() else {
                panic!("one dictation wake clip expected: {clips:?}");
            };
            let tags: Vec<i16> = samples
                .as_chunks::<FRAME_SAMPLES>()
                .0
                .iter()
                .map(|frame| frame[0])
                .collect();
            assert_eq!(tags.len(), 25);
            assert_eq!(tags[24] - tags[0], 24, "the last 25 frames");
            assert_eq!(usize::try_from(tags[24]).unwrap(), 31);
            assert_eq!(
                wake_lines(&dir, "desktop_wake_nothing"),
                Vec::<serde_json::Value>::new()
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_clip_holds_only_what_was_heard_since_listening_began() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("fresh-clip");
            let recorder = Recorder::in_dir(AUDIO_AND_METRICS, &dir).unwrap();
            let (rig, mut engine, mut frames) = Rig::recording("{}", recorder);
            let (first_listen_done, wait_for_first_listen) = tokio::sync::oneshot::channel();
            let task = tokio::task::spawn_local(async move {
                engine.listen(&mut frames).await.unwrap();
                first_listen_done.send(()).unwrap();
                engine.listen(&mut frames).await.unwrap();
                engine.finish().await;
            });
            hear(&rig, 1, 10).await;
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 11, 11).await;
            wait_for_first_listen.await.unwrap();
            // Audio between two listens is never scored, so it must not join the next clip.
            rig.gate.set_enabled(true);
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
            rig.wake.fire(WakeEvent::Capture);
            hear(&rig, 11, 11).await;
            hear(&rig, 20, 21).await;
            rig.gate.set_enabled(true);
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
                ("metrics", METRICS_ONLY, &["metrics.jsonl"][..]),
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
async fn a_new_ending_withholds_audio_again_after_speech_was_relayed() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello submit").await;
            rig.show_live_submit().await;
            rig.feed_until(1760, Kind::Quiet).await;
            rig.feed_until(2000, Kind::Voice).await;
            rig.finals("submit").await;
            rig.show_live_submit().await;
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
                preview: None,
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
            rig.show_live_submit().await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.finals("and then some more").await;
            assert_eq!(rig.started(Cue::Cancelled), [1500]);
            assert_eq!(
                rig.state(),
                EngineState::Capturing {
                    committed: "Hello submit and then some more".to_owned(),
                    partial: None,
                    notice: None,
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
                    CaptureError::NoEnding {
                        idle: Duration::from_secs(10),
                        listener: EndingListener::Other,
                    },
                ),
                (
                    "idle without words",
                    r#"{"idle": 10, "max": 600}"#,
                    false,
                    CAPTURE_OPENS + 10_000,
                    CaptureError::NoWords(Duration::from_secs(10)),
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
            rig.show_live_submit().await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.partial("more words").await;
            rig.feed_until(3000, Kind::Quiet).await;
            rig.finals("more words submit").await;
            rig.show_live_submit().await;
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
async fn every_cue_records_when_it_started_when_it_is_audible_and_what_the_user_waited_and_nothing_said()
 {
    LocalSet::new()
        .run_until(async {
            for rebuilt in [false, true] {
                let timing = CueTiming {
                    start: ms(40),
                    output: Some(ms(15)),
                    rebuilt,
                };
                let name = format!("cue-timing-{rebuilt}");
                let dictation =
                    cue_lines(&format!("{name}-dictation"), timing, Run::Dictation).await;
                assert_eq!(
                    cue_names(&dictation),
                    [
                        "wake",
                        "provisional",
                        "cancelled",
                        "provisional",
                        "accepted"
                    ]
                );
                let mut lines = dictation;
                lines.extend(cue_lines(&format!("{name}-failure"), timing, Run::Failure).await);
                let mut kinds = cue_names(&lines);
                kinds.sort_unstable();
                kinds.dedup();
                assert_eq!(
                    kinds,
                    ["accepted", "cancelled", "error", "provisional", "wake"]
                );
                let mut expected_keys = vec![
                    "audible_ms",
                    "cue",
                    "duration_ms",
                    "event",
                    "outcome",
                    "output_ms",
                    "perceived_ms",
                    "start_ms",
                    "ts_ms",
                ];
                if rebuilt {
                    expected_keys.push("rebuilt");
                    expected_keys.sort_unstable();
                }
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
                    let mut keys: Vec<&str> = line
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect();
                    keys.sort_unstable();
                    assert_eq!(
                        keys, expected_keys,
                        "only rebuilt cues carry rebuilt, and timings only: {line}"
                    );
                    assert_eq!(
                        line.get("rebuilt"),
                        rebuilt.then_some(&serde_json::Value::Bool(true)),
                        "only rebuilt cues carry rebuilt: true"
                    );
                    assert!(!line.to_string().contains("more words"), "{line}");
                }
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
                let outcome = engine.capture(&mut frames, None).await;
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
                rebuilt: false,
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

/// Runs a hold then its capture on the engine, as a caller would.
fn hold_task(engine: TestEngine, frames: FakeFrames) -> JoinHandle<CaptureOutcome> {
    capture_in(engine, frames, WakeTrigger::Hold, None)
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
    let cases: [Case; 5] = [
        (
            &[(1000, Partial("hello world"))],
            1200,
            accepted(CaptureRoute::Paste, "hello world"),
            Cue::Accepted,
            1600,
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
            1600,
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
async fn a_hold_kept_through_the_silence_limit_reports_the_silence_and_keeps_its_text() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Final("Hello world")).await;
            rig.feed_until(60_900, Kind::Quiet).await;
            assert!(!task.is_finished());
            rig.feed_until(61_100, Kind::Quiet).await;
            let error = CaptureError::HeldSilence(Duration::from_secs(60));
            assert_eq!(error.to_string(), "Stopped after 1 minute of silence.");
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Failed {
                    error,
                    recoverable_text: Some("Hello world".to_owned())
                }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_with_no_speech_or_under_the_minimum_pastes_nothing_and_says_why() {
    // (recognizer ready, speech, release, what the user is told)
    type Case = (bool, Option<(u64, Say)>, u64, CaptureError);
    let cases: [Case; 6] = [
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
        (false, None, 300, CaptureError::NothingHeard),
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
                let outcome = engine.capture(&mut frames, None).await;
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
                    partial: None,
                    notice: None,
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
    const CUSTOM: &str = r#"{"endings": {"paste": "paste", "write it": "write", "send it": "submit",
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
                rig.at(1600).await;
                assert_eq!(rig.started(Cue::Accepted), [1600], "hold: {said}");
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
                rig.show_live_submit().await;
                rig.feed_until(4000, Kind::Quiet).await;
                assert_eq!(finished(task).await, wake_outcome, "wake word: {said}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn key_auto_repeat_during_a_hold_starts_nothing() {
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
async fn a_new_press_after_the_callers_last_check_survives_resuming_the_listener() {
    let (rig, mut engine, mut frames) = Rig::new("{}");
    let mut presses = engine.stop_listening();
    rig.key.set(true);
    assert!(
        presses.take().is_some(),
        "the caller accounts for the first press"
    );
    rig.key.set(false);
    rig.key.set(true);
    engine.resume_listening(presses);
    assert_eq!(
        tokio::time::timeout(ms(5), engine.listen(&mut frames))
            .await
            .expect("the next press must start a hold")
            .unwrap(),
        WakeTrigger::Hold
    );
    assert!(
        engine.take_ignored().is_empty(),
        "the first press is counted only once"
    );
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
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_sends_what_is_said_over_its_start_cue() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            rig.cues.mutes(&rig.gate);
            let task = hold_task(engine, frames);
            rig.press().await;
            rig.server.ready();
            settle().await;
            // Said while the cue plays: the user talks from the press on.
            rig.at(100).await;
            rig.feed.send(quiet(5));
            settle().await;
            rig.at(230).await;
            rig.at(240).await;
            rig.feed.send(quiet(9));
            settle().await;
            assert_eq!(tags(&rig.sent()), [5, 9]);
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_wake_word_starts_a_capture_only_with_wake_words_on_and_the_hold_key_always_does() {
    #[derive(Clone, Copy)]
    enum Input {
        Wake,
        Hold,
        WakeThenHold,
    }
    use Input::{Hold, Wake, WakeThenHold};
    const OFF: &str = r#"{"wake": {"enabled": false}}"#;
    // (config, switched before listening, input, what listen returns, whether the wake model scored)
    let cases = [
        ("{}", None, Wake, Some(WakeTrigger::Capture), true),
        ("{}", None, Hold, Some(WakeTrigger::Hold), true),
        (OFF, None, Wake, None, false),
        (OFF, None, Hold, Some(WakeTrigger::Hold), false),
        (OFF, None, WakeThenHold, Some(WakeTrigger::Hold), false),
        ("{}", Some(false), Wake, None, false),
        (OFF, Some(true), Wake, Some(WakeTrigger::Capture), true),
    ];
    LocalSet::new()
        .run_until(async {
            for (case, (config, switched, input, expected, scored)) in cases.into_iter().enumerate()
            {
                let (rig, mut engine, mut frames) = Rig::new(config);
                if let Some(on) = switched {
                    engine.set_wake_words(on);
                }
                let task =
                    tokio::task::spawn_local(
                        async move { engine.listen(&mut frames).await.unwrap() },
                    );
                hear(&rig, 1, 3).await;
                if matches!(input, Wake | WakeThenHold) {
                    rig.wake.fire(WakeEvent::Capture);
                    hear(&rig, 4, 5).await;
                }
                if matches!(input, Hold | WakeThenHold) {
                    rig.press().await;
                }
                if let Some(trigger) = expected {
                    assert_eq!(finished(task).await, trigger, "case {case}");
                } else {
                    assert!(!task.is_finished(), "case {case}: the wake word started it");
                    task.abort();
                }
                assert_eq!(rig.wake.scored() > 0, scored, "case {case}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_that_waited_to_be_told_from_a_chord_keeps_the_words_said_meanwhile() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            settle().await;
            // AltGr goes down alone; the user starts talking before the wait is over.
            let wait = rig.key.press_alone(HoldKind::Dictate);
            rig.at(80).await;
            rig.feed.send(quiet(3));
            settle().await;
            rig.at(160).await;
            rig.feed.send(quiet(4));
            settle().await;
            rig.at(200).await;
            rig.key.confirm_alone(HoldKind::Dictate, wait);
            settle().await;
            rig.server.ready();
            settle().await;
            // The cue plays from 200 to 430, and what is said over it is sent too.
            rig.at(300).await;
            rig.feed.send(quiet(5));
            settle().await;
            rig.at(440).await;
            rig.at(450).await;
            rig.feed.send(quiet(9));
            settle().await;
            assert_eq!(tags(&rig.sent()), [3, 4, 5, 9]);
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled { dictated: None }
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn an_immediate_hold_keeps_two_frames_of_pre_roll_heard_before_the_press() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = hold_task(engine, frames);
            settle().await;
            for tag in [1, 2, 3, 4] {
                rig.feed.send(quiet(tag));
                settle().await;
            }
            rig.press().await;
            rig.server.ready();
            settle().await;
            rig.at(100).await;
            rig.feed.send(quiet(5));
            settle().await;
            rig.at(240).await;
            rig.feed.send(quiet(9));
            settle().await;
            assert_eq!(tags(&rig.sent()), [3, 4, 5, 9]);
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
                        let outcome = engine.capture(&mut frames, None).await;
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
                let outcome = engine.capture(&mut frames, None).await;
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
                let outcome = engine.capture(&mut frames, None).await;
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
                    assert_eq!(line["outcome"], "refused");
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
                let outcome = engine.capture(&mut frames, None).await;
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
                    engine.capture(&mut frames, None).await
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
            rig.at(1220).await;
            rig.press_write().await;
            rig.release_write(1220).await;
            assert_eq!(
                rig.sent(),
                sent,
                "a second key press does not flush the released hold again"
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
async fn a_hold_without_a_replaying_final_pastes_at_the_release_wait_or_the_recognizer_limit() {
    // (the recognizer's release limit, when the paste comes)
    let cases = [(None, 1450), (Some(ms(400)), 1600)];
    LocalSet::new()
        .run_until(async {
            for (limit, paste_at) in cases {
                let dir = record_dir("short-release-wait");
                let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
                let (mut rig, engine, frames) = Rig::recording("{}", recorder);
                if let Some(limit) = limit {
                    rig.server.release_limit(limit);
                }
                let task = hold_task(engine, frames);
                rig.hold().await;
                rig.say(1000, Say::Partial("partial text")).await;
                rig.release(1200).await;
                rig.at(paste_at - 1).await;
                assert!(!task.is_finished(), "{limit:?}: not yet at {paste_at}");
                assert!(
                    rig.started(Cue::Accepted).is_empty(),
                    "{limit:?}: no paste before the wait"
                );
                assert_eq!(
                    rig.state(),
                    EngineState::Capturing {
                        committed: String::new(),
                        partial: Some("partial text".to_owned()),
                        notice: None,
                    },
                    "{limit:?}: an ordinary release shows no finishing state during the wait"
                );
                rig.at(paste_at).await;
                assert_eq!(rig.started(Cue::Accepted), [paste_at], "{limit:?}");
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Paste, "partial text"),
                    "{limit:?}"
                );
                let waited = read_lines(&dir.join("metrics.jsonl"))
                    .into_iter()
                    .find(|line| line["event"] == "desktop_release_wait");
                assert!(
                    waited.is_none(),
                    "{limit:?}: a caught-up release records no finishing wait, only the model's latency: {waited:?}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_released_while_the_recognizer_is_behind_waits_for_its_final_and_pastes_it_all() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("release-wait-caught-up");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            rig.server.behind(true);
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Partial("please bring all the coffee"))
                .await;
            rig.release(1200).await;
            rig.feed_until(1499, Kind::Quiet).await;
            assert!(
                rig.started(Cue::Accepted).is_empty(),
                "a recognizer still behind at the release is not cut off at the 250 ms wait"
            );
            assert_eq!(
                rig.state(),
                EngineState::Finishing {
                    route: CaptureRoute::Paste
                },
                "the overlay says the hold is finishing"
            );
            // A final for the stretches heard so far, while the rest of the queued audio is not
            // answered yet: it must not end the hold.
            rig.feed_until(2000, Kind::Quiet).await;
            rig.finals("please bring all the coffee").await;
            assert!(
                !task.is_finished(),
                "a final that covers only part of the audio does not end a hold that is behind"
            );
            assert!(
                rig.started(Cue::Accepted).is_empty(),
                "the hold keeps waiting while audio from before the release is unanswered"
            );
            rig.feed_until(2999, Kind::Quiet).await;
            assert!(!task.is_finished(), "the capture is still open");
            rig.server.behind(false);
            rig.at(3000).await;
            rig.finals("we have").await;
            assert_eq!(
                rig.started(Cue::Accepted),
                [3000],
                "the recognizer's final after it caught up pastes at once, with every word"
            );
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "please bring all the coffee we have")
            );
            let waited = read_lines(&dir.join("metrics.jsonl"))
                .into_iter()
                .find(|line| line["event"] == "desktop_release_wait")
                .expect("the finishing wait is recorded");
            assert_eq!(waited["outcome"], "caught_up", "{waited}");
            assert_eq!(waited["duration_ms"], 1800.0, "{waited}");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_ends_when_the_recognizer_answers_the_end_of_its_audio() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("release-wait-ended");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            rig.server.behind(true);
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Partial("reduced our hosting")).await;
            rig.release(1200).await;
            rig.feed_until(1499, Kind::Quiet).await;
            assert_eq!(
                rig.state(),
                EngineState::Finishing {
                    route: CaptureRoute::Paste
                },
                "the overlay says the hold is finishing"
            );
            rig.finals("reduced our hosting bills").await;
            assert!(
                !task.is_finished(),
                "a final does not end a hold whose session is still behind"
            );
            rig.server.emit(Ok(RecognizerEvent::Ended));
            settle().await;
            assert_eq!(
                rig.started(Cue::Accepted),
                [1499],
                "the recognizer's answer for the end of the audio ends the hold at once"
            );
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "reduced our hosting bills")
            );
            let waited = read_lines(&dir.join("metrics.jsonl"))
                .into_iter()
                .find(|line| line["event"] == "desktop_release_wait")
                .expect("the finishing wait is recorded");
            assert_eq!(waited["outcome"], "caught_up", "{waited}");
            assert_eq!(waited["duration_ms"], 299.0, "{waited}");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_still_behind_fifteen_seconds_after_release_pastes_what_it_has_and_says_so() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, mut engine, mut frames) = Rig::new("{}");
            rig.server.behind(true);
            let task = tokio::task::spawn_local(async move {
                assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                let outcome = engine.capture(&mut frames, None).await;
                let note = engine.take_release_note();
                engine.finish().await;
                (outcome, note)
            });
            rig.hold().await;
            rig.say(1000, Say::Partial("please bring all the coffee")).await;
            rig.release(1200).await;
            rig.feed_until(1499, Kind::Quiet).await;
            assert!(
                rig.started(Cue::Accepted).is_empty(),
                "the hold waits past the 250 ms limit"
            );
            rig.feed_until(16_199, Kind::Quiet).await;
            assert!(
                rig.started(Cue::Accepted).is_empty(),
                "the bound is 15 s after the release"
            );
            rig.at(16_200).await;
            assert_eq!(
                rig.started(Cue::Accepted),
                [16_200],
                "still behind at the bound, the hold pastes what it has"
            );
            assert_eq!(
                finished(task).await,
                (
                    accepted(CaptureRoute::Paste, "please bring all the coffee"),
                    Some(
                        "The speech model was still catching up, so the end of the dictation may be missing."
                            .to_owned()
                    ),
                ),
                "the user is told the end may be missing"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_released_hold_while_behind_or_in_the_post_release_wait_delivers_nothing() {
    LocalSet::new()
        .run_until(async {
            for behind in [true, false] {
                let (mut rig, engine, frames) = Rig::new("{}");
                rig.server.behind(behind);
                let task = hold_task(engine, frames);
                rig.hold().await;
                rig.say(1000, Say::Final("hello")).await;
                rig.release(1200).await;
                if behind {
                    rig.feed_until(1500, Kind::Quiet).await;
                } else {
                    rig.at(1250).await;
                }
                rig.handle.cancel();
                settle().await;
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Cancelled {
                        dictated: Some("hello".to_owned())
                    },
                    "behind {behind}: a cancel during the finishing wait delivers nothing"
                );
                assert_eq!(rig.started(Cue::Accepted).len(), 0, "behind {behind}");
                assert_eq!(
                    rig.server.calls().last(),
                    Some(&FakeCall::Cancel),
                    "behind {behind}"
                );
            }
        })
        .await;
}

// ---- Writing a dictation out through the chat endpoint ----

/// What the fake chat endpoint does with a request.
#[derive(Clone, Copy)]
enum Chat {
    /// Streams what the test pushes through the [`ChatFeed`].
    Live,
    Status(u16),
    /// Takes the request and never answers.
    Stall,
    /// Answers with this event stream and ends it.
    Stream(&'static str),
    /// Answers with this event stream, this many milliseconds after the request.
    After(u64, &'static str),
}

/// Pushes streamed answer text to the fake chat endpoint's client.
struct ChatFeed(tokio::sync::mpsc::UnboundedSender<Vec<u8>>);

impl ChatFeed {
    fn text(&self, piece: &str) {
        let event = json!({"choices": [{"delta": {"content": piece}}]});
        self.0
            .send(format!("data: {event}\n\n").into_bytes())
            .unwrap();
    }

    /// Streams reasoning, the way llama.cpp sends it.
    fn thinking(&self, piece: &str) {
        let event = json!({"choices": [{"delta": {"reasoning_content": piece}}]});
        self.0
            .send(format!("data: {event}\n\n").into_bytes())
            .unwrap();
    }

    fn finish(self) {
        self.0.send(b"data: [DONE]\n\n".to_vec()).unwrap();
    }
}

/// What the fake endpoint streams before the test stops feeding it.
#[derive(Clone, Copy, Debug)]
enum Streamed {
    Nothing,
    Thinking,
    Text,
}

impl Streamed {
    /// Feeds it, unless nothing, and waits until the engine shows it.
    async fn feed(self, feed: &ChatFeed, rig: &Rig) {
        match self {
            Self::Nothing => {}
            Self::Thinking => {
                feed.thinking("Anna is late, so");
                rig.state_is(EngineState::Thinking).await;
            }
            Self::Text => {
                feed.text("Hi Anna, I am");
                rig.state_is(EngineState::Writing {
                    text: "Hi Anna, I am".to_owned(),
                })
                .await;
            }
        }
    }
}

/// What the fake chat endpoint was asked.
struct Asked {
    path: String,
    authorization: Option<String>,
    body: serde_json::Value,
}

async fn chat(
    reply: Chat,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<Asked>,
    ChatFeed,
) {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};

    let (asked_tx, asked) = tokio::sync::mpsc::unbounded_channel();
    let (feed_tx, feed_rx) = tokio::sync::mpsc::unbounded_channel();
    let feed_rx = Arc::new(Mutex::new(Some(feed_rx)));
    let app = Router::new().fallback(move |request: Request| {
        let asked_tx = asked_tx.clone();
        let feed_rx = Arc::clone(&feed_rx);
        async move {
            let path = request.uri().path().to_owned();
            let authorization = request
                .headers()
                .get(header::AUTHORIZATION)
                .map(|value| value.to_str().unwrap().to_owned());
            let body = to_bytes(request.into_body(), usize::MAX).await.unwrap();
            let _ = asked_tx.send(Asked {
                path,
                authorization,
                body: serde_json::from_slice(&body).unwrap(),
            });
            let events = |body: Body| -> Response {
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(body)
                    .unwrap()
            };
            match reply {
                Chat::Live => {
                    let rx = feed_rx.lock().unwrap().take().unwrap();
                    events(Body::from_stream(futures_util::stream::unfold(
                        rx,
                        |mut rx| async move {
                            rx.recv()
                                .await
                                .map(|chunk| (Ok::<_, std::convert::Infallible>(chunk), rx))
                        },
                    )))
                }
                Chat::Status(code) => StatusCode::from_u16(code).unwrap().into_response(),
                Chat::Stall => std::future::pending().await,
                Chat::Stream(text) => events(Body::from(text)),
                Chat::After(delay, text) => {
                    tokio::time::sleep(ms(delay)).await;
                    events(Body::from(text))
                }
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, asked, ChatFeed(feed_tx))
}

/// The next request the fake chat endpoint got, failing instead of waiting forever when none comes.
async fn request(asked: &mut tokio::sync::mpsc::UnboundedReceiver<Asked>) -> Asked {
    tokio::time::timeout(Duration::from_secs(5), asked.recv())
        .await
        .expect("no request reached the writing endpoint")
        .unwrap()
}

fn with_writing(url: &str) -> String {
    format!(
        r#"{{"ending_pause": {{"paste": 2.5}}, "writing": {{"url": "{url}", "model": "m", "api_key": "k"}}}}"#
    )
}

const WORDS: &str = "tell Anna I'm late";

/// The two ways to ask for a dictation to be written out.
#[derive(Clone, Copy, Debug)]
enum Ask {
    /// The spoken ending "write that out" in a wake-word capture.
    Ending,
    /// Holding the write key.
    Key,
}

impl Rig {
    async fn press_write(&self) {
        settle().await;
        self.key.set_write(true);
        settle().await;
    }

    async fn release_write(&self, at: u64) {
        self.at(at).await;
        self.key.set_write(false);
        settle().await;
    }

    /// Waits, for a few seconds of the paused clock, until the engine is in `expected`.
    async fn state_is(&self, expected: EngineState) {
        let mut state = self.handle.state();
        let reached = tokio::time::timeout(
            Duration::from_secs(5),
            state.wait_for(|state| *state == expected),
        )
        .await;
        assert!(
            reached.is_ok(),
            "the state never became {expected:?}; it is {:?}",
            self.state()
        );
    }

    async fn show_live_submit(&self) {
        if let EngineState::PendingEnding {
            route: CaptureRoute::Submit,
            preview: Some(preview),
            ..
        } = self.state()
        {
            preview.shown();
            settle().await;
        }
    }

    fn show_live_submit_previews(&self) -> AbortOnDrop {
        let mut states = self.handle.state();
        AbortOnDrop(tokio::task::spawn_local(async move {
            loop {
                if let EngineState::PendingEnding {
                    route: CaptureRoute::Submit,
                    preview: Some(preview),
                    ..
                } = states.borrow_and_update().clone()
                {
                    preview.shown();
                }
                if states.changed().await.is_err() {
                    break;
                }
            }
        }))
    }

    async fn live_submit_preview(
        &self,
        expected: &str,
        expected_waiting_for_quiet: bool,
    ) -> SubmitPreview {
        let mut states = self.handle.state();
        let observed = tokio::time::timeout(
            Duration::from_secs(5),
            states.wait_for(|state| {
                matches!(
                    state,
                    EngineState::PendingEnding {
                        route: CaptureRoute::Submit,
                        preview: Some(_),
                        ..
                    }
                )
            }),
        )
        .await
        .expect("the engine should ask for a live Submit preview")
        .unwrap();
        let EngineState::PendingEnding {
            route: CaptureRoute::Submit,
            text,
            waiting_for_quiet,
            preview: Some(preview),
        } = &*observed
        else {
            unreachable!()
        };
        assert_eq!(text, expected);
        assert_eq!(*waiting_for_quiet, expected_waiting_for_quiet);
        preview.clone()
    }
}

impl Ask {
    fn start(self, engine: TestEngine, frames: FakeFrames) -> JoinHandle<CaptureOutcome> {
        match self {
            Self::Ending => capture_task(engine, frames),
            Self::Key => hold_task(engine, frames),
        }
    }

    /// Says `words` and asks for them to be written out, up to the moment the engine hands them
    /// to the writing model.
    async fn say(self, rig: &mut Rig, words: &str) {
        match self {
            Self::Ending => {
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals(&format!("{words} write that out")).await;
                rig.feed_until(3499, Kind::Quiet).await;
                // The pause is over at 3500 and the accepted cue ends 100 ms later.
                rig.at(3600).await;
            }
            Self::Key => {
                rig.press_write().await;
                rig.server.ready();
                settle().await;
                rig.at(230).await;
                rig.feed_until(1000, Kind::Voice).await;
                rig.partial(words).await;
                rig.release_write(1200).await;
                rig.feed_until(1250, Kind::Voice).await;
                rig.finals(words).await;
                rig.at(1350).await;
            }
        }
    }
}

fn writing_failed(error: WritingError, words: &str) -> CaptureOutcome {
    CaptureOutcome::Failed {
        error: CaptureError::Writing(error),
        recoverable_text: Some(words.to_owned()),
    }
}

#[tokio::test(start_paused = true)]
async fn the_written_text_streams_into_the_state_after_any_thinking_and_only_it_is_pasted() {
    LocalSet::new()
        .run_until(async {
            for ask in [Ask::Ending, Ask::Key] {
                let (url, mut asked, feed) = chat(Chat::Live).await;
                let (mut rig, engine, frames) = Rig::new(&with_writing(&url));
                let task = ask.start(engine, frames);
                ask.say(&mut rig, WORDS).await;
                request(&mut asked).await;
                let writing = |text: &str| EngineState::Writing {
                    text: text.to_owned(),
                };
                rig.state_is(writing("")).await;
                feed.thinking("Anna is late, so");
                rig.state_is(EngineState::Thinking).await;
                feed.thinking(" an apology.");
                feed.text("Hi Anna,");
                rig.state_is(writing("Hi Anna,")).await;
                feed.text(" I'm late, sorry.\n");
                rig.state_is(writing("Hi Anna, I'm late, sorry.\n")).await;
                assert!(!task.is_finished(), "{ask:?}: nothing is pasted mid-stream");
                feed.finish();
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Paste, "Hi Anna, I'm late, sorry."),
                    "{ask:?}"
                );
                assert_eq!(rig.notices(), Vec::<String>::new(), "{ask:?}");
                assert_eq!(rig.state(), EngineState::Listening, "{ask:?}");
                assert!(rig.gate.is_enabled(), "{ask:?}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn only_the_spoken_words_are_sent_with_the_default_prompt() {
    LocalSet::new()
        .run_until(async {
            for (ask, words, key) in [
                (Ask::Ending, WORDS, "k"),
                // Every word of a hold is text, even the words of an ending.
                (Ask::Key, "ok never mind write that out", "k"),
                (Ask::Ending, WORDS, ""),
            ] {
                let (url, mut asked, _feed) = chat(Chat::Stream("data: [DONE]\n\n")).await;
                let config = with_writing(&url)
                    .replace(r#""api_key": "k""#, &format!(r#""api_key": "{key}""#));
                let (mut rig, engine, frames) = Rig::new(&config);
                let task = ask.start(engine, frames);
                ask.say(&mut rig, words).await;
                let asked = request(&mut asked).await;
                finished(task).await;
                let label = format!("{ask:?} {words:?}");
                assert_eq!(asked.path, "/v1/chat/completions", "{label}");
                assert_eq!(
                    asked.authorization,
                    (!key.is_empty()).then(|| format!("Bearer {key}")),
                    "{label}"
                );
                let body = asked.body.as_object().unwrap();
                let mut keys: Vec<&str> = body.keys().map(String::as_str).collect();
                keys.sort_unstable();
                assert_eq!(keys, ["messages", "model", "stream"], "{label}");
                assert_eq!(body["model"], "m", "{label}");
                assert_eq!(body["stream"], true, "{label}");
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 2, "{label}");
                assert_eq!(messages[0]["role"], "system", "{label}");
                assert_eq!(messages[0]["content"], DEFAULT_PROMPT, "{label}");
                assert_eq!(
                    messages[1],
                    json!({"role": "user", "content": words}),
                    "{label}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_opening_of_the_words_picks_the_system_message_and_the_words_stay_whole() {
    LocalSet::new()
        .run_until(async {
            let fallback = "Write short emails.\nSign off with “Best”.";
            let openings = json!([
                {"phrase": "write me an email", "prompt": "EMAIL"},
                {"phrase": "write me", "prompt": "ME"},
                {"phrase": "write a message", "prompt": "MESSAGE"},
            ]);
            // (how it is asked, the words as recognized, the system message expected)
            for (ask, words, prompt) in [
                (
                    Ask::Ending,
                    "Write me an email, tell Anna I'm late",
                    "EMAIL",
                ),
                (Ask::Key, "Write me an email, tell Anna I'm late", "EMAIL"),
                (Ask::Ending, "write a message that I am late", "MESSAGE"),
                (Ask::Key, "write me a poem", "ME"),
                (Ask::Ending, "please write me an email", fallback),
                (Ask::Key, WORDS, fallback),
            ] {
                let (url, mut asked, _feed) = chat(Chat::Stream("data: [DONE]\n\n")).await;
                let config = json!({
                    "ending_pause": {"paste": 2.5},
                    "writing": {"url": url, "model": "m", "prompt": fallback, "openings": openings},
                })
                .to_string();
                let (mut rig, engine, frames) = Rig::new(&config);
                let task = ask.start(engine, frames);
                ask.say(&mut rig, words).await;
                let asked = request(&mut asked).await;
                finished(task).await;
                assert_eq!(
                    asked.body["messages"],
                    json!([
                        {"role": "system", "content": prompt},
                        {"role": "user", "content": words},
                    ]),
                    "{ask:?} {words:?}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn writing_uses_the_bound_app_prompt_and_records_only_the_prompt_source() {
    LocalSet::new()
        .run_until(async {
            let kinds = RecordKinds {
                metrics: true,
                text: true,
                audio: false,
            };
            for ask in [Ask::Ending, Ask::Key] {
                for (words, window, prompt, source) in [
                    (
                        WORDS,
                        Some(WindowApp::new("kitty", "secret-title")),
                        "APP",
                        "app",
                    ),
                    (
                        "write me an email to Anna",
                        Some(WindowApp::new("kitty", "secret-title")),
                        "OPENING",
                        "opening",
                    ),
                    (
                        WORDS,
                        Some(WindowApp::new("kitty", "different-title")),
                        "FALLBACK",
                        "default",
                    ),
                    (WORDS, None, "FALLBACK", "default"),
                ] {
                    let (url, mut asked, feed) = chat(Chat::Live).await;
                    let config = json!({
                        "ending_pause": {"paste": 2.5},
                        "writing": {
                            "url": url, "model": "m", "prompt": "FALLBACK",
                            "openings": [{"phrase": "write me an email", "prompt": "OPENING"}],
                            "apps": [{"app": "kitty", "title": "secret-title", "prompt": "APP"}],
                        },
                    })
                    .to_string();
                    let dir =
                        record_dir(&format!("app-style-{ask:?}-{source}-{}", window.is_some()));
                    let (mut rig, engine, frames) =
                        Rig::recording(&config, Recorder::in_dir(kinds, &dir).unwrap());
                    let trigger = match ask {
                        Ask::Ending => WakeTrigger::Capture,
                        Ask::Key => WakeTrigger::Hold,
                    };
                    let task = capture_in(engine, frames, trigger, window);
                    ask.say(&mut rig, words).await;
                    let body = request(&mut asked).await.body;
                    for private in ["kitty", "secret-title"] {
                        assert!(
                            !body.to_string().contains(private),
                            "window metadata must stay out of the writing request: {body}"
                        );
                    }
                    assert_eq!(
                        body,
                        json!({
                            "model": "m", "stream": true,
                            "messages": [
                                {"role": "system", "content": prompt},
                                {"role": "user", "content": words},
                            ],
                        }),
                        "{ask:?} {source}"
                    );
                    feed.text("Written.");
                    feed.finish();
                    assert_eq!(
                        finished(task).await,
                        accepted(CaptureRoute::Paste, "Written.")
                    );
                    let metrics = read_lines(&dir.join("metrics.jsonl"));
                    let calls: Vec<_> = metrics
                        .iter()
                        .filter(|line| line["event"] == "desktop_writing")
                        .collect();
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0]["prompt_source"], source, "{ask:?}");
                    let saved = std::fs::read_to_string(dir.join("metrics.jsonl")).unwrap();
                    assert_eq!(dictation_dirs(&dir).len(), 1);
                    for private in ["kitty", "secret-title"] {
                        assert!(
                            !saved.contains(private),
                            "window metadata must stay out of metrics"
                        );
                        for path in dictation_dirs(&dir) {
                            let text = std::fs::read_to_string(path.join("text.txt")).unwrap();
                            assert!(
                                !text.contains(private),
                                "window metadata must stay out of recordings"
                            );
                        }
                    }
                    std::fs::remove_dir_all(dir).unwrap();
                }
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn nothing_is_written_unless_it_is_asked_for() {
    LocalSet::new()
        .run_until(async {
            // (how it is dictated, the words as recognized, what is pasted)
            for (name, expected) in [
                ("paste ending", accepted(CaptureRoute::Paste, WORDS)),
                (
                    "hold key",
                    accepted(CaptureRoute::Paste, "tell Anna I'm late write that out"),
                ),
                (
                    "write ending without words",
                    accepted(CaptureRoute::Paste, ""),
                ),
            ] {
                let (url, mut asked, _feed) = chat(Chat::Live).await;
                // The dictated words begin with an opening phrase, which alone starts nothing.
                let config = with_writing(&url).replace(
                    r#""model": "m""#,
                    r#""model": "m", "openings": [{"phrase": "tell anna", "prompt": "P"}], "apps": [{"app": "kitty", "prompt": "APP"}]"#,
                );
                let (mut rig, engine, frames) = Rig::new(&config);
                let task = if name == "hold key" {
                    let task = capture_in(engine, frames, WakeTrigger::Hold, Some(WindowApp::new("kitty", "secret-title")));
                    rig.hold().await;
                    rig.say(1000, Say::Partial("tell Anna I'm late write that out"))
                        .await;
                    rig.release(1200).await;
                    rig.say(1250, Say::Final("tell Anna I'm late write that out"))
                        .await;
                    task
                } else {
                    let task = capture_in(engine, frames, WakeTrigger::Capture, Some(WindowApp::new("kitty", "secret-title")));
                    rig.open().await;
                    rig.feed_until(1000, Kind::Quiet).await;
                    let said = if name == "paste ending" {
                        format!("{WORDS} paste that")
                    } else {
                        "write that out".to_owned()
                    };
                    rig.finals(&said).await;
                    rig.feed_until(3500, Kind::Quiet).await;
                    task
                };
                rig.at(4000).await;
                assert_eq!(finished(task).await, expected, "{name}");
                assert!(asked.try_recv().is_err(), "{name}: no request was made");
                assert!(
                    !matches!(rig.state(), EngineState::Writing { .. }),
                    "{name}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_writing_call_that_gives_no_text_pastes_nothing_keeps_the_words_and_says_where_to_set_it_up()
 {
    use WritingError::{BadResponse, Empty, NotSetUp, Status, Unreachable};

    LocalSet::new()
        .run_until(async {
            // Port 0 fails at once everywhere; Windows retries a refused port for seconds, past the paused clock's deadline.
            let refused = "http://127.0.0.1:0/v1".to_owned();
            // (name, endpoint, the reason, whether a request reached the endpoint)
            let cases = [
                ("no endpoint is set up", None, NotSetUp, false),
                ("the endpoint is down", Some((refused, Chat::Live)), Unreachable, false),
                ("it answers with an error status", Some((String::new(), Chat::Status(500))), Status(500), true),
                (
                    "it returns nothing",
                    Some((String::new(), Chat::Stream("data: [DONE]\n\n"))),
                    Empty,
                    true,
                ),
                (
                    "it returns only blanks",
                    Some((
                        String::new(),
                        Chat::Stream("data: {\"choices\":[{\"delta\":{\"content\":\" \\n\"}}]}\n\ndata: [DONE]\n\n"),
                    )),
                    Empty,
                    true,
                ),
                (
                    "it sends what is not a chat reply",
                    Some((String::new(), Chat::Stream("data: nonsense\n\n"))),
                    BadResponse,
                    true,
                ),
            ];
            for (name, endpoint, reason, reached) in cases {
                let (config, mut asked) = match endpoint {
                    None => (r#"{"ending_pause": {"paste": 2.5}}"#.to_owned(), None),
                    Some((url, reply)) if url.is_empty() => {
                        let (url, asked, _feed) = chat(reply).await;
                        (with_writing(&url), Some(asked))
                    }
                    Some((url, _)) => (with_writing(&url), None),
                };
                let (mut rig, engine, frames) = Rig::new(&config);
                let task = capture_task(engine, frames);
                Ask::Ending.say(&mut rig, WORDS).await;
                assert_eq!(
                    finished(task).await,
                    writing_failed(reason, WORDS),
                    "{name}"
                );
                if let Some(asked) = asked.as_mut() {
                    assert_eq!(asked.try_recv().is_ok(), reached, "{name}");
                }
                assert_eq!(rig.state(), EngineState::Listening, "{name}");
                assert!(rig.gate.is_enabled(), "{name}");
            }
            assert_eq!(
                CaptureError::Writing(NotSetUp).to_string(),
                "No writing model is set up. Add one under Settings → Chat model → Writing. Nothing was pasted.",
                "the user is told where to set up writing"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn writing_that_takes_longer_than_fifteen_seconds_pastes_none_of_what_arrived() {
    LocalSet::new()
        .run_until(async {
            // (name, the reply, what it streams first, the reason given)
            let cases = [
                (
                    "no reply at all",
                    Chat::Stall,
                    Streamed::Nothing,
                    WritingError::Timeout,
                ),
                (
                    "a reply that stops",
                    Chat::Live,
                    Streamed::Text,
                    WritingError::Timeout,
                ),
                (
                    "a model still thinking",
                    Chat::Live,
                    Streamed::Thinking,
                    WritingError::StillThinking,
                ),
            ];
            for (name, reply, streamed, reason) in cases {
                let (url, mut asked, feed) = chat(reply).await;
                let (mut rig, engine, frames) = Rig::new(&with_writing(&url));
                let task = capture_task(engine, frames);
                Ask::Ending.say(&mut rig, WORDS).await;
                request(&mut asked).await;
                let began = rig.now();
                streamed.feed(&feed, &rig).await;
                rig.at(began + 14_999).await;
                assert!(!task.is_finished(), "{name}: still within the 15 s");
                rig.at(began + 15_000).await;
                assert_eq!(
                    finished(task).await,
                    writing_failed(reason, WORDS),
                    "{name}"
                );
                assert_eq!(rig.state(), EngineState::Listening, "{name}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_before_the_paste_drops_the_written_text() {
    LocalSet::new()
        .run_until(async {
            for ask in [Ask::Ending, Ask::Key] {
                for streamed in [Streamed::Nothing, Streamed::Thinking, Streamed::Text] {
                    let label = format!("{ask:?}, {streamed:?}");
                    let reply = match streamed {
                        Streamed::Nothing => Chat::Stall,
                        Streamed::Thinking | Streamed::Text => Chat::Live,
                    };
                    let (url, mut asked, feed) = chat(reply).await;
                    let (mut rig, engine, frames) = Rig::new(&with_writing(&url));
                    let task = ask.start(engine, frames);
                    ask.say(&mut rig, WORDS).await;
                    request(&mut asked).await;
                    streamed.feed(&feed, &rig).await;
                    rig.handle.cancel();
                    assert_eq!(
                        finished(task).await,
                        CaptureOutcome::Cancelled {
                            dictated: Some(WORDS.to_owned())
                        },
                        "{label}"
                    );
                    assert_eq!(rig.state(), EngineState::Listening, "{label}");
                    assert_eq!(rig.notices(), Vec::<String>::new(), "{label}");
                }
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn each_writing_call_records_its_times_to_first_reasoning_and_first_text_total_time_and_outcome()
 {
    LocalSet::new()
        .run_until(async {
            let all = RecordKinds {
                metrics: true,
                text: false,
                audio: false,
            };
            // (the reply, the outcome recorded, whether reasoning and then text arrived)
            let cases = [
                (Some(Chat::Live), "ok", true),
                (Some(Chat::Status(503)), "status", false),
                (None, "not_set_up", false),
            ];
            for (index, (reply, outcome, first)) in cases.into_iter().enumerate() {
                let dir = record_dir(&format!("writing-{index}"));
                let (config, feed) = match reply {
                    Some(reply) => {
                        let (url, mut asked, feed) = chat(reply).await;
                        tokio::task::spawn_local(
                            async move { while asked.recv().await.is_some() {} },
                        );
                        (with_writing(&url), Some(feed))
                    }
                    None => (r#"{"ending_pause": {"paste": 2.5}}"#.to_owned(), None),
                };
                let recorder = Recorder::in_dir(all, &dir).unwrap();
                let (mut rig, engine, frames) = Rig::recording(&config, recorder);
                let task = capture_task(engine, frames);
                Ask::Ending.say(&mut rig, WORDS).await;
                if let Some(feed) = feed.filter(|_| first) {
                    feed.thinking("Anna is late");
                    rig.state_is(EngineState::Thinking).await;
                    rig.at(rig.now() + 200).await;
                    feed.text("Hi Anna.");
                    rig.state_is(EngineState::Writing {
                        text: "Hi Anna.".to_owned(),
                    })
                    .await;
                    rig.at(rig.now() + 400).await;
                    feed.finish();
                }
                finished(task).await;
                let lines = read_lines(&dir.join("metrics.jsonl"));
                let calls: Vec<_> = lines
                    .iter()
                    .filter(|line| line["event"] == "desktop_writing")
                    .collect();
                assert_eq!(calls.len(), 1, "{outcome}: {lines:?}");
                let call = calls[0];
                assert_eq!(call["outcome"], outcome);
                if outcome == "not_set_up" {
                    assert!(call.get("model").is_none(), "{call}");
                    continue;
                }
                assert_eq!(call["model"], "m", "{call}");
                for field in ["first_reasoning_ms", "first_token_ms"] {
                    assert_eq!(call[field].is_f64(), first, "{field}: {call}");
                }
                if first {
                    let near = |field: &str, ms: f64| {
                        assert!(
                            (call[field].as_f64().unwrap() - ms).abs() < 50.0,
                            "{field}: {call}"
                        );
                    };
                    near("first_reasoning_ms", 0.0);
                    near("first_token_ms", 200.0);
                    near("duration_ms", 600.0);
                }
                for words in [WORDS, "Hi Anna", "Anna is late"] {
                    assert!(
                        !call.to_string().contains(words),
                        "{call}: no text in metrics"
                    );
                }
            }
        })
        .await;
}

fn cleaning(url: &str) -> String {
    with_writing(url).replace(
        r#""model": "m""#,
        r#""model": "m", "clean_dictation": true, "prompt": "REWRITE", "openings": [{"phrase": "tell anna", "prompt": "OPENING"}], "apps": [{"app": "kitty", "prompt": "APP"}]"#,
    )
}

/// A dictation that ends as a paste or a submit. Returns when the ending final arrived: the
/// dictation final, where a cleanup's 3 s limit starts.
async fn dictate(rig: &mut Rig, route: CaptureRoute) -> u64 {
    let said = match route {
        CaptureRoute::Submit => format!("{WORDS} submit"),
        _ => format!("{WORDS} paste that"),
    };
    rig.open().await;
    rig.feed_until(1000, Kind::Quiet).await;
    let decided = rig.now();
    rig.finals(&said).await;
    rig.feed_until(3499, Kind::Quiet).await;
    rig.at(3600).await;
    decided
}

#[tokio::test(start_paused = true)]
async fn a_paste_gets_number_words_as_digits_by_default_and_off_delivers_the_same_bytes_as_before()
{
    LocalSet::new()
        .run_until(async {
            // (name, config, route, the ending, the words, what is delivered, rewrites)
            let cases = [
                (
                    "on by default",
                    "{}".to_owned(),
                    CaptureRoute::Paste,
                    "paste that",
                    "I have fifteen open issues",
                    "I have 15 open issues",
                    Some(1),
                ),
                (
                    "off",
                    r#"{"writing": {"numbers_as_digits": false}}"#.to_owned(),
                    CaptureRoute::Paste,
                    "paste that",
                    "I have fifteen open issues",
                    "I have fifteen open issues",
                    None,
                ),
                (
                    "nothing to rewrite",
                    "{}".to_owned(),
                    CaptureRoute::Paste,
                    "paste that",
                    "I have 15 open issues",
                    "I have 15 open issues",
                    None,
                ),
                (
                    "a task is not formatted",
                    "{}".to_owned(),
                    CaptureRoute::Task,
                    "do that",
                    "open tab three",
                    "open tab three",
                    None,
                ),
            ];
            for (name, config, route, ending, said, delivered, rewrites) in cases {
                let dir = record_dir("number-formatting");
                let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
                let (mut rig, engine, frames) = Rig::recording(&config, recorder);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals(&format!("{said} {ending}")).await;
                rig.feed_until(3499, Kind::Quiet).await;
                rig.at(3600).await;
                assert_eq!(finished(task).await, accepted(route, delivered), "{name}");
                let counts: Vec<_> = read_lines(&dir.join("metrics.jsonl"))
                    .into_iter()
                    .filter(|line| line["event"] == "desktop_numbers")
                    .map(|line| line["count"].as_u64().unwrap())
                    .collect();
                assert_eq!(counts, rewrites.as_slice(), "{name}: count only rewrites");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_write_sends_the_formatted_text_to_the_writing_model() {
    LocalSet::new()
        .run_until(async {
            let (url, mut asked, _feed) = chat(Chat::Stream(says!("I have 15 open issues."))).await;
            let (mut rig, engine, frames) = Rig::new(&with_writing(&url));
            let task = Ask::Ending.start(engine, frames);
            Ask::Ending
                .say(&mut rig, "I have fifteen open issues")
                .await;
            let asked = request(&mut asked).await;
            assert_eq!(
                asked.body["messages"][1],
                json!({"role": "user", "content": "I have 15 open issues"})
            );
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "I have 15 open issues.")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_submit_confirms_the_formatted_text_that_is_then_accepted() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("I have fifteen open issues submit").await;
            rig.live_submit_preview("I have 15 open issues", false)
                .await
                .shown();
            settle().await;
            rig.feed_until(3499, Kind::Quiet).await;
            rig.at(3600).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Submit, "I have 15 open issues")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cleanup_gets_the_formatted_text_and_the_paste_counts_the_rewrites_without_recording_them()
{
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("numbers-cleanup");
            let (url, mut asked, _feed) =
                chat(Chat::After(500, says!("I have 15 open issues."))).await;
            let (mut rig, engine, frames) =
                Rig::recording(&cleaning(&url), Recorder::in_dir(ALL, &dir).unwrap());
            let task = capture_in(engine, frames, WakeTrigger::Capture, None);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("I have fifteen open issues paste that").await;
            rig.feed_until(3499, Kind::Quiet).await;
            rig.at(3600).await;
            let body = request(&mut asked).await.body;
            assert_eq!(body["messages"][0]["content"], CLEANUP_PROMPT);
            assert_eq!(body["messages"][1]["content"], "I have 15 open issues");
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "I have 15 open issues.")
            );
            let metrics = read_lines(&dir.join("metrics.jsonl"));
            let numbers = metrics
                .iter()
                .find(|line| line["event"] == "desktop_numbers")
                .expect("a numbers metric");
            assert_eq!(numbers["outcome"], "ok");
            assert_eq!(numbers["count"], 1);
            assert!(
                !numbers.to_string().contains("I have"),
                "no dictated text in the metrics: {numbers}"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn an_ordinary_submit_never_reformats_its_cleanup_reply_after_the_confirmation() {
    LocalSet::new()
        .run_until(async {
            let (url, mut asked, _feed) =
                chat(Chat::Stream(says!("I have fifteen open issues."))).await;
            let (mut rig, engine, frames) = Rig::new(&cleaning(&url));
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("I have three open issues submit").await;
            let cleanup = request(&mut asked).await;
            assert_eq!(
                cleanup.body["messages"][1]["content"],
                "I have 3 open issues"
            );
            rig.live_submit_preview("I have fifteen open issues.", false)
                .await
                .shown();
            settle().await;
            assert_eq!(rig.started(Cue::Provisional).len(), 1);
            rig.feed_until(3499, Kind::Quiet).await;
            rig.at(3600).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Submit, "I have fifteen open issues."),
                "the reply shown before the pause is the text that is sent"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_entries_hint_or_the_detected_language_formats_the_dictation() {
    LocalSet::new()
        .run_until(async {
            const HINTED: &str = r#"{
                "stt": {"entries": [{"kind": "open_ai", "id": "de", "name": "German",
                    "active": true, "address": "http://127.0.0.1:9/v1", "model": "m",
                    "language": "de"}]}
            }"#;
            const GERMAN: &str = "Ich habe fünfzehn offene Tickets paste that";
            // (config, the language the recognizer detected, the words, what is pasted)
            // Plain ASCII is formatted as English only when no language was detected.
            let cases = [
                (HINTED, None, GERMAN, "Ich habe 15 offene Tickets"),
                ("{}", Some("German"), GERMAN, "Ich habe 15 offene Tickets"),
                (
                    "{}",
                    Some("french"),
                    "I have two cats paste that",
                    "I have two cats",
                ),
            ];
            for (config, detected, said, pasted) in cases {
                let (mut rig, engine, frames) = Rig::new(config);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                if let Some(language) = detected {
                    rig.server.heard_language(Language::detected(language));
                }
                rig.finals(said).await;
                rig.feed_until(3499, Kind::Quiet).await;
                rig.at(3600).await;
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Paste, pasted),
                    "{config} {detected:?}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
#[expect(
    clippy::too_many_lines,
    reason = "one table of the cleanup's replies, fallbacks and a cancel"
)]
async fn dictation_cleanup_pastes_the_reply_the_raw_words_or_nothing() {
    LocalSet::new()
        .run_until(async {
            // (name, route, the reply, what is delivered, the notice, how the cleanup went)
            let cases = [
                (
                    "a reply in time",
                    CaptureRoute::Paste,
                    Chat::After(500, says!("I think we should ship it on Friday.")),
                    Some("I think we should ship it on Friday."),
                    None,
                    "cleaned",
                ),
                (
                    "a submit shows the cleaned text",
                    CaptureRoute::Submit,
                    Chat::After(500, says!("Tell Anna I am late.")),
                    Some("Tell Anna I am late."),
                    None,
                    "cleaned",
                ),
                (
                    "an error pastes the raw words",
                    CaptureRoute::Paste,
                    Chat::Status(500),
                    Some(WORDS),
                    Some("Cleanup failed: The writing model failed with status 500. — pasted as said"),
                    "raw: The writing model failed with status 500.",
                ),
                (
                    "no reply by the limit pastes the raw words",
                    CaptureRoute::Submit,
                    Chat::Stall,
                    Some(WORDS),
                    Some("Cleanup failed: The cleanup took longer than 3 seconds. — pasted as said"),
                    "raw: The cleanup took longer than 3 seconds.",
                ),
            ];
            for (name, route, reply, delivered, notice, cleanup) in cases {
                let (url, mut asked, _feed) = chat(reply).await;
                let dir = record_dir(&format!("cleanup-{name}"));
                let (mut rig, mut engine, mut frames) = Rig::recording(
                    &cleaning(&url),
                    Recorder::in_dir(ALL, &dir).unwrap(),
                );
                let _previews = rig.show_live_submit_previews();
                let task = tokio::task::spawn_local(async move {
                    assert_eq!(
                        engine.listen(&mut frames).await.unwrap(),
                        WakeTrigger::Capture
                    );
                    let outcome = engine
                        .capture(&mut frames, Some(WindowApp::new("kitty", "secret")))
                        .await;
                    let cleanup_note = engine.take_cleanup_note();
                    let record = engine.take_cleanup();
                    let next_record = engine.take_cleanup();
                    engine.finish().await;
                    (outcome, cleanup_note, record, next_record)
                });
                let decided = dictate(&mut rig, route).await;
                let body = request(&mut asked).await.body;
                assert_eq!(
                    body["messages"][0]["content"], CLEANUP_PROMPT,
                    "{name}: openings, the prompt and the app style never choose it"
                );
                assert_eq!(body["messages"][1]["content"], WORDS, "{name}");
                assert!(
                    !body.to_string().contains("kitty"),
                    "{name}: the app stays out of the request"
                );
                if matches!(reply, Chat::Stall) {
                    // A submit is cleaned before its confirmation, from the dictation final.
                    rig.state_is(EngineState::CleaningUp).await;
                    rig.at(decided + 2_999).await;
                    assert!(
                        !matches!(rig.state(), EngineState::PendingEnding { .. }),
                        "{name}: the confirmation waits for the cleanup"
                    );
                    assert!(!task.is_finished(), "{name}: still within the 3 s");
                    rig.at(decided + 3_000).await;
                    rig.live_submit_preview(WORDS, false).await;
                    rig.at(rig.now() + 2_500).await;
                } else if route == CaptureRoute::Submit {
                    rig.live_submit_preview(delivered.unwrap(), false).await;
                    rig.at(rig.now() + 2_500).await;
                }
                let text = delivered.unwrap();
                let (outcome, cleanup_note, record, next_record) = finished(task).await;
                assert_eq!(outcome, accepted(route, text), "{name}");
                assert_eq!(
                    cleanup_note.as_deref(),
                    notice,
                    "{name}: delivery can keep showing the cleanup fallback"
                );
                assert_eq!(
                    record,
                    Some((cleanup.to_owned(), WORDS.to_owned())),
                    "{name}: the caller learns how the cleanup went and the words it rewrote"
                );
                assert_eq!(next_record, None, "{name}: the cleanup is taken once");
                assert_eq!(rig.notices(), notice.into_iter().collect::<Vec<_>>(), "{name}");
                let row = read_lines(&dir.join("metrics.jsonl"))
                    .into_iter()
                    .find(|line| line["event"] == "desktop_cleanup")
                    .expect("a cleanup metric");
                assert!(row["model_ms"].is_number(), "{name}: {row}");
                assert!(row["perceived_ms"].is_number(), "{name}: {row}");
                assert!(
                    row["perceived_ms"].as_f64().unwrap() >= row["model_ms"].as_f64().unwrap(),
                    "{name}: the wait from the final is its own field, {row}"
                );
                std::fs::remove_dir_all(&dir).unwrap();
            }

            // A cancel during cleanup pastes nothing and plays the cancelled cue.
            let (url, mut asked, _feed) = chat(Chat::Stall).await;
            let (mut rig, engine, frames) = Rig::new(&cleaning(&url));
            let task = capture_task(engine, frames);
            dictate(&mut rig, CaptureRoute::Paste).await;
            request(&mut asked).await;
            rig.state_is(EngineState::CleaningUp).await;
            rig.handle.cancel();
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some(WORDS.to_owned())
                }
            );
            assert_eq!(rig.started(Cue::Cancelled).len(), 1);
            assert_eq!(rig.notices(), Vec::<String>::new());
        })
        .await;
}

/// An endpoint that is not ready within 3 s of the dictation final falls back to the raw words
/// then, before any chat call. The confirmation of a submit shows those words.
#[tokio::test(start_paused = true)]
async fn cleanup_falls_back_when_the_endpoint_is_not_ready_within_3_s_of_the_final() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("cleanup-slow-endpoint");
            let (chat, child) = fake_local_chat(&dir);
            child.loading_from_now();
            child.delay_health(ms(10_000));
            let config = DesktopConfig::from_json_str(
                r#"{"writing": {"local": true, "clean_dictation": true}}"#,
            )
            .unwrap();
            let (mut rig, engine, frames) =
                Rig::with_chat(&config, Recorder::in_dir(ALL, &dir).unwrap(), chat);
            let task = capture_task(engine, frames);
            let decided = dictate(&mut rig, CaptureRoute::Submit).await;
            // The child has been asked to start and stays unhealthy past the limit.
            settle().await;
            assert_eq!(child.spawns(), 1, "the endpoint was asked to start");
            rig.at(decided + 2_999).await;
            assert!(
                !matches!(rig.state(), EngineState::PendingEnding { .. }),
                "still waiting on the endpoint at 2999 ms: {:?}",
                rig.state()
            );
            assert!(!task.is_finished(), "the capture has not confirmed yet");
            rig.at(decided + 3_000).await;
            rig.live_submit_preview(WORDS, false).await.shown();
            settle().await;
            assert_eq!(
                rig.notices(),
                vec![
                    "Cleanup failed: The cleanup took longer than 3 seconds. — pasted as said"
                        .to_owned()
                ]
            );
            // No chat call was made: the endpoint never became ready.
            assert_eq!(child.spawns(), 1);
            rig.at(rig.now() + 2_500).await;
            assert_eq!(finished(task).await, accepted(CaptureRoute::Submit, WORDS));
            let row = read_lines(&dir.join("metrics.jsonl"))
                .into_iter()
                .find(|line| line["event"] == "desktop_cleanup")
                .expect("a cleanup metric");
            assert!(
                row.get("model_ms").is_none_or(serde_json::Value::is_null),
                "{row}"
            );
            assert_eq!(row["perceived_ms"], 3_000.0, "{row}");
            assert_eq!(row["outcome"], "timeout", "{row}");
            std::fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

fn fake_local_chat(
    dir: &std::path::Path,
) -> (
    thalvi_desktop::chat::Chat,
    thalvi_desktop::chat::fakes::LaunchControl,
) {
    use thalvi_desktop::chat::fakes::FakeLaunch;
    use thalvi_desktop::chat::{Build, Chat, Files};
    use thalvi_desktop::models::{Asset, ModelKind, ModelStore};

    std::fs::create_dir_all(dir.join("runtime")).unwrap();
    std::fs::write(dir.join("runtime/llama-server"), "fake").unwrap();
    std::fs::write(dir.join("model.gguf"), "gguf").unwrap();
    let files = Files {
        runtime: Build::Cpu.files().runtime,
        folder: "runtime",
        model: Asset {
            kind: ModelKind::ChatModel,
            file_name: "model.gguf",
            url: "http://127.0.0.1:9/unused",
            size: 4,
            sha256: "unused",
        },
    };
    let (launch, child) = FakeLaunch::new();
    let chat = Chat::new(
        ModelStore::in_dir(dir.to_owned()),
        Build::Cpu,
        files,
        Box::new(launch),
    );
    (chat, child)
}

/// The ending phrase the fake spotter hears, from the frame numbered 9, which the engine sends
/// 880 ms after the wake word; the phrase ends two frames earlier, at 7 frames of audio.
const HEARD_AT_FRAME: u64 = 9;

fn hearing(config: &str) -> (Rig, SpotterControl, TestEngine, FakeFrames) {
    let (rig, spot, engine, frames) = Rig::spotting(config, Recorder::disabled());
    spot.hear(HEARD_AT_FRAME, "paste that", Ending::Paste);
    (rig, spot, engine, frames)
}

/// The words the overlay holds for the open capture.
fn drafted(rig: &Rig) -> String {
    match rig.state() {
        EngineState::Capturing { committed, .. } => committed,
        other => panic!("not capturing: {other:?}"),
    }
}

/// The notice the overlay shows with the open capture, if any.
fn shown(rig: &Rig) -> Option<String> {
    match rig.state() {
        EngineState::Capturing { notice, .. } => notice,
        _ => None,
    }
}

/// The metrics of a capture whose heard ending was told: the hit and the capture's totals, no words.
fn assert_recorded_as_notified(dir: &std::path::Path, name: &str) {
    let metrics = read_lines(&dir.join("metrics.jsonl"));
    let hit = metrics
        .iter()
        .find(|line| line["event"] == "desktop_ending_spotter" && line["call"] == "hit")
        .unwrap_or_else(|| panic!("{name}: no hit line in {metrics:?}"));
    assert_eq!(
        (hit["outcome"].as_str(), hit["ending"].as_str()),
        (Some("notified"), Some("paste")),
        "{name}"
    );
    assert!(
        metrics
            .iter()
            .any(|line| line["event"] == "desktop_ending_spotter"
                && line["call"] == "capture"
                && line["frames"].as_u64().is_some_and(|frames| frames > 9)
                && line["hits"] == 1),
        "{name}: capture summary"
    );
    let written = std::fs::read_to_string(dir.join("metrics.jsonl")).unwrap();
    assert!(
        !written.contains("haste") && !written.contains("report"),
        "{name}: no words in metrics"
    );
}

#[tokio::test(start_paused = true)]
async fn a_heard_ending_the_transcript_lacks_is_told_once_after_the_quiet_interval_and_changes_nothing()
 {
    use Act::{Audio, Final};
    use Kind::{Quiet, Voice};
    // (what happens after the spotter hears the phrase, when the notice comes)
    let cases: [(&str, Vec<Act>, u64); 2] = [
        (
            "quiet after the final",
            vec![
                Audio(1000, Quiet),
                Final(1000, "Write the report tomorrow haste that"),
            ],
            1000 + 700,
        ),
        (
            // The detector keeps judging speech for two frames after the voice stops.
            "speech after the final restarts the interval at the first quiet frame",
            vec![
                Audio(1000, Voice),
                Final(1000, "Write the report tomorrow haste that"),
                Audio(1600, Voice),
            ],
            1840 + 700,
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (name, script, notice_at) in cases {
                let dir = record_dir("disputed");
                let recorder = Recorder::in_dir(
                    RecordKinds {
                        metrics: true,
                        text: false,
                        audio: false,
                    },
                    &dir,
                )
                .unwrap();
                let (mut rig, spot, engine, frames) = Rig::spotting("{}", recorder);
                spot.hear(HEARD_AT_FRAME, "paste that", Ending::Paste);
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(920, Quiet).await;
                assert_eq!(spot.received(), HEARD_AT_FRAME, "{name}");
                assert_eq!(spot.resets(), 1, "{name}: each wake starts a fresh search");
                rig.play(&script).await;
                rig.feed_until(notice_at - 1, Quiet).await;
                assert_eq!(
                    (shown(&rig), rig.started(Cue::Cancelled).len()),
                    (None, 0),
                    "{name}: early"
                );
                rig.at(notice_at).await;
                let notice = shown(&rig).unwrap_or_else(|| panic!("{name}: nothing shown"));
                assert!(
                    notice.contains("\u{201c}paste that\u{201d}")
                        && !notice.contains("haste")
                        && !notice.contains("report"),
                    "{name}: names the ending, quotes no dictated words: {notice}"
                );
                assert_eq!(rig.started(Cue::Cancelled), [notice_at], "{name}");
                assert_eq!(rig.started(Cue::Provisional).len(), 0, "{name}");
                // It stays while the capture goes on, until words are heard.
                rig.feed_until(notice_at + 800, Quiet).await;
                assert!(!task.is_finished(), "{name}: the capture goes on");
                assert_eq!(shown(&rig), Some(notice), "{name}: still shown");
                assert_eq!(rig.started(Cue::Cancelled).len(), 1, "{name}: told once");
                assert!(rig.notices().is_empty(), "{name}: {:?}", rig.notices());
                assert_eq!(
                    drafted(&rig),
                    "Write the report tomorrow haste that",
                    "{name}"
                );
                // Saying it again delivers everything: nothing was cut at the heard phrase.
                let again = notice_at + 800;
                rig.finals("Paste that.").await;
                rig.feed_until(again + 700, Quiet).await;
                assert_eq!(
                    finished(task).await,
                    accepted(CaptureRoute::Paste, "Write the report tomorrow haste that"),
                    "{name}"
                );
                assert_recorded_as_notified(&dir, name);
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_heard_ending_is_never_told_when_the_transcript_carries_it_or_the_user_speaks_on() {
    use Act::{Audio, Final, Partial};
    use Kind::{Quiet, Voice};
    /// How the capture ends once the script has run.
    enum End {
        Delivers(CaptureRoute, &'static str, u64),
        Cancelled(&'static str),
    }
    let cases: [(&str, Vec<Act>, End, &[u64]); 6] = [
        (
            // 2.2 s after the phrase ended, long after a 0.8 s timer would have called it missed.
            "a late final carries the ending: late agreement",
            vec![
                Audio(1000, Quiet),
                Audio(3000, Quiet),
                Final(3000, "Write the report paste that"),
            ],
            End::Delivers(CaptureRoute::Paste, "Write the report", 3000 + 700),
            &[3000],
        ),
        (
            "the final came before the spotter: both signals deliver once",
            vec![
                Audio(800, Quiet),
                Final(800, "Write the report paste that"),
                Audio(1000, Quiet),
            ],
            End::Delivers(CaptureRoute::Paste, "Write the report", 800 + 700),
            &[800],
        ),
        (
            "the transcript says another ending: it wins",
            vec![Audio(1000, Quiet), Final(1000, "Write the report submit")],
            End::Delivers(CaptureRoute::Submit, "Write the report", 1000 + 2500),
            &[1000],
        ),
        (
            "words follow the disputed final",
            vec![
                Audio(1000, Quiet),
                Final(1000, "Write the report tomorrow haste that"),
                Audio(1200, Voice),
                Partial(1200, "and then"),
                Audio(3000, Quiet),
            ],
            End::Cancelled("Write the report tomorrow haste that"),
            &[],
        ),
        (
            "a final with words follows the disputed one",
            vec![
                Audio(1000, Quiet),
                Final(1000, "Write the report tomorrow haste that"),
                Audio(1300, Quiet),
                Final(1300, "and one more thing"),
                Audio(3000, Quiet),
            ],
            End::Cancelled("Write the report tomorrow haste that and one more thing"),
            &[],
        ),
        (
            "nothing but artifacts follows the heard phrase",
            vec![
                Audio(1000, Quiet),
                Final(1000, "[noise]"),
                Audio(1500, Quiet),
            ],
            End::Cancelled(""),
            &[],
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (name, script, end, provisional) in cases {
                let (mut rig, _spot, engine, frames) = hearing("{}");
                let _previews = rig.show_live_submit_previews();
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.play(&script).await;
                assert_eq!(shown(&rig), None, "{name}");
                let outcome = match end {
                    End::Delivers(route, text, at) => {
                        rig.feed_until(at, Quiet).await;
                        accepted(route, text)
                    }
                    End::Cancelled(text) => {
                        rig.handle.cancel();
                        settle().await;
                        CaptureOutcome::Cancelled {
                            dictated: (!text.is_empty()).then(|| text.to_owned()),
                        }
                    }
                };
                assert_eq!(finished(task).await, outcome, "{name}");
                assert_eq!(rig.started(Cue::Cancelled).len(), 0, "{name}");
                assert_eq!(rig.started(Cue::Provisional), provisional, "{name}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_hold_gives_the_spotter_no_frames_and_the_capture_runs_as_before() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, spot, engine, frames) = Rig::spotting("{}", Recorder::disabled());
            spot.hear(HEARD_AT_FRAME, "paste that", Ending::Paste);
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.say(1000, Say::Final("write the report tomorrow haste that"))
                .await;
            rig.release(1200).await;
            rig.at(1450).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Paste, "write the report tomorrow haste that")
            );
            assert_eq!((spot.received(), spot.resets()), (0, 0));
            assert_eq!(rig.notices(), Vec::<String>::new());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_while_a_dispute_waits_for_quiet_cancels_and_tells_nothing() {
    use Act::{Audio, Final};
    use Kind::Quiet;
    LocalSet::new()
        .run_until(async {
            let (mut rig, _spot, engine, frames) = hearing("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.play(&[
                Audio(1000, Quiet),
                Final(1000, "Write the report tomorrow haste that"),
                Audio(1300, Quiet),
            ])
            .await;
            rig.handle.cancel();
            settle().await;
            assert_eq!(
                finished(task).await,
                CaptureOutcome::Cancelled {
                    dictated: Some("Write the report tomorrow haste that".to_owned())
                }
            );
            rig.at(5000).await;
            assert_eq!(rig.started(Cue::Cancelled).len(), 0);
            assert_eq!(rig.notices(), Vec::<String>::new());
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_spotter_that_fails_mid_capture_is_one_notice_and_the_transcript_ending_still_delivers() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, spot, engine, frames) = Rig::spotting("{}", Recorder::disabled());
            spot.fail_at(5);
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            let notice = shown(&rig).expect("the overlay says so");
            assert!(
                notice.starts_with("Ending phrase detection stopped:")
                    && notice.ends_with("Endings still work from the transcript."),
                "{notice}"
            );
            rig.finals("Hello paste that").await;
            rig.feed_until(1700, Kind::Quiet).await;
            assert_eq!(finished(task).await, accepted(CaptureRoute::Paste, "Hello"));
            assert!(rig.notices().is_empty(), "{:?}", rig.notices());
            assert_eq!(spot.received(), 5, "no frame reaches a failed spotter");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_notice_stays_on_the_overlay_until_words_are_heard_and_artifacts_do_not_clear_it() {
    use Act::{Audio, Final, Partial};
    use Kind::Quiet;
    LocalSet::new()
        .run_until(async {
            let (mut rig, _spot, engine, frames) = hearing("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.play(&[
                Audio(1000, Quiet),
                Final(1000, "Write the report tomorrow haste that"),
                Audio(1700, Quiet),
            ])
            .await;
            let notice = shown(&rig).expect("told after the quiet interval");
            rig.partial("[noise]").await;
            rig.finals("(breathing)").await;
            assert_eq!(shown(&rig), Some(notice), "artifacts are not words");
            rig.play(&[Partial(1800, "and then")]).await;
            assert_eq!(shown(&rig), None, "words clear it");
            assert_eq!(rig.notices(), Vec::<String>::new());
            rig.handle.cancel();
            let _ = finished(task).await;
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_spotter_that_could_not_load_is_told_once_with_the_first_capture_until_words_are_heard() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::build(
                "{}",
                Recorder::disabled(),
                None,
                Some("Spoken endings can only use plain letters. Endings still work from the transcript."),
            );
            let task = capture_task(engine, frames);
            rig.open().await;
            assert_eq!(
                shown(&rig).as_deref(),
                Some("Spoken endings can only use plain letters. Endings still work from the transcript.")
            );
            rig.feed_until(1000, Kind::Voice).await;
            rig.partial("Hello").await;
            assert_eq!(shown(&rig), None);
            assert_eq!(rig.notices(), Vec::<String>::new());
            rig.handle.cancel();
            let _ = finished(task).await;
        })
        .await;
}

/// A clip of `frames` frames numbered from 100, whose first is loud enough to be a wake word.
fn clip(frames: usize) -> Clip {
    Clip::from_frames(
        (0..frames)
            .map(|n| if n == 0 { audible(100) } else { quiet(100 + n) })
            .collect(),
    )
}

/// A feeder on `rig`'s microphone, and whether a clip is playing.
fn feeder_for(rig: &Rig) -> (Feeder, Arc<watch::Sender<bool>>) {
    let feeding = Arc::new(watch::channel(false).0);
    (Feeder::new(&rig.gate, Arc::clone(&feeding)), feeding)
}

/// Starts `clip` with the wake word heard on its first frame.
fn feed_wake(rig: &Rig, feeder: &Feeder, clip: Clip) -> oneshot::Receiver<FeedEnd> {
    let (feed, ended) = Feed::new(clip);
    rig.wake.fire(WakeEvent::Capture);
    feeder.start(feed);
    ended
}

/// Like [`capture_task`], on a source that also plays clips, which it hands back at the end.
fn fed_capture_task(
    mut engine: TestEngine,
    mut frames: Fed<FakeFrames>,
) -> JoinHandle<(CaptureOutcome, Fed<FakeFrames>)> {
    tokio::task::spawn_local(async move {
        let trigger = engine.listen(&mut frames).await.unwrap();
        assert_eq!(trigger, WakeTrigger::Capture);
        let outcome = engine.capture(&mut frames, None).await;
        engine.finish().await;
        (outcome, frames)
    })
}

fn capture_flow_sources(dir: &std::path::Path) -> Vec<String> {
    read_lines(&dir.join("metrics.jsonl"))
        .iter()
        .filter(|row| row["event"] == "desktop_capture_flow")
        .map(|row| row["source"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_fed_file_replaces_the_microphone_at_real_time_pace_and_delivers_like_speech() {
    LocalSet::new()
        .run_until(async {
            let dir = record_dir("fed");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, engine, frames) = Rig::recording("{}", recorder);
            let (feeder, feeding) = feeder_for(&rig);
            let task = fed_capture_task(engine, feeder.wrap(frames));
            let mut ended = feed_wake(&rig, &feeder, clip(50));
            settle().await;
            assert!(*feeding.borrow());
            rig.server.ready();
            settle().await;
            // The room keeps sending frames, numbered below the file's, all through.
            rig.feed_until(230, Kind::Quiet).await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello paste that").await;
            rig.feed_until(1699, Kind::Quiet).await;
            rig.at(1700).await;
            assert_eq!(rig.started(Cue::Accepted), [1700]);

            let (outcome, mut fed) = finished(task).await;
            assert_eq!(outcome, accepted(CaptureRoute::Paste, "Hello"));
            // The file's frames from the end of the wake cue to the ending, one every 80 ms, and none of the room's.
            let sent = tags(&rig.sent());
            let first = 103;
            assert_eq!(
                sent,
                (first..first + i16::try_from(sent.len()).unwrap()).collect::<Vec<_>>()
            );
            assert_eq!(
                sent.len(),
                10,
                "the frames at 240 ms through 960 ms, before the ending"
            );
            assert_eq!(capture_flow_sources(&dir), ["file"]);

            // The file is fed when its last frame is given to the engine, 49 frames in.
            rig.at(49 * FRAME_MS - 1).await;
            assert!(ended.try_recv().is_err());
            assert!(*feeding.borrow());
            rig.at(49 * FRAME_MS).await;
            assert_eq!(ended.try_recv(), Ok(FeedEnd::Played));
            assert!(!*feeding.borrow());
            // Frames nobody read while the file played wait their turn, as the microphone's do;
            // then the microphone is heard again.
            rig.feed.send(quiet(900));
            let mut rest = Vec::new();
            while rest.last() != Some(&900) {
                let queued = tokio::time::timeout(ms(10), fed.next()).await;
                rest.push(queued.unwrap().unwrap().frame[0]);
            }
            rest.pop();
            assert_eq!(rest.last(), Some(&149), "the file's last frame");
            assert!(rest.windows(2).all(|pair| pair[1] == pair[0] + 1));
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_fed_frame_is_lost_while_the_microphone_gate_is_off() {
    LocalSet::new()
        .run_until(async {
            let (rig, _engine, frames) = Rig::new("{}");
            let (feeder, _feeding) = feeder_for(&rig);
            let mut fed = feeder.wrap(frames);
            rig.gate.set_enabled(false);
            let (lost, mut lost_end) = Feed::new(clip(3));
            feeder.start(lost);
            settle().await;
            rig.at(300).await;
            assert_eq!(lost_end.try_recv(), Ok(FeedEnd::Played));
            let next = tokio::time::timeout(ms(10), fed.next()).await;
            assert!(next.is_err(), "nothing reached the engine");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn cancel_stops_the_feed_and_the_capture_it_started_and_a_second_file_waits_its_turn() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let (feeder, feeding) = feeder_for(&rig);
            let task = fed_capture_task(engine, feeder.wrap(frames));
            let mut ended = feed_wake(&rig, &feeder, clip(50));
            settle().await;
            rig.server.ready();
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Hello").await;

            let (second, mut refused) = Feed::new(clip(5));
            feeder.start(second);
            assert_eq!(
                refused.try_recv(),
                Ok(FeedEnd::Refused("A file is already being fed."))
            );

            rig.handle.cancel();
            feeder.cancel();
            let (outcome, mut fed) = finished(task).await;
            assert_eq!(
                outcome,
                CaptureOutcome::Cancelled {
                    dictated: Some("Hello".to_owned())
                }
            );
            settle().await;
            assert_eq!(ended.try_recv(), Ok(FeedEnd::Cancelled));
            assert!(!*feeding.borrow());
            // What the file still held is gone; the next frame is the microphone's.
            rig.at(1500).await;
            rig.feed.send(quiet(900));
            assert_eq!(
                tokio::time::timeout(ms(10), fed.next())
                    .await
                    .map(|queued| queued.unwrap().frame[0]),
                Ok(900)
            );

            // Cancelled before any wake, a feed stops just the same, and what it had played unread is gone.
            let (unheard, mut unheard_end) = Feed::new(clip(50));
            feeder.start(unheard);
            rig.at(1900).await;
            assert!(*feeding.borrow());
            feeder.cancel();
            settle().await;
            assert_eq!(unheard_end.try_recv(), Ok(FeedEnd::Cancelled));
            assert!(!*feeding.borrow());
            rig.feed.send(quiet(901));
            let next = tokio::time::timeout(ms(10), fed.next()).await;
            assert_eq!(next.unwrap().unwrap().frame[0], 901);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_long_buffered_hold_is_not_cut_off_as_idle() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            rig.server.buffer_audio();
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.feed_until(61_000, Kind::Voice).await;
            rig.release(61_100).await;
            rig.finals("a long dictation").await;
            let got = finished(task).await;
            assert!(matches!(got, CaptureOutcome::Accepted(_)), "{got:?}");
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn previews_from_a_buffering_recognizer_are_shown_but_never_paste_end_or_cancel_a_capture() {
    // (a hold, what the previews say, the final, the outcome)
    type Case = (bool, [&'static str; 2], &'static str, CaptureOutcome);
    let cases: [Case; 2] = [
        (
            true,
            ["draft words send that", "draft never mind"],
            "draft words",
            accepted(CaptureRoute::Paste, "draft words"),
        ),
        (
            false,
            ["Run tests submit", "never mind"],
            "Run tests submit",
            accepted(CaptureRoute::Submit, "Run tests"),
        ),
    ];
    LocalSet::new()
        .run_until(async {
            for (hold, previews, last, outcome) in cases {
                let label = format!("hold {hold}");
                let (mut rig, engine, frames) = Rig::new(r#"{"ending_pause":{"paste":1.0}}"#);
                rig.server.buffer_audio();
                let task = if hold {
                    hold_task(engine, frames)
                } else {
                    capture_task(engine, frames)
                };
                if hold {
                    rig.hold().await;
                } else {
                    rig.open().await;
                }
                rig.feed_until(1200, Kind::Voice).await;
                for preview in previews {
                    rig.partial(preview).await;
                    assert_eq!(
                        rig.state(),
                        EngineState::Capturing {
                            committed: String::new(),
                            partial: Some(preview.to_owned()),
                            notice: None
                        },
                        "{label}: the overlay shows the preview"
                    );
                }
                assert!(!task.is_finished(), "{label}: no preview ended the capture");
                for cue in [Cue::Provisional, Cue::Cancelled, Cue::Accepted, Cue::Error] {
                    assert!(rig.started(cue).is_empty(), "{label}: {cue:?} played");
                }
                assert!(!rig.server.calls().contains(&FakeCall::FinishAudio));

                if hold {
                    rig.release(1400).await;
                } else {
                    rig.feed_until(3500, Kind::Quiet).await;
                }
                assert!(
                    matches!(rig.state(), EngineState::TranscribingLive { .. }),
                    "{label}: {:?}",
                    rig.state()
                );
                rig.finals(last).await;
                assert_eq!(finished(task).await, outcome, "{label}");
                let finishes = rig
                    .server
                    .calls()
                    .iter()
                    .filter(|call| matches!(call, FakeCall::FinishAudio))
                    .count();
                assert_eq!(finishes, 1, "{label}");
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_released_buffered_hold_waits_as_long_as_its_recognizer_needs_for_the_final() {
    let too_long = CaptureError::Recognizer(RecognizerError::said(
        "The speech server took too long to transcribe this capture. Nothing was delivered.",
    ));
    let cases = [
        (
            30_000,
            15_000,
            accepted(CaptureRoute::Paste, "slow but in time"),
        ),
        (30_000, 30_001, failed(too_long.clone())),
        (11_000, 11_001, failed(too_long)),
    ];
    for (wait, arrives_after, outcome) in cases {
        LocalSet::new()
            .run_until(async {
                let (mut rig, engine, frames) = Rig::new("{}");
                rig.server.buffer_audio();
                rig.server.final_wait(ms(wait));
                let task = hold_task(engine, frames);
                rig.hold().await;
                rig.feed_until(2_000, Kind::Voice).await;
                rig.release(2_100).await;
                rig.at(2_100 + arrives_after).await;
                rig.finals("slow but in time").await;
                assert_eq!(
                    finished(task).await,
                    outcome,
                    "wait {wait} ms, final after {arrives_after} ms"
                );
            })
            .await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_release_skips_silence_when_the_session_flushes_on_end() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            rig.server.flushes_on_end();
            let task = hold_task(engine, frames);
            rig.hold().await;
            rig.feed_until(500, Kind::Voice).await;
            let before = rig.sent();
            rig.release(700).await;
            assert_eq!(
                rig.sent(),
                before,
                "no synthetic silence frames reach a session that flushes on input_ended"
            );
            assert_eq!(rig.server.input_ended_calls(), 1);
            task.abort();
            let _ = task.await;
        })
        .await;
}
#[tokio::test(start_paused = true)]
async fn a_hold_release_returns_the_paste_before_the_accepted_cue_finishes() {
    LocalSet::new()
        .run_until(async {
            for cleanup in [false, true] {
                let (url, _asked, _feed) = chat(Chat::After(50, says!("hello world"))).await;
                let config = if cleanup {
                    cleaning(&url)
                } else {
                    "{}".to_owned()
                };
                let (mut rig, mut engine, mut frames) = Rig::new(&config);
                rig.cues.never_finishes(Cue::Accepted);
                rig.cues.mutes(&rig.gate);
                let task = tokio::task::spawn_local(async move {
                    assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                    let outcome = engine.capture(&mut frames, None).await;
                    (outcome, engine)
                });
                rig.hold().await;
                rig.say(1000, Say::Partial("hello world")).await;
                rig.release(1200).await;
                rig.say(1250, Say::Final("hello world")).await;
                if cleanup {
                    rig.at(1300).await;
                }
                assert_eq!(rig.started(Cue::Accepted), [1250]);
                assert!(
                    task.is_finished(),
                    "paste is ready while the accepted cue still plays"
                );
                let (outcome, engine) = task.await.unwrap();
                assert_eq!(outcome, accepted(CaptureRoute::Paste, "hello world"));
                assert!(!rig.cue_log().iter().any(|(_, log)| matches!(
                    log,
                    CueLog::Finished(Cue::Accepted) | CueLog::Cut(Cue::Accepted)
                )));
                assert!(
                    !rig.gate.is_enabled(),
                    "the remaining cue still mutes input"
                );
                rig.handle.cancel();
                settle().await;
                tokio::time::timeout(ms(5_000), engine.finish())
                    .await
                    .expect("cancel stops the remaining cue instead of blocking the caller");
                assert!(
                    rig.gate.is_enabled(),
                    "input returns after the remaining cue stops"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_microphone_error_while_listening_does_not_cut_the_previous_accepted_cue() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, mut engine, mut frames) = Rig::new("{}");
            rig.cues.set_length(Cue::Accepted, ms(1000));
            let task = tokio::task::spawn_local(async move {
                assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                assert_eq!(
                    engine.capture(&mut frames, None).await,
                    accepted(CaptureRoute::Paste, "hello world")
                );
                let next = engine.listen(&mut frames).await;
                (engine, next)
            });
            rig.hold().await;
            rig.say(1000, Say::Partial("hello world")).await;
            rig.release(1200).await;
            rig.say(1250, Say::Final("hello world")).await;
            assert_eq!(rig.started(Cue::Accepted), [1250]);
            rig.feed.fail(AudioError::DeviceLost);
            let (engine, next) = finished(task).await;
            assert!(matches!(
                next,
                Err(ListenError::Audio(AudioError::DeviceLost))
            ));
            assert!(!rig.cue_log().contains(&(1250, CueLog::Cut(Cue::Accepted))));
            engine.finish().await;
            assert!(
                rig.cue_log()
                    .contains(&(2250, CueLog::Finished(Cue::Accepted)))
            );
        })
        .await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_native_cue_player_reports_the_latest_calls_first_sample_timing() {
    use std::io::Write;
    use std::time::Instant;
    use thalvi_desktop::cues::{CueError, Cues, Mic};
    use thalvi_desktop::engine::{CpalCuePlayer, CuePlayer};

    const TEST: &str = "the_native_cue_player_reports_the_latest_calls_first_sample_timing";
    const CHECKED: &str = "Checked the native cue timing";
    if std::env::var_os("THALVI_PRIVATE_ALSA_TEST").is_some() {
        let player = CpalCuePlayer::new(Cues::default(), InputGate::default());
        let called = Instant::now();
        assert_eq!(player.timing_since(called), None);
        let outcome = player.play(Cue::Wake, Mic::Open).await;
        let timing = player
            .timing_since(called)
            .expect("the output callback reported the first sample");
        match outcome {
            Ok(returned) => assert_eq!(returned, Some(timing)),
            // ALSA's null output may reject pausing after the sample has arrived.
            Err(CueError::Failed(error)) if error.to_string().contains("snd_pcm_pause") => {}
            Err(error) => panic!("{error}"),
        }
        assert_eq!(player.timing_since(Instant::now()), None);
        println!("{CHECKED}");
        return;
    }

    // ALSA reads its configuration process-wide, so only the child gets a software output.
    let config = std::env::temp_dir().join(format!("thalvi-cue-alsa-{}.conf", std::process::id()));
    std::fs::File::create_new(&config)
        .unwrap()
        .write_all(b"pcm.!default { type null }\n")
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("ALSA_CONFIG_PATH", &config)
            .env("THALVI_PRIVATE_ALSA_TEST", "1")
            .kill_on_drop(true)
            .output(),
    )
    .await;
    std::fs::remove_file(config).unwrap();
    let result = result
        .expect("the native cue timing test finished")
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success() && stdout.contains(CHECKED),
        "{stdout}\n{stderr}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_next_hold_cuts_the_old_end_cue_and_hears_words_from_the_press() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    LocalSet::new()
        .run_until(async {
            let dir = record_dir("consecutive-holds");
            let recorder = Recorder::in_dir(METRICS_ONLY, &dir).unwrap();
            let (mut rig, mut engine, mut frames) = Rig::recording("{}", recorder);
            rig.cues.mutes(&rig.gate);
            rig.cues.set_length(Cue::Accepted, ms(10_000));
            let task = tokio::task::spawn_local(async move {
                assert_eq!(engine.listen(&mut frames).await.unwrap(), WakeTrigger::Hold);
                let outcome = engine.capture(&mut frames, None).await;
                (outcome, engine, frames)
            });
            rig.hold().await;
            rig.say(1000, Say::Partial("first words")).await;
            rig.release(1200).await;
            rig.say(1250, Say::Final("first words")).await;
            assert!(task.is_finished(), "the first final completes the hold");
            let (outcome, mut engine, mut frames) = finished(task).await;
            assert_eq!(outcome, accepted(CaptureRoute::Paste, "first words"));
            assert!(
                !rig.gate.is_enabled(),
                "the first ending cue is still playing"
            );

            rig.server.connect_again();
            rig.at(1300).await;
            {
                let listen = engine.listen(&mut frames);
                tokio::pin!(listen);
                poll_fn(|cx| {
                    assert!(listen.as_mut().poll(cx).is_pending());
                    rig.key.set(true);
                    assert!(listen.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                settle().await;
                // A frame can arrive after the cut cue opens the gate and before listen returns.
                assert!(rig.gate.is_enabled());
                rig.feed.send(quiet(54));
                assert_eq!(listen.await.unwrap(), WakeTrigger::Hold);
            }
            let task = tokio::task::spawn_local(async move {
                let outcome = engine.capture(&mut frames, None).await;
                (outcome, engine)
            });
            rig.server.ready();
            settle().await;
            assert!(
                rig.gate.is_enabled(),
                "the next hold hears from the press while the old end cue is cut: {:?}, {:?}",
                rig.state(),
                rig.cue_log()
            );
            assert!(rig.cue_log().contains(&(1300, CueLog::Cut(Cue::Accepted))));
            assert_eq!(
                rig.started(Cue::Wake),
                [0, 1300],
                "the new start cue does not wait behind the old end cue"
            );
            rig.at(1350).await;
            rig.feed.send(quiet(55));
            settle().await;
            rig.at(1530).await;
            let sent = tags(&rig.sent());
            assert!(sent.contains(&54), "the hold handoff keeps its first frame");
            assert!(
                sent.contains(&55),
                "words over the second start cue reach recognition"
            );
            rig.say(1600, Say::Partial("second words")).await;
            rig.release(1800).await;
            rig.say(1850, Say::Final("second words")).await;
            assert!(task.is_finished(), "the second final completes the hold");
            let (outcome, engine) = finished(task).await;
            assert_eq!(outcome, accepted(CaptureRoute::Paste, "second words"));
            rig.handle.cancel();
            settle().await;
            engine.finish().await;
            let rows = read_lines(&dir.join("metrics.jsonl"));
            assert!(
                rows.iter().any(|row| row["event"] == "desktop_end_cue"
                    && row["cue"] == "accepted"
                    && row["outcome"] == "cancelled"),
                "cutting the old cue keeps its cancellation metric"
            );
            let cut = rows
                .iter()
                .find(|row| row["event"] == "desktop_end_cue" && row["outcome"] == "cancelled")
                .unwrap();
            assert_eq!(
                cut["start_ms"], 25.0,
                "cutting a sounding cue keeps its first-sample timing"
            );
            assert_eq!(cut["output_ms"], 15.0);
            assert_eq!(cut["audible_ms"], 40.0);
        })
        .await;
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn live_submit_waits_for_preview_and_keeps_the_full_pause_after_it_is_shown() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Run tests submit").await;
            let preview = rig.live_submit_preview("Run tests", false).await;
            rig.feed_until(1400, Kind::Quiet).await;
            assert!(!task.is_finished(), "must wait for preview before counting");
            preview.shown();
            settle().await;
            rig.feed_until(1400 + 2499, Kind::Quiet).await;
            assert!(
                !task.is_finished(),
                "full cancellation interval runs after the review is shown"
            );
            rig.at(1400 + 2500).await;
            assert_eq!(
                finished(task).await,
                accepted(CaptureRoute::Submit, "Run tests")
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn live_submit_keeps_the_text_if_the_full_preview_is_not_visible() {
    LocalSet::new()
        .run_until(async {
            for (presented, reported, reason) in [
                (false, Some(PreviewError::TooLong), PreviewError::TooLong),
                (false, None, PreviewError::Unavailable),
                (
                    true,
                    Some(PreviewError::Unavailable),
                    PreviewError::Unavailable,
                ),
            ] {
                let (mut rig, engine, frames) = Rig::new("{}");
                let task = capture_task(engine, frames);
                rig.open().await;
                rig.feed_until(1000, Kind::Quiet).await;
                rig.finals("Run tests submit").await;
                let preview = rig.live_submit_preview("Run tests", false).await;
                if presented {
                    preview.shown();
                    settle().await;
                    rig.feed_until(2000, Kind::Quiet).await;
                }
                if let Some(error) = reported {
                    preview.failed(error);
                } else {
                    rig.feed_until(2000, Kind::Quiet).await;
                }
                assert_eq!(
                    finished(task).await,
                    CaptureOutcome::Failed {
                        error: reason.into(),
                        recoverable_text: Some("Run tests".to_owned()),
                    }
                );
                assert_eq!(rig.started(Cue::Accepted).len(), 0);
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn speaking_more_words_cancels_a_live_submit_preview_without_timing_out() {
    LocalSet::new()
        .run_until(async {
            let (mut rig, engine, frames) = Rig::new("{}");
            let task = capture_task(engine, frames);
            rig.open().await;
            rig.feed_until(1000, Kind::Quiet).await;
            rig.finals("Run tests submit").await;
            let _preview = rig.live_submit_preview("Run tests", false).await;
            rig.feed_until(1500, Kind::Quiet).await;
            rig.partial("actually wait").await;
            rig.feed_until(3000, Kind::Quiet).await;
            assert!(!task.is_finished(), "old preview timeout was cleared");
            rig.handle.cancel();
            assert!(matches!(
                finished(task).await,
                CaptureOutcome::Cancelled { .. }
            ));
        })
        .await;
}

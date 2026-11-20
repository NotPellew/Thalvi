//! Owns the musical earcons: their names, per-cue switches, synthesized waveforms and playback.

use std::f64::consts::PI;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    ErrorKind, FromSample, OutputCallbackInfo, OutputStreamTimestamp, SampleFormat, SizedSample,
    Stream,
};

use crate::audio::InputGate;

/// Peak level of every cue.
const AMPLITUDE: f64 = 0.15;

/// How long past a cue's own length playback may take before the output counts as stuck.
const STALL_MARGIN: Duration = Duration::from_secs(2);

/// The five earcons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    Wake,
    Provisional,
    Cancelled,
    Accepted,
    Error,
}

/// Whether the microphone hears a cue while it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mic {
    /// Input is dropped until the cue ends, so the recognizer never hears it.
    Muted,
    /// Input goes on: the user may already be talking, as at the start of a hold.
    Open,
}

/// Which cues are switched on; all are by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per cue, named as in the config"
)]
pub struct Cues {
    pub wake: bool,
    pub provisional: bool,
    pub cancelled: bool,
    pub accepted: bool,
    pub error: bool,
}

impl Default for Cues {
    fn default() -> Self {
        Self {
            wake: true,
            provisional: true,
            cancelled: true,
            accepted: true,
            error: true,
        }
    }
}

impl Cues {
    /// Whether `cue` is switched on.
    #[must_use]
    pub fn enabled(&self, cue: Cue) -> bool {
        match cue {
            Cue::Wake => self.wake,
            Cue::Provisional => self.provisional,
            Cue::Cancelled => self.cancelled,
            Cue::Accepted => self.accepted,
            Cue::Error => self.error,
        }
    }
}

/// A cue's notes as (frequency in Hz, duration in seconds).
fn notes(cue: Cue) -> &'static [(f64, f64)] {
    match cue {
        Cue::Wake => &[(523.25, 0.09), (783.99, 0.14)],
        Cue::Provisional => &[(659.25, 0.07), (880.0, 0.07)],
        Cue::Cancelled => &[(783.99, 0.09), (523.25, 0.14)],
        Cue::Accepted => &[(523.25, 0.08), (659.25, 0.08), (783.99, 0.16)],
        Cue::Error => &[(261.63, 0.14), (369.99, 0.20)],
    }
}

/// Renders `cue` as mono samples at `rate` Hz: sine notes under a half-sine envelope.
#[must_use]
pub fn synthesize(cue: Cue, rate: u32) -> Vec<f32> {
    let rate = f64::from(rate);
    notes(cue)
        .iter()
        .flat_map(|&(frequency, duration)| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the note length is a small positive number of samples"
            )]
            let count = (rate * duration) as u32;
            (0..count).map(move |n| {
                let t = f64::from(n) / rate;
                #[expect(clippy::cast_possible_truncation, reason = "audio samples are f32")]
                let sample = (AMPLITUDE
                    * (2.0 * PI * frequency * t).sin()
                    * (PI * t / duration).sin()) as f32;
                sample
            })
        })
        .collect()
}

/// When a cue's first sample reached the output, measured from the call that played it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CueTiming {
    /// Until the output callback that wrote the first sample: our code and the stream start.
    pub start: Duration,
    /// The delay the device reports after that callback; none when the backend reports none.
    pub output: Option<Duration>,
    /// The output stream was rebuilt for this cue.
    pub rebuilt: bool,
}

impl CueTiming {
    /// When the first sample should reach the speaker; unknown when the device reports no delay.
    #[must_use]
    pub fn audible(&self) -> Option<Duration> {
        self.output.map(|output| self.start + output)
    }
}

/// The delay between an output callback and the device playing what it wrote.
fn output_delay(stamp: OutputStreamTimestamp) -> Option<Duration> {
    stamp.playback.checked_duration_since(stamp.callback)
}

/// Why a cue did not play; `Display` is the plain notice for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CueError {
    /// No output device exists.
    NoOutputDevice,
    /// The output device failed to open or while playing.
    Failed(cpal::Error),
    /// The output device stopped consuming audio.
    Stalled,
}

impl fmt::Display for CueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Audio feedback failed. Check the output device.")
    }
}

impl std::error::Error for CueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

enum Phase {
    Playing,
    /// Every sample is with the device; the last one sounds at this instant.
    Drained(Instant),
    Cancelled,
    Failed(cpal::Error),
}

/// One cue being played, shared between the output callback and the waiting caller.
struct Playback {
    phase: Mutex<Phase>,
    changed: Condvar,
    /// The instant of the callback that wrote the first sample, and the device delay it reported.
    first_sample: OnceLock<(Instant, Option<Duration>, bool)>,
    called: Instant,
}

impl Playback {
    fn new(called: Instant) -> Self {
        Self {
            phase: Mutex::new(Phase::Playing),
            changed: Condvar::new(),
            first_sample: OnceLock::new(),
            called,
        }
    }

    fn phase(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, next: Phase) {
        let mut phase = self.phase();
        // Stopping or a device error still counts while the tail is sounding.
        let replaces = matches!(
            (&*phase, &next),
            (Phase::Playing, _) | (Phase::Drained(_), Phase::Cancelled | Phase::Failed(_))
        );
        if replaces {
            *phase = next;
            self.changed.notify_all();
        }
    }

    /// Blocks until the last sample has sounded, the cue is cancelled, or `limit` passes.
    fn wait(&self, limit: Duration) -> Result<(), CueError> {
        let deadline = Instant::now() + limit;
        let mut phase = self.phase();
        loop {
            let now = Instant::now();
            let until = match &*phase {
                Phase::Cancelled => return Ok(()),
                Phase::Failed(error) => return Err(CueError::Failed(error.clone())),
                Phase::Drained(at) if *at <= now => return Ok(()),
                Phase::Drained(at) => (*at).min(deadline),
                Phase::Playing => deadline,
            };
            if until <= now {
                return Err(CueError::Stalled);
            }
            phase = self
                .changed
                .wait_timeout(phase, until - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Plays cues on the default output, muting the microphone for those it must not hear.
pub struct CueEngine {
    cues: Cues,
    gate: InputGate,
    serial: Mutex<()>,
    last: Mutex<Option<Arc<Playback>>>,
    output: Box<dyn Output + Send + Sync>,
    kept: Mutex<Option<Kept>>,
}

impl CueEngine {
    /// An engine that plays the cues switched on in `cues`, muting `gate` while a [`Mic::Muted`] one plays.
    #[must_use]
    pub fn new(cues: Cues, gate: InputGate) -> Self {
        Self {
            cues,
            gate,
            serial: Mutex::new(()),
            last: Mutex::new(None),
            output: Box::new(CpalOutput),
            kept: Mutex::new(None),
        }
    }

    /// Attempts to prepare a paused output; a failure is retried by the next cue.
    pub(crate) fn warm_up(&self) {
        let _turn = self.serial.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cache = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.is_none()
            && let Ok(kept) = self.output.build()
        {
            *cache = Some(kept);
        }
    }

    /// Plays `cue` and returns when it has finished, was stopped, or failed. `called` is when the
    /// caller asked for it; the timing is none when the cue never reached the output.
    ///
    /// Does nothing if the cue is switched off. With [`Mic::Muted`], microphone input is dropped from
    /// the start of the cue until it ends and is restored only if it was on before. Concurrent calls
    /// play in turn.
    /// It blocks, so call it from a blocking context.
    ///
    /// # Errors
    ///
    /// Why the cue could not be played.
    pub fn play(&self, cue: Cue, mic: Mic, called: Instant) -> Result<Option<CueTiming>, CueError> {
        let mut timing = None;
        self.play_gated(cue, called, |playback| {
            let id = self.output.default_device()?;
            let mut cache = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            let (kept, rebuilt) = match cache.take() {
                Some(kept) if kept.id == id && !kept.state.broken.load(Ordering::Acquire) => {
                    (kept, false)
                }
                _ => (self.output.build()?, true),
            };
            let _muted = (mic == Mic::Muted).then(|| Muted::new(&self.gate));
            let rate = kept.speaker.rate();
            *kept
                .state
                .voice
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Voice {
                samples: synthesize(cue, rate),
                position: 0,
                channels: kept.channels,
                rate,
                rebuilt,
                playback: Arc::clone(playback),
            });
            let limit =
                Duration::from_secs_f64(notes(cue).iter().map(|note| note.1).sum()) + STALL_MARGIN;
            let result = kept.speaker.play().and_then(|()| playback.wait(limit));
            let paused = kept.speaker.pause();
            if result.is_err() || paused.is_err() {
                kept.state.broken.store(true, Ordering::Release);
            }
            *kept
                .state
                .voice
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            timing = playback
                .first_sample
                .get()
                .map(|&(at, output, rebuilt)| CueTiming {
                    start: at.saturating_duration_since(called),
                    output,
                    rebuilt,
                });
            *cache = Some(kept);
            result.and(paused)
        })?;
        Ok(timing)
    }

    /// The most recent cue's first sample, if that cue was called at or after `since`.
    pub(crate) fn timing_since(&self, since: Instant) -> Option<CueTiming> {
        let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let playback = last.as_ref()?;
        let &(at, output, rebuilt) = playback.first_sample.get()?;
        (playback.called >= since).then_some(CueTiming {
            start: at.saturating_duration_since(playback.called),
            output,
            rebuilt,
        })
    }

    /// Cuts the cue that is playing short; `play` then returns `Ok`.
    pub fn stop(&self) {
        if let Some(playback) = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            playback.set(Phase::Cancelled);
        }
    }

    fn play_gated(
        &self,
        cue: Cue,
        called: Instant,
        run: impl FnOnce(&Arc<Playback>) -> Result<(), CueError>,
    ) -> Result<(), CueError> {
        if !self.cues.enabled(cue) {
            return Ok(());
        }
        let _turn = self.serial.lock().unwrap_or_else(PoisonError::into_inner);
        let playback = Arc::new(Playback::new(called));
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&playback));
        run(&playback)
    }
}

/// Keeps input off for its lifetime and turns it back on after, if it was on before.
struct Muted<'a> {
    gate: &'a InputGate,
    was_enabled: bool,
}

impl<'a> Muted<'a> {
    fn new(gate: &'a InputGate) -> Self {
        let was_enabled = gate.is_enabled();
        gate.set_enabled(false);
        Self { gate, was_enabled }
    }
}

impl Drop for Muted<'_> {
    fn drop(&mut self) {
        if self.was_enabled {
            self.gate.set_enabled(true);
        }
    }
}

trait Output {
    fn default_device(&self) -> Result<cpal::DeviceId, CueError>;
    /// Builds the stream in its initial stopped state.
    fn build(&self) -> Result<Kept, CueError>;
}

trait Speaker {
    fn rate(&self) -> u32;
    fn play(&self) -> Result<(), CueError>;
    fn pause(&self) -> Result<(), CueError>;
}

#[derive(Default)]
struct OutputState {
    voice: Mutex<Option<Voice>>,
    broken: AtomicBool,
}

struct Kept {
    id: cpal::DeviceId,
    channels: usize,
    speaker: Box<dyn Speaker + Send>,
    state: Arc<OutputState>,
}

struct CpalOutput;

impl Output for CpalOutput {
    fn default_device(&self) -> Result<cpal::DeviceId, CueError> {
        cpal::default_host()
            .default_output_device()
            .ok_or(CueError::NoOutputDevice)?
            .id()
            .map_err(CueError::Failed)
    }

    fn build(&self) -> Result<Kept, CueError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(CueError::NoOutputDevice)?;
        let id = device.id().map_err(CueError::Failed)?;
        let state = Arc::new(OutputState::default());
        let supported = device.default_output_config().map_err(CueError::Failed)?;
        let rate = supported.sample_rate();
        let config = supported.config();
        let channels = usize::from(config.channels.max(1));
        let callback = Arc::clone(&state);
        let stream = match supported.sample_format() {
            SampleFormat::F32 => output::<f32>(&device, config, callback),
            SampleFormat::I16 => output::<i16>(&device, config, callback),
            SampleFormat::I32 => output::<i32>(&device, config, callback),
            SampleFormat::U16 => output::<u16>(&device, config, callback),
            SampleFormat::F64 => output::<f64>(&device, config, callback),
            other => Err(cpal::Error::with_message(
                ErrorKind::UnsupportedConfig,
                format!("speaker sample format {other} is not supported"),
            )),
        }
        .map_err(CueError::Failed)?;
        Ok(Kept {
            id,
            channels,
            speaker: Box::new(CpalSpeaker { stream, rate }),
            state,
        })
    }
}

struct CpalSpeaker {
    stream: Stream,
    rate: u32,
}

impl Speaker for CpalSpeaker {
    fn rate(&self) -> u32 {
        self.rate
    }
    fn play(&self) -> Result<(), CueError> {
        self.stream.play().map_err(CueError::Failed)
    }
    fn pause(&self) -> Result<(), CueError> {
        self.stream.pause().map_err(CueError::Failed)
    }
}

fn output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    state: Arc<OutputState>,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let errors = Arc::clone(&state);
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &OutputCallbackInfo| {
            let mut voice = state.voice.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(voice) = voice.as_mut() {
                voice.fill(data, info);
            } else {
                data.fill(T::from_sample(0.0));
            }
        },
        move |error| {
            errors.broken.store(true, Ordering::Release);
            if let Some(voice) = errors
                .voice
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                voice.playback.set(Phase::Failed(error));
            }
        },
        Some(Duration::from_secs(5)),
    )
}

/// The output callback's state.
struct Voice {
    samples: Vec<f32>,
    position: usize,
    channels: usize,
    rate: u32,
    rebuilt: bool,
    playback: Arc<Playback>,
}

impl Voice {
    fn fill<T: SizedSample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        info: &OutputCallbackInfo,
    ) {
        let start = self.position;
        for frame in data.chunks_mut(self.channels) {
            let value = self.samples.get(self.position).copied().unwrap_or(0.0);
            self.position += 1;
            frame.fill(T::from_sample(value));
        }
        let stamp = info.timestamp();
        if start == 0 {
            let _ =
                self.playback
                    .first_sample
                    .set((Instant::now(), output_delay(stamp), self.rebuilt));
        }
        if start < self.samples.len() && self.position >= self.samples.len() {
            let latency = output_delay(stamp).unwrap_or_default();
            let written = self.samples.len() - start;
            let written = u32::try_from(written).unwrap_or(u32::MAX);
            let tail = Duration::from_secs_f64(f64::from(written) / f64::from(self.rate));
            self.playback
                .set(Phase::Drained(Instant::now() + latency + tail));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    const RATE: u32 = 24_000;

    #[derive(Default)]
    struct FakeOutput {
        device: std::sync::atomic::AtomicUsize,
        builds: Mutex<Vec<usize>>,
        plays: Mutex<Vec<usize>>,
        states: Mutex<Vec<Arc<OutputState>>>,
        fail_build: AtomicBool,
        fail_play: AtomicBool,
        wait_for_build: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
        wait_for_stop: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        gate: InputGate,
        build_input: Mutex<Vec<bool>>,
        play_input: Mutex<Vec<bool>>,
        paused: AtomicBool,
    }

    impl Output for Arc<FakeOutput> {
        fn default_device(&self) -> Result<cpal::DeviceId, CueError> {
            Ok(cpal::DeviceId::new(
                cpal::default_host().id(),
                self.device.load(Ordering::SeqCst).to_string(),
            ))
        }

        fn build(&self) -> Result<Kept, CueError> {
            let id = self.default_device()?;
            let state = Arc::new(OutputState::default());
            self.build_input
                .lock()
                .unwrap()
                .push(self.gate.is_enabled());
            let device = id.id().parse().unwrap();
            self.builds.lock().unwrap().push(device);
            if let Some((started, resume)) = self.wait_for_build.lock().unwrap().take() {
                started.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if self.fail_build.load(Ordering::SeqCst) {
                return Err(CueError::NoOutputDevice);
            }
            self.paused.store(true, Ordering::SeqCst);
            self.states.lock().unwrap().push(Arc::clone(&state));
            Ok(Kept {
                id,
                channels: 1,
                speaker: Box::new(FakeSpeaker {
                    output: Arc::clone(self),
                    device,
                    state: Arc::clone(&state),
                }),
                state,
            })
        }
    }

    struct FakeSpeaker {
        output: Arc<FakeOutput>,
        device: usize,
        state: Arc<OutputState>,
    }

    impl Speaker for FakeSpeaker {
        fn rate(&self) -> u32 {
            RATE
        }
        fn play(&self) -> Result<(), CueError> {
            self.output.paused.store(false, Ordering::SeqCst);
            self.output.plays.lock().unwrap().push(self.device);
            self.output
                .play_input
                .lock()
                .unwrap()
                .push(self.output.gate.is_enabled());
            if self.output.fail_play.load(Ordering::SeqCst) {
                return Err(CueError::NoOutputDevice);
            }
            let voice = self.state.voice.lock().unwrap();
            let playback = &voice.as_ref().unwrap().playback;
            playback
                .first_sample
                .set((
                    Instant::now(),
                    Some(Duration::ZERO),
                    voice.as_ref().unwrap().rebuilt,
                ))
                .unwrap();
            if let Some(started) = self.output.wait_for_stop.lock().unwrap().take() {
                started.send(()).unwrap();
            } else {
                playback.set(Phase::Drained(Instant::now()));
            }
            Ok(())
        }
        fn pause(&self) -> Result<(), CueError> {
            if self.state.voice.lock().unwrap().is_none() {
                return Err(CueError::Failed(cpal::Error::new(ErrorKind::InvalidInput)));
            }
            self.output.paused.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn fake_engine() -> (CueEngine, Arc<FakeOutput>) {
        let output = Arc::new(FakeOutput::default());
        let mut engine = CueEngine::new(Cues::default(), output.gate.clone());
        engine.output = Box::new(Arc::clone(&output));
        (engine, output)
    }

    #[test]
    fn the_first_cue_reuses_the_warmed_output_without_playing_during_warmup() {
        let (engine, output) = fake_engine();
        engine.warm_up();
        assert!(
            output.plays.lock().unwrap().is_empty(),
            "warming never plays audio"
        );
        assert!(
            output.paused.load(Ordering::SeqCst),
            "the warmed output stays stopped"
        );
        let first = engine
            .play(Cue::Wake, Mic::Open, Instant::now())
            .unwrap()
            .unwrap();
        assert!(!first.rebuilt, "the first cue reuses the warmed output");
        assert_eq!(*output.builds.lock().unwrap(), [0]);
        assert!(output.paused.load(Ordering::SeqCst));
    }

    #[test]
    fn a_failed_warmup_leaves_the_first_cue_to_rebuild_and_report_it() {
        let (engine, output) = fake_engine();
        output.fail_build.store(true, Ordering::SeqCst);
        engine.warm_up();
        assert!(
            output.plays.lock().unwrap().is_empty(),
            "a failed warmup plays nothing"
        );
        output.fail_build.store(false, Ordering::SeqCst);
        let first = engine
            .play(Cue::Wake, Mic::Open, Instant::now())
            .unwrap()
            .unwrap();
        assert!(
            first.rebuilt,
            "the first cue reports its successful rebuild"
        );
        assert_eq!(
            *output.builds.lock().unwrap(),
            [0, 0],
            "the first cue retries the failed warmup"
        );
    }

    #[test]
    fn two_cues_share_one_output_and_leave_it_paused() {
        let (engine, output) = fake_engine();
        let called = Instant::now();
        let first = engine.play(Cue::Wake, Mic::Open, called).unwrap().unwrap();
        assert_eq!(
            engine.timing_since(called),
            Some(first),
            "a completed cue keeps its first-sample timing"
        );
        assert_eq!(
            engine.timing_since(called + Duration::from_secs(1)),
            None,
            "an old cue's timing cannot belong to a new call"
        );
        let second = engine
            .play(Cue::Accepted, Mic::Muted, Instant::now())
            .unwrap()
            .unwrap();
        assert_eq!(
            *output.builds.lock().unwrap(),
            [0],
            "two cues build the output once"
        );
        assert!(first.rebuilt);
        assert!(!second.rebuilt);
        assert!(
            output.paused.load(Ordering::SeqCst),
            "idle output is paused"
        );
    }

    #[test]
    fn a_default_device_change_rebuilds_before_playing_on_the_new_device() {
        let (engine, output) = fake_engine();
        engine.play(Cue::Wake, Mic::Open, Instant::now()).unwrap();
        output.device.store(1, Ordering::SeqCst);
        let changed = engine
            .play(Cue::Accepted, Mic::Muted, Instant::now())
            .unwrap()
            .unwrap();
        engine.play(Cue::Wake, Mic::Open, Instant::now()).unwrap();
        assert_eq!(
            *output.builds.lock().unwrap(),
            [0, 1],
            "only the changed device rebuilds"
        );
        assert_eq!(
            *output.plays.lock().unwrap(),
            [0, 1, 1],
            "no cue plays on the old device"
        );
        assert!(changed.rebuilt);
    }

    #[test]
    fn a_stream_error_rebuilds_on_the_next_cue_and_reports_a_failed_rebuild() {
        for fails in [false, true] {
            let (engine, output) = fake_engine();
            engine.play(Cue::Wake, Mic::Open, Instant::now()).unwrap();
            engine
                .play(Cue::Accepted, Mic::Muted, Instant::now())
                .unwrap();
            assert_eq!(
                output.builds.lock().unwrap().len(),
                1,
                "healthy stream is reused before error"
            );
            output.states.lock().unwrap()[0]
                .broken
                .store(true, Ordering::Release);
            output.fail_build.store(fails, Ordering::SeqCst);
            let result = engine.play(Cue::Wake, Mic::Open, Instant::now());
            if fails {
                assert_eq!(result, Err(CueError::NoOutputDevice));
                assert_eq!(*output.plays.lock().unwrap(), [0, 0]);
            } else {
                assert!(result.unwrap().unwrap().rebuilt);
                assert_eq!(*output.plays.lock().unwrap(), [0, 0, 0]);
            }
            assert_eq!(*output.builds.lock().unwrap(), [0, 0]);
        }
    }

    #[test]
    fn a_muted_cue_keeps_input_enabled_during_build_and_mutes_only_playback() {
        for enabled in [false, true] {
            for mic in [Mic::Muted, Mic::Open] {
                for fails in [false, true] {
                    let (engine, output) = fake_engine();
                    output.gate.set_enabled(enabled);
                    output.fail_play.store(fails, Ordering::SeqCst);
                    let epoch = output.gate.state().1;
                    let result = engine.play(Cue::Wake, mic, Instant::now());
                    assert_eq!(result.is_ok(), !fails);
                    if fails {
                        assert_eq!(result, Err(CueError::NoOutputDevice));
                    }
                    assert_eq!(
                        *output.build_input.lock().unwrap(),
                        [enabled],
                        "building leaves the input state alone"
                    );
                    assert_eq!(
                        *output.play_input.lock().unwrap(),
                        [enabled && mic == Mic::Open],
                        "only a muted cue turns input off while it sounds"
                    );
                    assert_eq!(
                        output.gate.is_enabled(),
                        enabled,
                        "input returns to its original state even on failure"
                    );
                    assert_eq!(
                        output.gate.state().1 > epoch,
                        mic == Mic::Muted,
                        "an open cue never touches input"
                    );
                }
            }
        }
    }

    /// (Hz, seconds, samples at 24 kHz)
    type Note = (f64, f64, usize);
    const ALL: [Cue; 5] = [
        Cue::Wake,
        Cue::Provisional,
        Cue::Cancelled,
        Cue::Accepted,
        Cue::Error,
    ];

    /// Positive-going plus negative-going zero crossings, which is twice the cycle count.
    fn crossings(samples: &[f32]) -> u32 {
        let count = samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0) && pair[1] != 0.0)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    #[test]
    fn every_cue_has_its_notes_and_lengths() {
        let table: [(Cue, &[Note]); 5] = [
            (Cue::Wake, &[(523.25, 0.09, 2160), (783.99, 0.14, 3360)]),
            (
                Cue::Provisional,
                &[(659.25, 0.07, 1680), (880.0, 0.07, 1680)],
            ),
            (
                Cue::Cancelled,
                &[(783.99, 0.09, 2160), (523.25, 0.14, 3360)],
            ),
            (
                Cue::Accepted,
                &[
                    (523.25, 0.08, 1920),
                    (659.25, 0.08, 1920),
                    (783.99, 0.16, 3840),
                ],
            ),
            (Cue::Error, &[(261.63, 0.14, 3360), (369.99, 0.20, 4800)]),
        ];
        for (cue, expected) in table {
            let samples = synthesize(cue, RATE);
            assert_eq!(
                samples.len(),
                expected.iter().map(|n| n.2).sum::<usize>(),
                "{cue:?} length"
            );
            let mut start = 0;
            for &(frequency, duration, count) in expected {
                let note = &samples[start..start + count];
                start += count;
                let heard = f64::from(crossings(note)) / (2.0 * duration);
                assert!(
                    (heard - frequency).abs() / frequency < 0.03,
                    "{cue:?}: heard {heard:.1} Hz, expected {frequency}"
                );
            }
        }
    }

    #[test]
    fn each_note_swells_from_silence_to_the_cue_level_and_back() {
        // Single-note view of the Wake cue's first note: 0.09 s = 2160 samples at 24 kHz.
        let note = &synthesize(Cue::Wake, RATE)[..2160];
        let peak = note.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert_eq!(note[0], 0.0);
        assert!((0.14..=0.15).contains(&peak), "peak {peak}");
        let edge = note[..100]
            .iter()
            .chain(&note[2060..])
            .fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(edge < 0.02, "the ends must fade, edge level {edge}");
    }

    #[test]
    fn cues_keep_their_pitch_and_length_at_other_device_rates() {
        for rate in [44_100_u32, 48_000] {
            let samples = synthesize(Cue::Provisional, rate);
            let length = u32::try_from(samples.len()).unwrap_or(u32::MAX);
            let seconds = f64::from(length) / f64::from(rate);
            assert!((seconds - 0.14).abs() < 0.001, "{rate} Hz: {seconds} s");
        }
    }

    #[test]
    fn switches_follow_the_cue_they_are_named_for() {
        for cue in ALL {
            let mut cues = Cues::default();
            assert!(cues.enabled(cue));
            match cue {
                Cue::Wake => cues.wake = false,
                Cue::Provisional => cues.provisional = false,
                Cue::Cancelled => cues.cancelled = false,
                Cue::Accepted => cues.accepted = false,
                Cue::Error => cues.error = false,
            }
            let off: Vec<Cue> = ALL.into_iter().filter(|c| !cues.enabled(*c)).collect();
            assert_eq!(off, [cue]);
        }
    }

    #[test]
    fn a_switched_off_cue_neither_plays_nor_touches_input() {
        let gate = InputGate::default();
        let cues = Cues {
            wake: false,
            ..Cues::default()
        };
        let engine = CueEngine::new(cues, gate.clone());
        let epoch_before = gate.state().1;
        let ran = AtomicBool::new(false);
        engine
            .play_gated(Cue::Wake, Instant::now(), |_| {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .expect("nothing to play");
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(gate.state().1, epoch_before);
    }

    #[test]
    fn a_cue_stopped_during_build_never_mutes_or_plays_and_keeps_the_output() {
        let (engine, output) = fake_engine();
        let (started, building) = std::sync::mpsc::channel();
        let (resume, released) = std::sync::mpsc::channel();
        *output.wait_for_build.lock().unwrap() = Some((started, released));
        let input = output.gate.state();
        let called = Instant::now();
        let stopped = std::thread::scope(|scope| {
            let task = scope.spawn(|| engine.play(Cue::Accepted, Mic::Muted, called));
            building.recv_timeout(Duration::from_secs(5)).unwrap();
            engine.stop();
            resume.send(()).unwrap();
            task.join().unwrap()
        });
        assert_eq!(
            output.gate.state(),
            input,
            "a cue cancelled during build must never mute input"
        );
        assert!(
            output.plays.lock().unwrap().is_empty(),
            "a cue cancelled during build must never play"
        );
        assert_eq!(stopped, Ok(None));
        assert_eq!(engine.timing_since(called), None);
        let next = engine
            .play(Cue::Wake, Mic::Open, Instant::now())
            .unwrap()
            .unwrap();
        assert!(
            !next.rebuilt,
            "the next cue reuses the cancelled cue's output"
        );
        assert_eq!(*output.builds.lock().unwrap(), [0]);
        assert_eq!(*output.plays.lock().unwrap(), [0]);
        assert_eq!(output.gate.state(), input);
    }

    #[test]
    fn stop_ends_the_cue_that_is_playing_and_input_returns() {
        let (engine, output) = fake_engine();
        let (started, playing) = std::sync::mpsc::channel();
        *output.wait_for_stop.lock().unwrap() = Some(started);
        std::thread::scope(|scope| {
            let task = scope.spawn(|| engine.play(Cue::Wake, Mic::Muted, Instant::now()));
            playing.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(
                !output.gate.is_enabled(),
                "playback mutes input until stopped"
            );
            engine.stop();
            assert!(task.join().unwrap().is_ok());
        });
        assert!(output.gate.is_enabled());
    }

    #[test]
    fn playback_ends_after_the_last_sample_sounds_not_when_it_is_handed_over() {
        let playback = Playback::new(Instant::now());
        playback.set(Phase::Drained(Instant::now() + Duration::from_millis(60)));
        let started = Instant::now();
        assert_eq!(playback.wait(Duration::from_secs(5)), Ok(()));
        assert!(started.elapsed() >= Duration::from_millis(55));
    }

    #[test]
    fn a_device_that_stops_consuming_audio_is_reported_instead_of_hanging() {
        let playback = Playback::new(Instant::now());
        assert_eq!(
            playback.wait(Duration::from_millis(30)),
            Err(CueError::Stalled)
        );
    }

    #[test]
    fn the_first_cancel_or_error_wins_even_after_the_last_sample_is_handed_over() {
        let error = cpal::Error::new(ErrorKind::DeviceNotAvailable);
        let other = cpal::Error::new(ErrorKind::DeviceBusy);
        let tail = || Phase::Drained(Instant::now() + Duration::from_secs(2));
        let cases: [(&str, Vec<Phase>, Result<(), CueError>); 5] = [
            (
                "cancel during the tail",
                vec![tail(), Phase::Cancelled],
                Ok(()),
            ),
            (
                "error during the tail",
                vec![tail(), Phase::Failed(error.clone())],
                Err(CueError::Failed(error.clone())),
            ),
            (
                "drain does not undo an error",
                vec![Phase::Failed(error.clone()), tail()],
                Err(CueError::Failed(error.clone())),
            ),
            (
                "drain does not undo a cancel",
                vec![Phase::Cancelled, tail()],
                Ok(()),
            ),
            (
                "a later cancel or error does not replace an error",
                vec![
                    Phase::Failed(error.clone()),
                    Phase::Cancelled,
                    Phase::Failed(other),
                ],
                Err(CueError::Failed(error)),
            ),
        ];
        for (name, phases, expected) in cases {
            let playback = Playback::new(Instant::now());
            for phase in phases {
                playback.set(phase);
            }
            let started = Instant::now();
            assert_eq!(playback.wait(Duration::from_secs(5)), expected, "{name}");
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "{name}: must not wait out the tail"
            );
        }
    }

    #[test]
    fn an_output_error_while_playing_is_reported() {
        let playback = Playback::new(Instant::now());
        let error = cpal::Error::new(ErrorKind::DeviceNotAvailable);
        playback.set(Phase::Failed(error.clone()));
        assert_eq!(
            playback.wait(Duration::from_secs(5)),
            Err(CueError::Failed(error))
        );
    }

    #[test]
    fn the_device_delay_is_what_the_backend_reports_after_the_callback_or_nothing() {
        let at = cpal::StreamInstant::from_millis;
        let stamp = |callback, playback| OutputStreamTimestamp {
            callback: at(callback),
            playback: at(playback),
        };
        for (name, callback, playback, expected) in [
            ("playback after callback", 100, 123, Some(23)),
            ("playback at callback", 100, 100, Some(0)),
            ("playback before callback", 100, 90, None),
        ] {
            assert_eq!(
                output_delay(stamp(callback, playback)),
                expected.map(Duration::from_millis),
                "{name}"
            );
        }
    }
}

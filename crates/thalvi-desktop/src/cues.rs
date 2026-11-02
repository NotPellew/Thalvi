//! Owns the musical earcons: their names, per-cue switches, synthesized waveforms and playback.

use std::f64::consts::PI;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, OutputCallbackInfo, SampleFormat, SizedSample, Stream};

use crate::audio::InputGate;

/// Peak level of every cue, as in the Python client.
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

/// Which cues are switched on; all are by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

impl Playback {
    fn new() -> Self {
        Self {
            phase: Mutex::new(Phase::Playing),
            changed: Condvar::new(),
        }
    }

    fn phase(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, next: Phase) {
        let mut phase = self.phase();
        if matches!(*phase, Phase::Playing) {
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

/// Plays cues on the default output while keeping the microphone from hearing them.
pub struct CueEngine {
    cues: Cues,
    gate: InputGate,
    serial: Mutex<()>,
    current: Mutex<Option<Arc<Playback>>>,
}

impl CueEngine {
    /// An engine that plays the cues switched on in `cues`, muting `gate` while each plays.
    #[must_use]
    pub fn new(cues: Cues, gate: InputGate) -> Self {
        Self {
            cues,
            gate,
            serial: Mutex::new(()),
            current: Mutex::new(None),
        }
    }

    /// Plays `cue` and returns when it has finished, was stopped, or failed.
    ///
    /// Does nothing if the cue is switched off. Microphone input is dropped from the start of the
    /// cue until it ends and is restored only if it was on before. Concurrent calls play in turn.
    /// It blocks, so call it from a blocking context.
    ///
    /// # Errors
    ///
    /// Why the cue could not be played.
    pub fn play(&self, cue: Cue) -> Result<(), CueError> {
        self.play_gated(cue, |playback| play_on_default_output(cue, playback))
    }

    /// Cuts the cue that is playing short; `play` then returns `Ok`.
    pub fn stop(&self) {
        if let Some(playback) = self
            .current
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
        run: impl FnOnce(&Arc<Playback>) -> Result<(), CueError>,
    ) -> Result<(), CueError> {
        if !self.cues.enabled(cue) {
            return Ok(());
        }
        let _turn = self.serial.lock().unwrap_or_else(PoisonError::into_inner);
        let _muted = Muted::new(&self.gate);
        let playback = Arc::new(Playback::new());
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&playback));
        let result = run(&playback);
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
        result
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

fn play_on_default_output(cue: Cue, playback: &Arc<Playback>) -> Result<(), CueError> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or(CueError::NoOutputDevice)?;
    let supported = device.default_output_config().map_err(CueError::Failed)?;
    let rate = supported.sample_rate();
    let channels = usize::from(supported.channels().max(1));
    let samples = synthesize(cue, rate);
    let limit = Duration::from_secs_f64(notes(cue).iter().map(|note| note.1).sum()) + STALL_MARGIN;
    let voice = Voice {
        samples,
        position: 0,
        channels,
        rate,
        playback: Arc::clone(playback),
    };
    let config = supported.config();
    let stream: Stream = match supported.sample_format() {
        SampleFormat::F32 => output::<f32>(&device, config, voice),
        SampleFormat::I16 => output::<i16>(&device, config, voice),
        SampleFormat::I32 => output::<i32>(&device, config, voice),
        SampleFormat::U16 => output::<u16>(&device, config, voice),
        SampleFormat::F64 => output::<f64>(&device, config, voice),
        other => Err(cpal::Error::with_message(
            ErrorKind::UnsupportedConfig,
            format!("speaker sample format {other} is not supported"),
        )),
    }
    .map_err(CueError::Failed)?;
    stream.play().map_err(CueError::Failed)?;
    playback.wait(limit)
}

fn output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut voice: Voice,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let playback = Arc::clone(&voice.playback);
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &OutputCallbackInfo| voice.fill(data, info),
        move |error| playback.set(Phase::Failed(error)),
        Some(Duration::from_secs(5)),
    )
}

/// The output callback's state.
struct Voice {
    samples: Vec<f32>,
    position: usize,
    channels: usize,
    rate: u32,
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
        if start < self.samples.len() && self.position >= self.samples.len() {
            let stamp = info.timestamp();
            let latency = stamp.playback.saturating_duration_since(stamp.callback);
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
    fn every_cue_has_the_notes_and_lengths_of_the_python_client() {
        // (cue, [(Hz, seconds)]) from cues.py, with the samples each note takes at 24 kHz.
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
    fn input_is_off_for_the_whole_cue_and_back_on_after() {
        let gate = InputGate::default();
        let engine = CueEngine::new(Cues::default(), gate.clone());
        let epoch_before = gate.state().1;
        let result = engine.play_gated(Cue::Wake, |_| {
            assert!(!gate.is_enabled(), "input must be off while the cue plays");
            Ok(())
        });
        assert_eq!(result, Ok(()));
        assert!(gate.is_enabled());
        assert!(gate.state().1 > epoch_before);
    }

    #[test]
    fn input_stays_off_after_a_cue_if_it_was_off_before() {
        let gate = InputGate::default();
        gate.set_enabled(false);
        let engine = CueEngine::new(Cues::default(), gate.clone());
        engine
            .play_gated(Cue::Accepted, |_| Ok(()))
            .expect("played");
        assert!(!gate.is_enabled());
    }

    #[test]
    fn input_comes_back_when_the_cue_fails() {
        let gate = InputGate::default();
        let engine = CueEngine::new(Cues::default(), gate.clone());
        let result = engine.play_gated(Cue::Error, |_| Err(CueError::NoOutputDevice));
        assert_eq!(result, Err(CueError::NoOutputDevice));
        assert!(gate.is_enabled());
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
            .play_gated(Cue::Wake, |_| {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .expect("nothing to play");
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(gate.state().1, epoch_before);
    }

    #[test]
    fn stop_ends_the_cue_that_is_playing_and_input_returns() {
        let gate = InputGate::default();
        let engine = CueEngine::new(Cues::default(), gate.clone());
        let result = engine.play_gated(Cue::Wake, |playback| {
            engine.stop();
            playback.wait(Duration::from_secs(5))
        });
        assert_eq!(result, Ok(()));
        assert!(gate.is_enabled());
    }

    #[test]
    fn playback_ends_after_the_last_sample_sounds_not_when_it_is_handed_over() {
        let playback = Playback::new();
        playback.set(Phase::Drained(Instant::now() + Duration::from_millis(60)));
        let started = Instant::now();
        assert_eq!(playback.wait(Duration::from_secs(5)), Ok(()));
        assert!(started.elapsed() >= Duration::from_millis(55));
    }

    #[test]
    fn a_device_that_stops_consuming_audio_is_reported_instead_of_hanging() {
        let playback = Playback::new();
        assert_eq!(
            playback.wait(Duration::from_millis(30)),
            Err(CueError::Stalled)
        );
    }

    #[test]
    fn an_output_error_while_playing_is_reported() {
        let playback = Playback::new();
        let error = cpal::Error::new(ErrorKind::DeviceNotAvailable);
        playback.set(Phase::Failed(error.clone()));
        assert_eq!(
            playback.wait(Duration::from_secs(5)),
            Err(CueError::Failed(error))
        );
    }
}

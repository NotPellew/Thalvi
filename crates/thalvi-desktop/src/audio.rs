//! Owns the microphone: cpal capture, 16 kHz framing and the input gate.

mod error;
mod framer;
mod gate;

use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    ErrorKind, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, StreamInstant,
    SupportedStreamConfig, SupportedStreamConfigRange,
};
use futures_util::task::AtomicWaker;
use rtrb::{Consumer, Producer, RingBuffer};

pub use error::AudioError;
pub use framer::Framer;
pub use gate::InputGate;

use crate::frame::Frame;

/// Frames queued between the capture thread and the consumer: 10 s, as the Python client did.
const QUEUE_FRAMES: usize = 125;

/// The device rate asked for first, as the Python client did.
const PREFERRED_RATE: u32 = 48_000;

/// Sample formats the capture callback converts, best first.
const FORMATS: [SampleFormat; 5] = [
    SampleFormat::F32,
    SampleFormat::I16,
    SampleFormat::I32,
    SampleFormat::U16,
    SampleFormat::F64,
];

const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// State the capture and error callbacks share with the consumer.
#[derive(Default)]
struct Shared {
    waker: AtomicWaker,
    overrun: AtomicBool,
    closed: AtomicBool,
    error: Mutex<Option<AudioError>>,
}

impl Shared {
    /// Marks the stream as gone and wakes a consumer that is waiting for a frame.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.waker.wake();
    }

    fn report(&self, error: cpal::Error) {
        if let Some(error) = AudioError::from_stream(error) {
            self.error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert(error);
            self.waker.wake();
        }
    }
}

/// A frame and the gate epoch it was captured in, so a consumer can tell stale ones apart.
struct Tagged {
    epoch: u64,
    frame: Frame,
}

/// The capture callback's state; feeding it never allocates or locks.
///
/// Dropping it, which is what closing the stream does, ends the consumer's frames.
struct Sink {
    framer: Framer,
    producer: Producer<Tagged>,
    gate: InputGate,
    seen_epoch: u64,
    /// Audio captured before this instant predates the last gate change and is dropped.
    boundary: StreamInstant,
    channels: u16,
    shared: Arc<Shared>,
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl Sink {
    /// Converts one callback's `data`, which the device captured at `capture`.
    ///
    /// When the gate changed, `callback` becomes the boundary. This chunk and every later one
    /// captured before it are dropped, because backends can deliver packets recorded during a cue
    /// well after input is back on.
    fn feed<T>(&mut self, data: &[T], callback: StreamInstant, capture: StreamInstant)
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let (enabled, epoch) = self.gate.state();
        let changed = epoch != self.seen_epoch;
        if changed {
            self.seen_epoch = epoch;
            self.boundary = callback;
        }
        if !enabled || changed || capture < self.boundary {
            self.framer.reset();
            return;
        }
        let Self {
            framer,
            producer,
            channels,
            shared,
            seen_epoch,
            ..
        } = self;
        let epoch = *seen_epoch;
        let mut emit = |frame: &Frame| {
            if producer
                .push(Tagged {
                    epoch,
                    frame: *frame,
                })
                .is_ok()
            {
                shared.waker.wake();
            } else {
                shared.overrun.store(true, Ordering::Release);
            }
        };
        for samples in data.chunks_exact(usize::from(*channels)) {
            let mono =
                samples.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / f32::from(*channels);
            framer.push(mono, &mut emit);
        }
    }
}

const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Microphone>();
    assert_send::<Frames>();
};

/// The receiving end of one open microphone stream.
pub struct Frames {
    consumer: Consumer<Tagged>,
    shared: Arc<Shared>,
    gate: InputGate,
    failure: Option<AudioError>,
}

impl Frames {
    /// Waits for the next 80 ms frame.
    ///
    /// After a failure, or once the microphone is paused, every call returns that error.
    /// Frames captured before the input gate last changed are discarded.
    ///
    /// # Errors
    ///
    /// The [`AudioError`] that ended capture.
    pub async fn next(&mut self) -> Result<Frame, AudioError> {
        poll_fn(|cx| {
            if let Some(result) = self.try_frame() {
                return Poll::Ready(result);
            }
            self.shared.waker.register(cx.waker());
            // A frame or failure may have landed before the waker was registered.
            self.try_frame().map_or(Poll::Pending, Poll::Ready)
        })
        .await
    }

    fn try_frame(&mut self) -> Option<Result<Frame, AudioError>> {
        if let Some(error) = &self.failure {
            return Some(Err(error.clone()));
        }
        let failure = self
            .shared
            .error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .or_else(|| {
                self.shared
                    .overrun
                    .load(Ordering::Acquire)
                    .then_some(AudioError::Dropped)
            })
            .or_else(|| {
                self.shared
                    .closed
                    .load(Ordering::Acquire)
                    .then_some(AudioError::Closed)
            });
        if let Some(error) = failure {
            self.failure = Some(error.clone());
            return Some(Err(error));
        }
        let (_, current) = self.gate.state();
        while let Ok(Tagged { epoch, frame }) = self.consumer.pop() {
            if epoch != current {
                continue;
            }
            return Some(Ok(frame));
        }
        None
    }
}

/// Opens and closes the default microphone.
///
/// Closing the stream is what turns the operating system's microphone indicator off.
pub struct Microphone {
    gate: InputGate,
    open: Option<(Stream, Arc<Shared>)>,
}

impl Microphone {
    /// A closed microphone whose captured audio is dropped while `gate` is off.
    #[must_use]
    pub fn new(gate: InputGate) -> Self {
        Self { gate, open: None }
    }

    /// Whether a stream is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Opens the current default microphone, closing any stream already open.
    ///
    /// Use this to resume after [`pause`](Self::pause) or to follow a new default device after an
    /// [`AudioError::DeviceLost`] or [`AudioError::DeviceChanged`]. It blocks while the device
    /// starts, so call it from a blocking context.
    ///
    /// # Errors
    ///
    /// Why the microphone could not be opened.
    pub fn open(&mut self) -> Result<Frames, AudioError> {
        self.pause();
        let device = cpal::default_host()
            .default_input_device()
            .ok_or(AudioError::NoDevice)?;
        let default = device
            .default_input_config()
            .map_err(AudioError::from_open)?;
        let ranges = device
            .supported_input_configs()
            .map(Iterator::collect)
            .unwrap_or_default();
        let chosen = pick_config(ranges, default);

        let (producer, consumer) = RingBuffer::new(QUEUE_FRAMES);
        let shared = Arc::new(Shared::default());
        let (_, epoch) = self.gate.state();
        let sink = Sink {
            framer: Framer::new(chosen.sample_rate()),
            producer,
            gate: self.gate.clone(),
            seen_epoch: epoch,
            boundary: StreamInstant::ZERO,
            channels: chosen.channels().max(1),
            shared: Arc::clone(&shared),
        };
        let stream = build_stream(&device, &chosen, sink).map_err(AudioError::from_open)?;
        stream.play().map_err(AudioError::from_open)?;
        self.open = Some((stream, Arc::clone(&shared)));
        Ok(Frames {
            consumer,
            shared,
            gate: self.gate.clone(),
            failure: None,
        })
    }

    /// Closes the stream so the operating system shows the microphone as off.
    ///
    /// The [`Frames`] from the stream then report [`AudioError::Closed`].
    pub fn pause(&mut self) {
        if let Some((stream, shared)) = self.open.take() {
            drop(stream);
            shared.close();
        }
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.pause();
    }
}

fn build_stream(
    device: &cpal::Device,
    chosen: &SupportedStreamConfig,
    sink: Sink,
) -> Result<Stream, cpal::Error> {
    let config: StreamConfig = chosen.config();
    let shared = Arc::clone(&sink.shared);
    let on_error = move |error| shared.report(error);
    match chosen.sample_format() {
        SampleFormat::F32 => build::<f32>(device, config, sink, on_error),
        SampleFormat::I16 => build::<i16>(device, config, sink, on_error),
        SampleFormat::I32 => build::<i32>(device, config, sink, on_error),
        SampleFormat::U16 => build::<u16>(device, config, sink, on_error),
        SampleFormat::F64 => build::<f64>(device, config, sink, on_error),
        other => Err(cpal::Error::with_message(
            ErrorKind::UnsupportedConfig,
            format!("microphone sample format {other} is not supported"),
        )),
    }
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut sink: Sink,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream(
        config,
        move |data: &[T], info| {
            let stamp = info.timestamp();
            sink.feed(data, stamp.callback, stamp.capture);
        },
        on_error,
        Some(OPEN_TIMEOUT),
    )
}

/// Picks the fewest channels at 48 kHz in a format we convert, else the device default.
fn pick_config(
    ranges: Vec<SupportedStreamConfigRange>,
    default: SupportedStreamConfig,
) -> SupportedStreamConfig {
    ranges
        .into_iter()
        .filter_map(|range| range.try_with_sample_rate(PREFERRED_RATE))
        .filter_map(|config| {
            let rank = FORMATS.iter().position(|f| *f == config.sample_format())?;
            Some((config.channels(), rank, config))
        })
        .min_by_key(|(channels, rank, _)| (*channels, *rank))
        .map_or(default, |(_, _, config)| config)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use cpal::SupportedBufferSize;

    use super::*;
    use crate::frame::FRAME_SAMPLES;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// A capture pipeline without a device: feed the sink as a callback would.
    fn pipeline(rate: u32, channels: u16, gate: &InputGate) -> (Sink, Frames) {
        let (producer, consumer) = RingBuffer::new(QUEUE_FRAMES);
        let shared = Arc::new(Shared::default());
        let (_, epoch) = gate.state();
        let sink = Sink {
            framer: Framer::new(rate),
            producer,
            gate: gate.clone(),
            seen_epoch: epoch,
            boundary: StreamInstant::ZERO,
            channels,
            shared: Arc::clone(&shared),
        };
        let frames = Frames {
            consumer,
            shared,
            gate: gate.clone(),
            failure: None,
        };
        (sink, frames)
    }

    /// 80 ms of 16-bit audio at 16 kHz holding `value` everywhere.
    fn block(value: i16) -> Vec<i16> {
        vec![value; FRAME_SAMPLES]
    }

    /// Feeds a chunk that was captured at the moment it was delivered.
    fn live<T>(sink: &mut Sink, data: &[T])
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        sink.feed(data, StreamInstant::ZERO, StreamInstant::ZERO);
    }

    async fn next(frames: &mut Frames) -> Result<Frame, AudioError> {
        tokio::time::timeout(TEST_TIMEOUT, frames.next())
            .await
            .expect("a frame or an error should arrive")
    }

    #[tokio::test]
    async fn device_samples_arrive_as_16khz_mono_frames() {
        // Stereo at 48 kHz: the channels are averaged, then every three samples.
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(48_000, 2, &gate);
        let stereo: Vec<i16> = (0..FRAME_SAMPLES * 3)
            .flat_map(|_| [3_000, 1_000])
            .collect();
        live(&mut sink, &stereo);
        let frame = next(&mut frames).await.expect("frame");
        assert!(frame.iter().all(|&s| s == 2_000));
    }

    #[tokio::test]
    async fn a_waiting_consumer_wakes_when_the_capture_thread_delivers() {
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(16_000, 1, &gate);
        let waiting = tokio::spawn(async move { next(&mut frames).await.map(|f| f[0]) });
        tokio::task::yield_now().await;
        let capture = std::thread::spawn(move || {
            live(&mut sink, &block(7));
            sink
        });
        assert_eq!(waiting.await.expect("task"), Ok(7));
        capture.join().expect("capture thread");
    }

    #[tokio::test]
    async fn frames_are_dropped_while_the_gate_is_off_and_the_first_chunk_after_it() {
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(16_000, 1, &gate);
        live(&mut sink, &block(1));
        next(&mut frames).await.expect("frame before the cue");

        gate.set_enabled(false);
        live(&mut sink, &block(2));
        live(&mut sink, &block(3));
        gate.set_enabled(true);
        live(&mut sink, &block(4)); // in flight when the cue ended
        live(&mut sink, &block(5));
        live(&mut sink, &block(6));

        assert_eq!(next(&mut frames).await.expect("frame")[0], 5);
        assert_eq!(next(&mut frames).await.expect("frame")[0], 6);
    }

    #[tokio::test]
    async fn packets_recorded_during_a_cue_stay_out_even_when_delivered_after_it() {
        let at = StreamInstant::from_millis;
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(16_000, 1, &gate);
        gate.set_enabled(false);
        sink.feed(&block(1), at(100), at(90)); // during the cue
        gate.set_enabled(true);
        sink.feed(&block(2), at(500), at(450)); // first callback after the cue
        sink.feed(&block(3), at(501), at(300)); // backlog from the cue, delivered late
        sink.feed(&block(4), at(502), at(420)); // backlog, captured just before the boundary
        sink.feed(&block(5), at(503), at(500)); // fresh, captured exactly at the boundary
        sink.feed(&block(6), at(583), at(583));

        assert_eq!(next(&mut frames).await.expect("frame")[0], 5);
        assert_eq!(next(&mut frames).await.expect("frame")[0], 6);
    }

    #[tokio::test]
    async fn queued_frames_and_a_partial_frame_do_not_survive_a_gate_change() {
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(16_000, 1, &gate);
        live(&mut sink, &block(1));
        live(&mut sink, &block(2)[..FRAME_SAMPLES / 2]);
        gate.set_enabled(false);
        gate.set_enabled(true);
        live(&mut sink, &block(3)); // dropped as the chunk in flight
        live(&mut sink, &block(4));
        assert_eq!(next(&mut frames).await.expect("frame")[0], 4);
    }

    #[tokio::test]
    async fn a_lost_device_ends_capture_even_with_frames_queued() {
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(16_000, 1, &gate);
        live(&mut sink, &block(1));
        sink.shared.report(ErrorKind::DeviceNotAvailable.into());
        assert_eq!(next(&mut frames).await, Err(AudioError::DeviceLost));
        assert_eq!(next(&mut frames).await, Err(AudioError::DeviceLost));
    }

    #[tokio::test]
    async fn a_harmless_stream_error_leaves_capture_running() {
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(16_000, 1, &gate);
        sink.shared.report(ErrorKind::RealtimeDenied.into());
        live(&mut sink, &block(1));
        assert_eq!(next(&mut frames).await.expect("frame")[0], 1);
    }

    #[tokio::test]
    async fn a_consumer_that_falls_ten_seconds_behind_gets_a_dropped_audio_error() {
        let gate = InputGate::default();
        let (mut sink, mut frames) = pipeline(16_000, 1, &gate);
        for _ in 0..=QUEUE_FRAMES {
            live(&mut sink, &block(1));
        }
        assert_eq!(next(&mut frames).await, Err(AudioError::Dropped));
    }

    #[tokio::test]
    async fn closing_the_stream_ends_the_frames() {
        let gate = InputGate::default();
        let (sink, mut frames) = pipeline(16_000, 1, &gate);
        let waiting = tokio::spawn(async move { next(&mut frames).await });
        tokio::task::yield_now().await;
        drop(sink);
        assert_eq!(waiting.await.expect("task"), Err(AudioError::Closed));
    }

    #[test]
    fn config_prefers_fewest_channels_at_48khz_in_a_known_format() {
        let range = |channels, min, max, format| {
            SupportedStreamConfigRange::new(
                channels,
                min,
                max,
                SupportedBufferSize::Unknown,
                format,
            )
        };
        let default =
            SupportedStreamConfig::new(2, 44_100, SupportedBufferSize::Unknown, SampleFormat::F32);
        let pick = |ranges: Vec<SupportedStreamConfigRange>| {
            let config = pick_config(ranges, default);
            (
                config.channels(),
                config.sample_rate(),
                config.sample_format(),
            )
        };
        let cases = [
            (
                vec![
                    range(2, 8_000, 96_000, SampleFormat::F32),
                    range(1, 8_000, 96_000, SampleFormat::I16),
                    range(1, 8_000, 96_000, SampleFormat::I24),
                ],
                (1, 48_000, SampleFormat::I16),
            ),
            (
                vec![
                    range(1, 8_000, 96_000, SampleFormat::I16),
                    range(1, 8_000, 96_000, SampleFormat::F32),
                ],
                (1, 48_000, SampleFormat::F32),
            ),
            // No range reaches 48 kHz: keep what the device calls its default.
            (
                vec![range(1, 8_000, 44_100, SampleFormat::I16)],
                (2, 44_100, SampleFormat::F32),
            ),
        ];
        for (ranges, expected) in cases {
            assert_eq!(pick(ranges), expected);
        }
    }

    /// Opens the default microphone for 4 s and checks 16 kHz frames arrive at the real rate,
    /// then that dropping the microphone ends a reader that is waiting for frames.
    #[tokio::test]
    #[ignore = "needs a microphone"]
    async fn default_microphone_delivers_16khz_frames_in_real_time() {
        let gate = InputGate::default();
        let mut microphone = Microphone::new(gate.clone());
        let mut frames = microphone.open().expect("a default microphone");
        let started = Instant::now();
        let mut count = 0_u32;
        while started.elapsed() < Duration::from_secs(4) {
            let frame = next(&mut frames).await.expect("frames keep arriving");
            assert_eq!(frame.len(), FRAME_SAMPLES);
            count += 1;
        }
        let expected = started.elapsed().as_secs_f64() / 0.08;
        assert!(
            (f64::from(count) - expected).abs() <= 3.0,
            "{count} frames in {:?}, expected about {expected:.1}",
            started.elapsed()
        );
        gate.set_enabled(false);
        gate.set_enabled(true);
        next(&mut frames)
            .await
            .expect("real capture timestamps let frames through after the gate reopens");
        microphone.pause();
        assert!(!microphone.is_open());
        assert_eq!(next(&mut frames).await, Err(AudioError::Closed));

        let mut frames = microphone.open().expect("the microphone reopens");
        let reader = tokio::spawn(async move {
            loop {
                if let Err(error) = next(&mut frames).await {
                    return error;
                }
            }
        });
        tokio::task::yield_now().await;
        drop(microphone);
        assert_eq!(reader.await.expect("task"), AudioError::Closed);
    }
}

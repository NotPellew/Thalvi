//! Owns the log-mel features of the local models: incremental ones matching sherpa-onnx, and whole-buffer ones matching `NeMo`.
//!
//! [`LogMel`] matches kaldi-native-fbank as sherpa-onnx configures it for `NeMo` transducers: 25 ms
//! Hann windows every 10 ms centred on their frame (reflected at the start), 0.97 pre-emphasis, a
//! 512-point power spectrum and 128 Slaney mel bins up to 8 kHz. [`KaldiFbank`] is the profile of
//! the keyword spotter: it removes each frame's DC offset, uses a Povey window and 80 HTK mel bins
//! from 20 Hz to 7.6 kHz. [`WindowMel`] matches `NeMo`'s own preprocessor, which the buffered model
//! was trained with.

use std::f32::consts::PI;
use std::sync::Arc;

/// Mel bins per feature frame.
pub const MEL_BINS: usize = 128;

const WINDOW: usize = 400;
const SHIFT: usize = 160;
const FFT_SIZE: usize = 512;
const SPECTRUM: usize = FFT_SIZE / 2 + 1;
const PRE_EMPHASIS: f32 = 0.97;
const MAX_HZ: f32 = 8_000.0;
const SAMPLE_RATE: f32 = 16_000.0;
/// Added before the logarithm by `NeMo`'s preprocessor, so digital silence has a finite feature.
const NEMO_LOG_GUARD: f32 = 5.960_464_5e-8;
/// Added to the standard deviation when `NeMo` normalizes each mel bin.
const NEMO_STD_GUARD: f32 = 1e-5;

/// Samples between the starts of two feature frames.
pub const HOP: usize = SHIFT;

/// One frame of log-mel features.
pub type MelFrame = [f32; MEL_BINS];

fn slaney_mel(hz: f32) -> f32 {
    if hz <= 1_000.0 {
        hz * 3.0 / 200.0
    } else {
        15.0 + 14.545_078 * (hz / 1_000.0).ln()
    }
}

fn slaney_hz(mel: f32) -> f32 {
    if mel <= 15.0 {
        200.0 / 3.0 * mel
    } else {
        1_000.0 * ((mel - 15.0) * 0.068_751_78).exp()
    }
}

/// A triangular mel filter: the first spectrum bin it covers and its weights.
struct Filter {
    first: usize,
    weights: Vec<f32>,
}

#[expect(
    clippy::cast_precision_loss,
    reason = "bin and filter indices are far below 2^24"
)]
fn filters() -> Vec<Filter> {
    let bin_width = SAMPLE_RATE / FFT_SIZE as f32;
    let (low, high) = (slaney_mel(0.0), slaney_mel(MAX_HZ));
    let delta = (high - low) / (MEL_BINS + 1) as f32;
    (0..MEL_BINS)
        .map(|bin| {
            let edge = |step: usize| slaney_hz(low + (bin + step) as f32 * delta);
            let (left, center, right) = (edge(0), edge(1), edge(2));
            let weights: Vec<(usize, f32)> = (0..SPECTRUM)
                .filter_map(|index| {
                    let hz = bin_width * index as f32;
                    (hz > left && hz < right).then(|| {
                        let slope = if hz <= center {
                            (hz - left) / (center - left)
                        } else {
                            (right - hz) / (right - center)
                        };
                        (index, slope * 2.0 / (right - left))
                    })
                })
                .collect();
            Filter {
                first: weights.first().map_or(0, |(index, _)| *index),
                weights: weights.into_iter().map(|(_, weight)| weight).collect(),
            }
        })
        .collect()
}

/// In-place radix-2 FFT of `FFT_SIZE` complex samples.
struct Fft {
    twiddles: Vec<(f32, f32)>,
}

impl Fft {
    #[expect(clippy::cast_precision_loss, reason = "indices are far below 2^24")]
    fn new() -> Self {
        let twiddles = (0..FFT_SIZE / 2)
            .map(|k| {
                let angle = -2.0 * PI * k as f32 / FFT_SIZE as f32;
                (angle.cos(), angle.sin())
            })
            .collect();
        Self { twiddles }
    }

    fn run(&self, re: &mut [f32; FFT_SIZE], im: &mut [f32; FFT_SIZE]) {
        let bits = FFT_SIZE.trailing_zeros();
        for i in 0..FFT_SIZE {
            let j = i.reverse_bits() >> (usize::BITS - bits);
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut half = 1;
        while half < FFT_SIZE {
            let stride = FFT_SIZE / (2 * half);
            for start in (0..FFT_SIZE).step_by(2 * half) {
                for k in 0..half {
                    let (cos, sin) = self.twiddles[k * stride];
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * cos - im[b] * sin;
                    let ti = re[b] * sin + im[b] * cos;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            half *= 2;
        }
    }
}

/// What every stream computes with, built once.
struct Tables {
    /// Hann window with period `WINDOW`, as kaldi has it.
    periodic: [f32; WINDOW],
    /// Hann window from 0 to 0 over `WINDOW` samples, as `NeMo` has it.
    symmetric: [f32; WINDOW],
    filters: Vec<Filter>,
    fft: Fft,
}

impl Tables {
    #[expect(clippy::cast_precision_loss, reason = "indices are far below 2^24")]
    fn new() -> Arc<Self> {
        let hann = |period: usize| {
            std::array::from_fn(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / period as f32).cos())
        };
        Arc::new(Self {
            periodic: hann(WINDOW),
            symmetric: hann(WINDOW - 1),
            filters: filters(),
            fft: Fft::new(),
        })
    }

    /// The mel energies of a pre-emphasized `wave` under `window`, through `log`.
    fn mel(
        &self,
        wave: &[f32; WINDOW],
        window: &[f32; WINDOW],
        log: impl Fn(f32) -> f32,
    ) -> MelFrame {
        let mut re = [0.0; FFT_SIZE];
        let mut im = [0.0; FFT_SIZE];
        for (i, (value, weight)) in wave.iter().zip(window).enumerate() {
            re[i] = value * weight;
        }
        self.fft.run(&mut re, &mut im);
        let power: [f32; SPECTRUM] = std::array::from_fn(|i| re[i] * re[i] + im[i] * im[i]);
        std::array::from_fn(|bin| {
            let filter = &self.filters[bin];
            let energy: f32 = filter
                .weights
                .iter()
                .zip(&power[filter.first..])
                .map(|(weight, power)| weight * power)
                .sum();
            log(energy)
        })
    }
}

/// The samples of a stream that later frames still need.
#[derive(Clone, Default)]
struct Samples {
    /// Samples not yet dropped; the first is sample number `offset` of the stream.
    kept: Vec<f32>,
    offset: usize,
}

impl Samples {
    fn total(&self) -> usize {
        self.offset + self.kept.len()
    }

    /// Sample `index`, reflected around the first sample as kaldi does before the stream starts.
    fn at(&self, index: isize) -> f32 {
        let mut index = index;
        while index < 0 {
            index = -index - 1;
        }
        let index = index.unsigned_abs().min(self.total() - 1);
        self.kept[index - self.offset]
    }

    fn drop_before(&mut self, keep_from: usize) {
        if keep_from > self.offset {
            self.kept.drain(..keep_from - self.offset);
            self.offset = keep_from;
        }
    }
}

/// Turns a stream of samples into log-mel frames as soon as each frame's window is complete.
#[derive(Clone)]
pub struct LogMel {
    tables: Arc<Tables>,
    samples: Samples,
    frames: usize,
}

impl Default for LogMel {
    fn default() -> Self {
        Self::new()
    }
}

impl LogMel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tables: Tables::new(),
            samples: Samples::default(),
            frames: 0,
        }
    }

    /// Adds 16 kHz samples scaled to -1..1 and appends every frame they complete to `out`.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<MelFrame>) {
        self.samples.kept.extend_from_slice(samples);
        let total = self.samples.total();
        // Frame `n` is centred on sample `n * SHIFT + SHIFT / 2`.
        while (self.frames * SHIFT + SHIFT / 2 + WINDOW / 2) <= total {
            out.push(self.frame(self.frames));
            self.frames += 1;
        }
        self.samples
            .drop_before((self.frames * SHIFT + SHIFT / 2).saturating_sub(WINDOW / 2));
    }

    #[expect(
        clippy::cast_possible_wrap,
        reason = "stream positions stay far below isize::MAX"
    )]
    fn frame(&self, number: usize) -> MelFrame {
        let start = (number * SHIFT + SHIFT / 2) as isize - (WINDOW / 2) as isize;
        let mut wave: [f32; WINDOW] = std::array::from_fn(|i| self.samples.at(start + i as isize));
        for i in (1..WINDOW).rev() {
            wave[i] -= PRE_EMPHASIS * wave[i - 1];
        }
        wave[0] -= PRE_EMPHASIS * wave[0];
        self.tables.mel(&wave, &self.tables.periodic, |energy| {
            energy.max(f32::EPSILON).ln()
        })
    }
}

/// Mel bins per frame of the Kaldi profile.
pub const KALDI_BINS: usize = 80;

const KALDI_LOW_HZ: f32 = 20.0;
const KALDI_HIGH_HZ: f32 = 7_600.0;

/// One frame of the Kaldi profile's log-mel features.
pub type KaldiFrame = [f32; KALDI_BINS];

fn htk_mel(hz: f32) -> f32 {
    1_127.0 * (1.0 + hz / 700.0).ln()
}

/// Triangular filters of the Kaldi profile, weighted in the mel domain over the 256 bins below Nyquist.
#[expect(
    clippy::cast_precision_loss,
    reason = "bin and filter indices are far below 2^24"
)]
fn kaldi_filters() -> Vec<Filter> {
    let bin_width = SAMPLE_RATE / FFT_SIZE as f32;
    let (low, high) = (htk_mel(KALDI_LOW_HZ), htk_mel(KALDI_HIGH_HZ));
    let delta = (high - low) / (KALDI_BINS + 1) as f32;
    (0..KALDI_BINS)
        .map(|bin| {
            let edge = |step: usize| low + (bin + step) as f32 * delta;
            let (left, center, right) = (edge(0), edge(1), edge(2));
            let weights: Vec<(usize, f32)> = (0..FFT_SIZE / 2)
                .filter_map(|index| {
                    let mel = htk_mel(bin_width * index as f32);
                    (mel > left && mel < right).then(|| {
                        let weight = if mel <= center {
                            (mel - left) / (center - left)
                        } else {
                            (right - mel) / (right - center)
                        };
                        (index, weight)
                    })
                })
                .collect();
            Filter {
                first: weights.first().map_or(0, |(index, _)| *index),
                weights: weights.into_iter().map(|(_, weight)| weight).collect(),
            }
        })
        .collect()
}

struct KaldiTables {
    window: [f32; WINDOW],
    filters: Vec<Filter>,
    fft: Fft,
}

/// Like [`LogMel`] with the Kaldi profile the keyword spotter was trained on.
#[derive(Clone)]
pub struct KaldiFbank {
    tables: Arc<KaldiTables>,
    samples: Samples,
    frames: usize,
}

impl Default for KaldiFbank {
    fn default() -> Self {
        Self::new()
    }
}

impl KaldiFbank {
    #[must_use]
    #[expect(clippy::cast_precision_loss, reason = "indices are far below 2^24")]
    pub fn new() -> Self {
        let window = std::array::from_fn(|i| {
            (0.5 - 0.5 * (2.0 * PI * i as f32 / (WINDOW - 1) as f32).cos()).powf(0.85)
        });
        Self {
            tables: Arc::new(KaldiTables {
                window,
                filters: kaldi_filters(),
                fft: Fft::new(),
            }),
            samples: Samples::default(),
            frames: 0,
        }
    }

    /// Adds 16 kHz samples scaled to -1..1 and appends every frame they complete to `out`.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<KaldiFrame>) {
        self.samples.kept.extend_from_slice(samples);
        let total = self.samples.total();
        while (self.frames * SHIFT + SHIFT / 2 + WINDOW / 2) <= total {
            out.push(self.frame(self.frames));
            self.frames += 1;
        }
        self.samples
            .drop_before((self.frames * SHIFT + SHIFT / 2).saturating_sub(WINDOW / 2));
    }

    #[expect(
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        reason = "stream positions stay far below isize::MAX and window sizes below 2^24"
    )]
    fn frame(&self, number: usize) -> KaldiFrame {
        let start = (number * SHIFT + SHIFT / 2) as isize - (WINDOW / 2) as isize;
        let mut wave: [f32; WINDOW] = std::array::from_fn(|i| self.samples.at(start + i as isize));
        let mean = wave.iter().sum::<f32>() / WINDOW as f32;
        for value in &mut wave {
            *value -= mean;
        }
        for i in (1..WINDOW).rev() {
            wave[i] -= PRE_EMPHASIS * wave[i - 1];
        }
        wave[0] -= PRE_EMPHASIS * wave[0];
        let mut re = [0.0; FFT_SIZE];
        let mut im = [0.0; FFT_SIZE];
        for (i, (value, weight)) in wave.iter().zip(&self.tables.window).enumerate() {
            re[i] = value * weight;
        }
        self.tables.fft.run(&mut re, &mut im);
        let power: [f32; FFT_SIZE / 2] = std::array::from_fn(|i| re[i] * re[i] + im[i] * im[i]);
        std::array::from_fn(|bin| {
            let filter = &self.tables.filters[bin];
            let energy: f32 = filter
                .weights
                .iter()
                .zip(&power[filter.first..])
                .map(|(weight, power)| weight * power)
                .sum();
            energy.max(f32::EPSILON).ln()
        })
    }
}

/// Features of a whole buffer of speech, recomputed from its first sample each time.
#[derive(Clone)]
pub struct WindowMel {
    tables: Arc<Tables>,
}

impl Default for WindowMel {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowMel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tables: Tables::new(),
        }
    }

    /// The features `NeMo`'s preprocessor makes of `speech` followed by silence: one frame per
    /// [`HOP`] samples, each mel bin normalized over those frames alone.
    #[must_use]
    #[expect(
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        reason = "positions and counts stay far below 2^24"
    )]
    pub fn features(&self, speech: &[f32]) -> Vec<MelFrame> {
        let emphasized: Vec<f32> = speech
            .iter()
            .enumerate()
            .map(|(at, value)| {
                value
                    - if at == 0 {
                        0.0
                    } else {
                        speech[at - 1] * PRE_EMPHASIS
                    }
            })
            .collect();
        let at = |index: isize| -> f32 {
            usize::try_from(index)
                .ok()
                .and_then(|index| emphasized.get(index))
                .copied()
                .unwrap_or(0.0)
        };
        let count = speech.len() / HOP;
        let mut frames: Vec<MelFrame> = (0..count)
            .map(|number| {
                let start = (number * HOP) as isize - (WINDOW / 2) as isize;
                let wave: [f32; WINDOW] = std::array::from_fn(|i| at(start + i as isize));
                self.tables.mel(&wave, &self.tables.symmetric, |energy| {
                    (energy + NEMO_LOG_GUARD).ln()
                })
            })
            .collect();
        if frames.is_empty() {
            return frames;
        }
        for bin in 0..MEL_BINS {
            // Centred on the first frame so that a constant bin has exactly no deviation.
            let reference = frames[0][bin];
            let mean = reference
                + frames
                    .iter()
                    .map(|frame| frame[bin] - reference)
                    .sum::<f32>()
                    / count as f32;
            let variance = if count > 1 {
                frames
                    .iter()
                    .map(|frame| (frame[bin] - mean).powi(2))
                    .sum::<f32>()
                    / (count - 1) as f32
            } else {
                0.0
            };
            let deviation = variance.sqrt() + NEMO_STD_GUARD;
            for frame in &mut frames {
                frame[bin] = (frame[bin] - mean) / deviation;
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deterministic noise `fbank_ref.py` also generates, scaled like 16-bit PCM.
    fn noise(len: usize) -> Vec<f32> {
        let mut state = 1_u32;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let [high, low, ..] = state.to_be_bytes();
                (f32::from(u16::from_be_bytes([high, low]) % 2000) - 1000.0) / 32768.0
            })
            .collect()
    }

    fn features(chunks: &[usize]) -> Vec<MelFrame> {
        let samples = noise(4000);
        let mut mel = LogMel::new();
        let mut out = Vec::new();
        let mut at = 0;
        for size in chunks {
            mel.push(&samples[at..at + size], &mut out);
            at += size;
        }
        out
    }

    #[test]
    fn frames_match_kaldi_native_fbank_however_the_samples_arrive() {
        // Expected values come from kaldi-native-fbank's OnlineFbank with sherpa-onnx's NeMo options.
        let expected: [(usize, [f32; 6], f32, f32); 3] = [
            (
                0,
                [-14.7502, -14.1768, -13.792, -12.9314, -15.7662, -13.4385],
                -1058.12,
                -6.2776,
            ),
            (
                1,
                [-14.2031, -14.7839, -15.9424, -13.3898, -13.4911, -13.4919],
                -1047.203,
                -4.9056,
            ),
            (
                5,
                [-13.5087, -12.7021, -12.2374, -11.5336, -12.5787, -11.6715],
                -1018.261,
                -5.5949,
            ),
        ];
        for chunks in [vec![4000], vec![1280, 1280, 1280, 160]] {
            let frames = features(&chunks);
            assert_eq!(frames.len(), 24);
            for (number, head, sum, last) in expected {
                let frame = &frames[number];
                for (got, want) in frame.iter().zip(head) {
                    assert!((got - want).abs() < 2e-3, "frame {number}: {got} vs {want}");
                }
                assert!(
                    (frame.iter().sum::<f32>() - sum).abs() < 0.2,
                    "frame {number} sum"
                );
                assert!(
                    (frame[MEL_BINS - 1] - last).abs() < 2e-3,
                    "frame {number} last bin"
                );
            }
        }
    }

    #[test]
    fn whole_buffer_features_match_nemo_and_are_normalized_over_the_speech_alone() {
        // Expected values come from `NeMo`'s FilterbankFeatures with the unified model's
        // preprocessor settings, given this noise followed by silence in a longer buffer.
        let expected: [(usize, [f32; 6], f32, f32); 4] = [
            (
                0,
                [0.123, -0.1164, -0.0739, -0.5342, -0.8301, -1.6341],
                -109.075,
                -2.1645,
            ),
            (
                1,
                [-0.3904, -0.7264, -0.5874, -0.2659, -0.2386, -0.3639],
                -6.863,
                -0.9049,
            ),
            (
                10,
                [-0.0101, -0.8095, -1.4228, -0.4722, 0.259, 0.5591],
                -24.866,
                -0.9676,
            ),
            (
                24,
                [1.3464, 1.2346, 0.9802, 0.2305, 0.4131, 0.8246],
                30.787,
                0.2888,
            ),
        ];
        let frames = WindowMel::new().features(&noise(4000));
        assert_eq!(frames.len(), 25);
        for (number, head, sum, last) in expected {
            let frame = &frames[number];
            for (got, want) in frame.iter().zip(head) {
                assert!((got - want).abs() < 2e-3, "frame {number}: {got} vs {want}");
            }
            assert!(
                (frame.iter().sum::<f32>() - sum).abs() < 0.2,
                "frame {number} sum"
            );
            assert!(
                (frame[MEL_BINS - 1] - last).abs() < 2e-3,
                "frame {number} last bin"
            );
        }
        for bin in 0..MEL_BINS {
            let mean = frames.iter().map(|frame| frame[bin]).sum::<f32>() / 25.0;
            let spread = frames
                .iter()
                .map(|frame| (frame[bin] - mean).powi(2))
                .sum::<f32>()
                / 24.0;
            assert!(
                mean.abs() < 1e-3 && (spread - 1.0).abs() < 1e-2,
                "bin {bin}"
            );
        }
    }

    #[test]
    fn digital_silence_has_no_features_to_amplify() {
        let frames = WindowMel::new().features(&[0.0; 3200]);
        assert_eq!(frames.len(), 20);
        assert!(frames.iter().flatten().all(|value| *value == 0.0));
    }

    #[test]
    fn kaldi_frames_match_kaldi_native_fbank_however_the_samples_arrive() {
        // Expected values come from kaldi-native-fbank's OnlineFbank with sherpa-onnx's keyword options.
        let expected: [(usize, [f32; 6], f32, f32); 3] = [
            (
                0,
                [-11.1924, -9.8937, -9.9471, -11.0002, -10.2933, -9.2331],
                -312.9,
                0.2234,
            ),
            (
                1,
                [-12.711, -12.8061, -10.4374, -10.1782, -10.1082, -9.4308],
                -306.098,
                0.675,
            ),
            (
                5,
                [-10.2306, -8.7779, -8.5133, -9.028, -8.5031, -7.6634],
                -286.117,
                -0.1698,
            ),
        ];
        let samples = noise(4000);
        for chunks in [vec![4000], vec![1280, 1280, 1280, 160]] {
            let mut fbank = KaldiFbank::new();
            let mut frames = Vec::new();
            let mut at = 0;
            for size in chunks {
                fbank.push(&samples[at..at + size], &mut frames);
                at += size;
            }
            assert_eq!(frames.len(), 24);
            for (number, head, sum, last) in expected {
                let frame = &frames[number];
                for (got, want) in frame.iter().zip(head) {
                    assert!((got - want).abs() < 2e-3, "frame {number}: {got} vs {want}");
                }
                assert!(
                    (frame.iter().sum::<f32>() - sum).abs() < 0.2,
                    "frame {number} sum"
                );
                assert!(
                    (frame[KALDI_BINS - 1] - last).abs() < 2e-3,
                    "frame {number} last bin"
                );
            }
        }
    }
}

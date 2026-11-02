//! Owns the NeMo-style log-mel features of the local speech model, computed incrementally from 16 kHz samples.
//!
//! Matches kaldi-native-fbank as sherpa-onnx configures it for NeMo transducers: 25 ms Hann
//! windows every 10 ms centred on their frame (reflected at the start), 0.97 pre-emphasis, a
//! 512-point power spectrum and 128 Slaney mel bins up to 8 kHz.

use std::f32::consts::PI;

/// Mel bins per feature frame.
pub const MEL_BINS: usize = 128;

const WINDOW: usize = 400;
const SHIFT: usize = 160;
const FFT_SIZE: usize = 512;
const SPECTRUM: usize = FFT_SIZE / 2 + 1;
const PRE_EMPHASIS: f32 = 0.97;
const MAX_HZ: f32 = 8_000.0;
const SAMPLE_RATE: f32 = 16_000.0;

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

/// Turns a stream of samples into log-mel frames as soon as each frame's window is complete.
pub struct LogMel {
    window: [f32; WINDOW],
    filters: Vec<Filter>,
    fft: Fft,
    /// Samples not yet dropped; the first is sample number `offset` of the stream.
    samples: Vec<f32>,
    offset: usize,
    frames: usize,
}

impl Default for LogMel {
    fn default() -> Self {
        Self::new()
    }
}

impl LogMel {
    #[must_use]
    #[expect(clippy::cast_precision_loss, reason = "indices are far below 2^24")]
    pub fn new() -> Self {
        let window =
            std::array::from_fn(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / WINDOW as f32).cos());
        Self {
            window,
            filters: filters(),
            fft: Fft::new(),
            samples: Vec::new(),
            offset: 0,
            frames: 0,
        }
    }

    /// Adds 16 kHz samples scaled to -1..1 and appends every frame they complete to `out`.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<MelFrame>) {
        self.samples.extend_from_slice(samples);
        let total = self.offset + self.samples.len();
        // Frame `n` is centred on sample `n * SHIFT + SHIFT / 2`.
        while (self.frames * SHIFT + SHIFT / 2 + WINDOW / 2) <= total {
            out.push(self.frame(self.frames));
            self.frames += 1;
        }
        let keep_from = (self.frames * SHIFT + SHIFT / 2).saturating_sub(WINDOW / 2);
        if keep_from > self.offset {
            self.samples.drain(..keep_from - self.offset);
            self.offset = keep_from;
        }
    }

    fn sample(&self, index: isize) -> f32 {
        let total = self.offset + self.samples.len();
        let mut index = index;
        // Reflect around the first sample, as kaldi does for the first frame.
        while index < 0 {
            index = -index - 1;
        }
        let index = index.unsigned_abs().min(total - 1);
        self.samples[index - self.offset]
    }

    #[expect(
        clippy::cast_possible_wrap,
        reason = "stream positions stay far below isize::MAX"
    )]
    fn frame(&self, number: usize) -> MelFrame {
        let start = (number * SHIFT + SHIFT / 2) as isize - (WINDOW / 2) as isize;
        let mut wave: [f32; WINDOW] = std::array::from_fn(|i| self.sample(start + i as isize));
        for i in (1..WINDOW).rev() {
            wave[i] -= PRE_EMPHASIS * wave[i - 1];
        }
        wave[0] -= PRE_EMPHASIS * wave[0];
        let mut re = [0.0; FFT_SIZE];
        let mut im = [0.0; FFT_SIZE];
        for (i, (value, weight)) in wave.iter().zip(&self.window).enumerate() {
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
            energy.max(f32::EPSILON).ln()
        })
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
}

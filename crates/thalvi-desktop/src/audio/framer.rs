//! Turns device samples into 16 kHz mono frames without allocating.

use crate::frame::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

/// Averages device samples down to 16 kHz and cuts them into [`Frame`]s.
///
/// Each output sample is the mean of the input span it covers, so 48 kHz is a mean of three
/// samples like the Python client, and rates that are not a multiple of 16 kHz still work.
pub struct Framer {
    in_rate: u32,
    sum: f64,
    filled: u32,
    frame: Frame,
    len: usize,
}

impl Framer {
    /// A framer for a device running at `in_rate` Hz.
    #[must_use]
    pub fn new(in_rate: u32) -> Self {
        Self {
            in_rate: in_rate.max(1),
            sum: 0.0,
            filled: 0,
            frame: [0; FRAME_SAMPLES],
            len: 0,
        }
    }

    /// Feeds one mono sample in `-1.0..=1.0`, calling `emit` for each frame it completes.
    pub fn push(&mut self, sample: f32, emit: &mut impl FnMut(&Frame)) {
        let mut remaining = SAMPLE_RATE;
        loop {
            let needed = self.in_rate - self.filled;
            if remaining < needed {
                self.sum += f64::from(sample) * f64::from(remaining);
                self.filled += remaining;
                return;
            }
            self.sum += f64::from(sample) * f64::from(needed);
            remaining -= needed;
            let mean = self.sum / f64::from(self.in_rate);
            self.sum = 0.0;
            self.filled = 0;
            self.frame[self.len] = to_i16(mean);
            self.len += 1;
            if self.len == FRAME_SAMPLES {
                self.len = 0;
                emit(&self.frame);
            }
            if remaining == 0 {
                return;
            }
        }
    }

    /// Drops the partly built frame and the partly averaged sample.
    pub fn reset(&mut self) {
        self.sum = 0.0;
        self.filled = 0;
        self.len = 0;
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is clamped to the i16 range first"
)]
fn to_i16(sample: f64) -> i16 {
    (sample * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(in_rate: u32, input: &[f32]) -> Vec<i16> {
        let mut framer = Framer::new(in_rate);
        let mut out = Vec::new();
        for &sample in input {
            framer.push(sample, &mut |frame| out.extend_from_slice(frame));
        }
        out
    }

    fn ints(values: &[i16]) -> Vec<f32> {
        values.iter().map(|&v| f32::from(v) / 32_768.0).collect()
    }

    #[test]
    fn frames_hold_the_mean_of_each_input_span() {
        // 48 kHz averages triples like the Python client; 16 kHz passes through; 32 kHz averages pairs.
        for (rate, per_output) in [(48_000, 3_i32), (16_000, 1), (32_000, 2)] {
            let width = usize::try_from(per_output).unwrap_or(1);
            let input: Vec<i16> = (0..FRAME_SAMPLES * width)
                .map(|n| i16::try_from(n * 5 % 1_000).unwrap_or(0))
                .collect();
            let expected: Vec<i16> = input
                .chunks(width)
                .map(|chunk| {
                    let total: i32 = chunk.iter().map(|&v| i32::from(v)).sum();
                    i16::try_from((total + per_output / 2) / per_output).unwrap_or(0)
                })
                .collect();
            assert_eq!(run(rate, &ints(&input)), expected, "{rate} Hz");
        }
    }

    #[test]
    fn a_fractional_ratio_keeps_the_rate_and_the_level() {
        // 44.1 kHz of a constant gives 16 kHz of the same constant, one frame per 80 ms.
        let seconds = 2;
        let input = vec![0.25_f32; 44_100 * seconds];
        let out = run(44_100, &input);
        assert_eq!(out.len() / FRAME_SAMPLES, 16_000 * seconds / FRAME_SAMPLES);
        assert!(out.iter().all(|&s| s == 8_192), "constant must stay 0.25");
    }

    #[test]
    fn frame_boundaries_do_not_depend_on_callback_size() {
        let input: Vec<f32> = (0..14_400_u16)
            .map(|n| f32::from(n % 100) / 200.0)
            .collect();
        let whole = run(48_000, &input);
        let mut framer = Framer::new(48_000);
        let mut chunked = Vec::new();
        for chunk in input.chunks(441) {
            for &sample in chunk {
                framer.push(sample, &mut |frame| chunked.extend_from_slice(frame));
            }
        }
        assert_eq!(whole.len(), 3 * FRAME_SAMPLES);
        assert_eq!(whole, chunked);
    }

    #[test]
    fn loud_input_clips_instead_of_wrapping() {
        let out = run(16_000, &vec![1.5; FRAME_SAMPLES]);
        assert!(out.iter().all(|&s| s == i16::MAX));
        let out = run(16_000, &vec![-1.5; FRAME_SAMPLES]);
        assert!(out.iter().all(|&s| s == i16::MIN));
    }

    #[test]
    fn reset_discards_the_partial_frame() {
        let mut framer = Framer::new(16_000);
        let mut completed = 0;
        for _ in 0..FRAME_SAMPLES - 1 {
            framer.push(0.5, &mut |_| completed += 1);
        }
        framer.reset();
        framer.push(0.5, &mut |_| completed += 1);
        assert_eq!(completed, 0);
    }
}

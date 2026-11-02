//! Voice activity detection on microphone frames, for the pause that ends a capture.

use webrtc_vad::{SampleRate, Vad, VadMode};

use crate::frame::Frame;

/// Samples in the 20 ms sub-frames WebRTC VAD judges.
const SUB_FRAME_SAMPLES: usize = 320;

/// Speech or silence per frame, stricter while an ending is pending so that breaths and room
/// noise do not cancel it.
pub struct VadDetector {
    normal: Vad,
    pending: Vad,
}

impl VadDetector {
    #[must_use]
    pub fn new() -> Self {
        let vad = |mode| Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, mode);
        Self {
            normal: vad(VadMode::Aggressive),
            pending: vad(VadMode::VeryAggressive),
        }
    }

    /// True when any 20 ms part of the frame is speech. `pending` selects the stricter mode.
    pub fn speech(&mut self, frame: &Frame, pending: bool) -> bool {
        let vad = if pending {
            &mut self.pending
        } else {
            &mut self.normal
        };
        // Every sub-frame is judged, not only up to the first speech: the detector adapts to each.
        frame
            .as_chunks::<SUB_FRAME_SAMPLES>()
            .0
            .iter()
            .fold(false, |any, sub| {
                // Only an unsupported frame length is an error, and a 320-sample part is supported.
                vad.is_voice_segment(sub).unwrap_or(false) | any
            })
    }
}

impl Default for VadDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FRAME_SAMPLES;

    /// A 140 Hz buzz with harmonics and a slow swell, standing in for a voice.
    fn voiced(offset: usize) -> Frame {
        std::array::from_fn(|i| {
            let t = f64::from(u32::try_from(i + offset).expect("small index")) / 16_000.0;
            let swell = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
            let buzz: f64 = (1..=12)
                .map(|h| {
                    (2.0 * std::f64::consts::PI * 140.0 * f64::from(h) * t).sin() / f64::from(h)
                })
                .sum();
            #[expect(
                clippy::cast_possible_truncation,
                reason = "amplitude stays below 16 bits"
            )]
            let sample = (buzz * 4000.0 * swell) as i16;
            sample
        })
    }

    fn hiss(amplitude: i32, state: &mut u32) -> Frame {
        std::array::from_fn(|_| {
            *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let span = u32::try_from(2 * amplitude + 1).expect("positive");
            i16::try_from(i32::try_from((*state >> 16) % span).expect("small") - amplitude)
                .expect("within 16 bits")
        })
    }

    #[test]
    fn silence_is_not_speech_and_a_voice_is() {
        let mut vad = VadDetector::new();
        assert!(!vad.speech(&[0; FRAME_SAMPLES], false));
        assert!(vad.speech(&voiced(0), false));
    }

    #[test]
    fn speech_in_any_one_20_ms_part_counts() {
        let loud = voiced(0);
        for part in 0..4 {
            let mut frame = [0; FRAME_SAMPLES];
            let span = part * SUB_FRAME_SAMPLES..(part + 1) * SUB_FRAME_SAMPLES;
            frame[span.clone()].copy_from_slice(&loud[span]);
            assert!(
                VadDetector::new().speech(&frame, false),
                "speech only in part {part}"
            );
        }
    }

    #[test]
    fn a_pending_ending_ignores_quiet_hiss_that_normally_counts_as_speech() {
        let (mut normal, mut pending) = (VadDetector::new(), VadDetector::new());
        let mut state = 1;
        let results: Vec<(bool, bool)> = (0..2)
            .map(|_| {
                let frame = hiss(50, &mut state);
                (normal.speech(&frame, false), pending.speech(&frame, true))
            })
            .collect();
        assert_eq!(results, [(true, true), (true, false)]);
    }
}

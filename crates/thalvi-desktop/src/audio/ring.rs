//! The rolling buffer of the most recent microphone frames, kept for the pre-wake handover.

use crate::frame::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

/// Length of audio kept from before the wake phrase, in milliseconds.
pub const PRE_WAKE_MS: usize = 560;

const FRAME_MS: usize = FRAME_SAMPLES * 1_000 / SAMPLE_RATE as usize;

/// Frames covering [`PRE_WAKE_MS`].
pub const PRE_WAKE_FRAMES: usize = PRE_WAKE_MS / FRAME_MS;

const _: () = assert!(PRE_WAKE_FRAMES * FRAME_MS == PRE_WAKE_MS);

/// The last [`PRE_WAKE_FRAMES`] frames; pushing never allocates.
pub struct RollingAudioBuffer {
    frames: [Frame; PRE_WAKE_FRAMES],
    next: usize,
    len: usize,
}

impl Default for RollingAudioBuffer {
    fn default() -> Self {
        Self {
            frames: std::array::from_fn(|_| [0; FRAME_SAMPLES]),
            next: 0,
            len: 0,
        }
    }
}

impl RollingAudioBuffer {
    /// Stores `frame`, dropping the oldest once full, and returns the stored copy.
    pub fn push(&mut self, frame: Frame) -> &Frame {
        let slot = self.next;
        self.frames[slot] = frame;
        self.next = (slot + 1) % PRE_WAKE_FRAMES;
        self.len = (self.len + 1).min(PRE_WAKE_FRAMES);
        &self.frames[slot]
    }

    /// The kept frames from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &Frame> {
        let start = (self.next + PRE_WAKE_FRAMES - self.len) % PRE_WAKE_FRAMES;
        (0..self.len).map(move |offset| &self.frames[(start + offset) % PRE_WAKE_FRAMES])
    }

    /// Forgets every frame, because the audio after this point is not contiguous with it.
    pub fn clear(&mut self) {
        self.next = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(value: i16) -> Frame {
        [value; FRAME_SAMPLES]
    }

    fn marks(buffer: &RollingAudioBuffer) -> Vec<i16> {
        buffer.iter().map(|frame| frame[0]).collect()
    }

    #[test]
    fn holds_560_ms_as_seven_frames() {
        assert_eq!((PRE_WAKE_MS, PRE_WAKE_FRAMES), (560, 7));
    }

    #[test]
    fn keeps_the_newest_frames_in_order_as_it_fills_and_rolls_over() {
        let cases: [(i16, &[i16]); 4] = [
            (0, &[]),
            (3, &[1, 2, 3]),
            (7, &[1, 2, 3, 4, 5, 6, 7]),
            (10, &[4, 5, 6, 7, 8, 9, 10]),
        ];
        for (pushed, expected) in cases {
            let mut buffer = RollingAudioBuffer::default();
            for value in 1..=pushed {
                assert_eq!(buffer.push(marked(value))[0], value);
            }
            assert_eq!(marks(&buffer), expected, "after {pushed} pushes");
        }
    }

    #[test]
    fn clear_forgets_old_frames() {
        let mut buffer = RollingAudioBuffer::default();
        for value in 1..=9 {
            buffer.push(marked(value));
        }
        buffer.clear();
        assert_eq!(marks(&buffer), Vec::<i16>::new());
        buffer.push(marked(10));
        assert_eq!(marks(&buffer), [10]);
    }
}

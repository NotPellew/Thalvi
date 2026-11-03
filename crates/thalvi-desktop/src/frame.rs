//! The microphone frame shared by capture, wake and voice activity detection, and the order
//! frames and key releases are published in.

use std::sync::atomic::{AtomicU64, Ordering};

/// Samples per second of every frame.
pub const SAMPLE_RATE: u32 = 16_000;

/// Samples in one 80 ms frame, the unit openWakeWord scores.
pub const FRAME_SAMPLES: usize = 1_280;

/// One 80 ms block of 16 kHz mono signed 16-bit PCM.
pub type Frame = [i16; FRAME_SAMPLES];

/// Where a completed frame or a key release stands among all of them published in this process:
/// the one with the smaller order was published first. It says nothing about when the device
/// captured the sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Order(u64);

impl Order {
    /// The next place in the order; never blocks or allocates.
    #[must_use]
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A frame as it was queued for the engine.
#[derive(Clone, Copy)]
pub struct Queued {
    pub frame: Frame,
    pub order: Order,
}

impl Queued {
    /// Stamps `frame` as queued now.
    #[must_use]
    pub fn now(frame: Frame) -> Self {
        Self {
            frame,
            order: Order::next(),
        }
    }
}

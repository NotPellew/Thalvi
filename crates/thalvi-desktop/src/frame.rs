//! The microphone frame shared by capture, wake and voice activity detection.

/// Samples per second of every frame.
pub const SAMPLE_RATE: u32 = 16_000;

/// Samples in one 80 ms frame, the unit openWakeWord scores.
pub const FRAME_SAMPLES: usize = 1_280;

/// One 80 ms block of 16 kHz mono signed 16-bit PCM.
pub type Frame = [i16; FRAME_SAMPLES];

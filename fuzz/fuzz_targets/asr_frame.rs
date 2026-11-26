//! Recognizer text frames from the network into `thalvi_asr::parse_event`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(frame) = std::str::from_utf8(data) {
        let _ = thalvi_asr::parse_event(frame);
    }
});

//! Word errors and compute time of a real speech model on recorded takes, through the desktop's own path.
//!
//! Needs `THALVI_SPEECH_MODEL` (a model folder) and `THALVI_TAKES_DIR` (a folder of 48 kHz mono
//! 16-bit WAV files with a `takes.jsonl` of `file` and `text`). Prints scores only, never the
//! texts. Run with `--release --ignored --nocapture`.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::path::{Path, PathBuf};
use std::time::Instant;

use thalvi_desktop::audio::Framer;
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::{SpeechModel, SpeechStream};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::RecognizerEvent;
use thalvi_desktop::transducer::{Transducer, TransducerFiles};

/// Digital silence after each take, long enough for the last stretch to end.
const TAIL_FRAMES: usize = 40;
/// Frames that are 160 ms of audio, the time a buffered step has to finish.
const BUDGET_FRAMES: usize = 2;

fn read_wav(path: &Path) -> Vec<f32> {
    let wav = std::fs::read(path).expect("take is readable");
    let data_at = wav
        .windows(4)
        .position(|w| w == b"data")
        .expect("data chunk");
    wav[data_at + 8..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
        .collect()
}

fn frames_of(samples: &[f32]) -> Vec<Frame> {
    let mut down = Framer::new(48_000);
    let mut mono = Vec::new();
    for sample in samples {
        down.push(*sample, &mut |frame| mono.push(*frame));
    }
    mono.extend(std::iter::repeat_n([0; FRAME_SAMPLES], TAIL_FRAMES));
    mono
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace('\u{2019}', "'")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn edits(reference: &[String], heard: &[String]) -> usize {
    let mut previous: Vec<usize> = (0..=heard.len()).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, h) in heard.iter().enumerate() {
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(r != h)),
            );
        }
        previous = current;
    }
    previous[heard.len()]
}

fn percentile(sorted: &[f64], per_cent: usize) -> f64 {
    let at = sorted.len() * per_cent / 100;
    sorted
        .get(at.min(sorted.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0.0)
}

#[test]
#[ignore = "needs a model folder in THALVI_SPEECH_MODEL and takes in THALVI_TAKES_DIR"]
fn word_errors_and_compute_time_on_the_takes() {
    let model_dir =
        PathBuf::from(std::env::var("THALVI_SPEECH_MODEL").expect("THALVI_SPEECH_MODEL"));
    let takes_dir = PathBuf::from(std::env::var("THALVI_TAKES_DIR").expect("THALVI_TAKES_DIR"));
    let loaded = Instant::now();
    let model = Transducer::load(
        &TransducerFiles::in_folder(&model_dir),
        &WordList::default(),
    )
    .expect("model loads");
    let load_ms = loaded.elapsed().as_secs_f64() * 1000.0;
    let manifest = std::fs::read_to_string(takes_dir.join("takes.jsonl")).expect("takes.jsonl");
    let (mut errors, mut total, mut takes) = (0_u32, 0_u32, 0_u32);
    let mut per_audio: Vec<f64> = Vec::new();
    let mut per_frame_max = 0.0_f64;
    for line in manifest.lines().filter(|line| !line.trim().is_empty()) {
        let row: serde_json::Value = serde_json::from_str(line).expect("manifest row");
        let file = row["file"].as_str().expect("file");
        let reference = words(row["text"].as_str().expect("text"));
        let frames = frames_of(&read_wav(&takes_dir.join(file)));
        let mut stream = model.stream();
        let (mut finals, mut partial) = (Vec::<String>::new(), String::new());
        let mut cost = Vec::new();
        for frame in &frames {
            let started = Instant::now();
            let events = stream.accept(frame).expect("frame is recognized");
            cost.push(started.elapsed().as_secs_f64() * 1000.0);
            for event in events {
                match event {
                    RecognizerEvent::Final(text) => {
                        finals.push(text);
                        partial.clear();
                    }
                    RecognizerEvent::Partial(text) => partial = text,
                }
            }
        }
        per_frame_max = cost.iter().copied().fold(per_frame_max, f64::max);
        per_audio.extend(
            cost.chunks(BUDGET_FRAMES)
                .map(|pair| pair.iter().sum::<f64>()),
        );
        finals.push(partial);
        let heard = words(&finals.join(" "));
        errors += u32::try_from(edits(&reference, &heard)).expect("small");
        total += u32::try_from(reference.len()).expect("small");
        takes += 1;
    }
    per_audio.sort_by(f64::total_cmp);
    println!(
        "takes {takes}  words {total}  errors {errors}  wer {:.2}%  load {load_ms:.0} ms  compute per 160 ms of audio: p50 {:.1} ms  p95 {:.1} ms  slowest frame {per_frame_max:.1} ms",
        100.0 * f64::from(errors) / f64::from(total),
        percentile(&per_audio, 50),
        percentile(&per_audio, 95),
    );
}

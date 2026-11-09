//! Word errors and compute time of a real speech model on recorded takes, through the desktop's own path.
//!
//! Needs `THALVI_SPEECH_MODEL` (a model folder) or `THALVI_MODEL_STORE` (a store of catalog files, loaded
//! as the app loads `unified_160ms`: on a GPU when the adapter check finds one, so `VK_ICD_FILENAMES` picks
//! the device), and `THALVI_TAKES_DIR` (a folder of 48 kHz mono 16-bit WAV files with a `takes.jsonl` of
//! `file` and `text`). Prints scores only, never the texts. Run with `--release --ignored --nocapture`.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::path::{Path, PathBuf};
use std::time::Instant;

use thalvi_desktop::audio::Framer;
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::{ModelSource, SpeechModel, SpeechStream};
use thalvi_desktop::models::{CatalogSpeech, ModelStore};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::RecognizerEvent;
use thalvi_desktop::transducer::{
    Recognition, SpeechChoice, Transducer, TransducerFiles, TransducerSource,
};

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

/// The model as the app loads it from a store, or from a plain folder.
fn load_model() -> Transducer {
    match std::env::var("THALVI_MODEL_STORE") {
        Ok(store) => {
            let source = TransducerSource::new(
                ModelStore::in_dir(store),
                SpeechChoice::Catalog(CatalogSpeech::Unified160ms),
                &WordList::default(),
            );
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(source.load())
                .expect("model loads")
        }
        Err(_) => Transducer::load(
            &TransducerFiles::in_folder(&PathBuf::from(
                std::env::var("THALVI_SPEECH_MODEL").expect("THALVI_SPEECH_MODEL"),
            )),
            &WordList::default(),
        )
        .expect("model loads"),
    }
}

#[test]
#[ignore = "needs a model in THALVI_SPEECH_MODEL or THALVI_MODEL_STORE and takes in THALVI_TAKES_DIR"]
fn word_errors_and_compute_time_on_the_takes() {
    let takes_dir = PathBuf::from(std::env::var("THALVI_TAKES_DIR").expect("THALVI_TAKES_DIR"));
    let loaded = Instant::now();
    let model = load_model();
    println!("device: {}", model.device().line(true));
    let load_ms = loaded.elapsed().as_secs_f64() * 1000.0;
    let tdt = matches!(model.stream(), Recognition::Revising(_));
    let manifest = std::fs::read_to_string(takes_dir.join("takes.jsonl")).expect("takes.jsonl");
    let (mut errors, mut total, mut takes) = (0_u32, 0_u32, 0_u32);
    let mut per_audio: Vec<f64> = Vec::new();
    let mut per_frame_max = 0.0_f64;
    let mut settle = Vec::new();
    let mut lag = Vec::new();
    let mut empty = 0;
    for line in manifest.lines().filter(|line| !line.trim().is_empty()) {
        let row: serde_json::Value = serde_json::from_str(line).expect("manifest row");
        let file = row["file"].as_str().expect("file");
        let reference = words(row["text"].as_str().expect("text"));
        let samples = read_wav(&takes_dir.join(file));
        let audio_seconds = f64::from(u32::try_from(samples.len()).expect("short take")) / 48_000.0;
        let frames = frames_of(&samples);
        let mut available = 0.0_f64;
        let mut last_final = None;
        let mut stream = model.stream();
        let (mut finals, mut partial) = (Vec::<String>::new(), String::new());
        let mut cost = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let started = Instant::now();
            let events = stream.accept(frame).expect("frame is recognized");
            let seconds = started.elapsed().as_secs_f64();
            cost.push(seconds * 1000.0);
            let received = f64::from(u32::try_from(index + 1).expect("short take")) * 0.08;
            // Project measured compute onto 80 ms arrivals, including any queueing.
            available = available.max(received) + seconds;
            if !events.is_empty() {
                lag.push((available - received) * 1000.0);
            }
            for event in events {
                match event {
                    RecognizerEvent::Final(text) => {
                        if !words(&text).is_empty() {
                            last_final = Some(available);
                        }
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
        empty += usize::from(heard.is_empty());
        if let Some(at) = last_final {
            settle.push((at - audio_seconds).max(0.0) * 1000.0);
        }
        errors += u32::try_from(edits(&reference, &heard)).expect("small");
        total += u32::try_from(reference.len()).expect("small");
        takes += 1;
    }
    per_audio.sort_by(f64::total_cmp);
    settle.sort_by(f64::total_cmp);
    lag.sort_by(f64::total_cmp);
    println!(
        "takes {takes}  words {total}  errors {errors}  wer {:.2}%  load {load_ms:.0} ms  compute per 160 ms of audio: p50 {:.1} ms  p95 {:.1} ms  slowest frame {per_frame_max:.1} ms  partial lag: p50 {:.1} ms  p95 {:.1} ms  settle lag: p50 {:.1} ms  p95 {:.1} ms  empty {empty}  finalized {}/{}",
        100.0 * f64::from(errors) / f64::from(total),
        percentile(&per_audio, 50),
        percentile(&per_audio, 95),
        percentile(&lag, 50),
        percentile(&lag, 95),
        percentile(&settle, 50),
        percentile(&settle, 95),
        settle.len(),
        takes,
    );
    if tdt {
        assert!(
            100.0 * f64::from(errors) / f64::from(total) <= 5.5,
            "TDT must stay within 5.5% word errors: {errors}/{total}"
        );
        assert_eq!(empty, 0, "TDT must recognize every take");
    }
}

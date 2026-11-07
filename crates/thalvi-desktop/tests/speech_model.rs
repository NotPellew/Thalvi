//! The real speech model transcribes a `LibriSpeech` clip the way sherpa-onnx does.
//!
//! Needs the model files (`encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`,
//! `tokens.txt`) in the directory named by `THALVI_SPEECH_MODEL`; run with `--ignored`.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::path::PathBuf;

use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::{SpeechModel, SpeechStream};
use thalvi_desktop::models::{Asset, ModelKind, ModelStore};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::RecognizerEvent;
use thalvi_desktop::transducer::{Transducer, TransducerFiles};

/// A `LibriSpeech` test utterance (CC BY 4.0) shipped with sherpa-onnx's model repositories.
const CLIP: Asset<'static> = Asset {
    kind: ModelKind::Speech,
    file_name: "librispeech-test-0.wav",
    url: "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26/resolve/672fbf1b30579d6585301139bb363f42a0ad4a24/test_wavs/0.wav",
    size: 212_044,
    sha256: "6bc58a4efdf20daac252b6b1502632601a71efe0308f6757dc1eda34891a7e4f",
};

async fn clip_frames() -> Vec<Frame> {
    let store = ModelStore::in_dir(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("models"));
    let wav = std::fs::read(store.fetch(&CLIP).await.expect("clip downloads")).expect("readable");
    let data_at = wav
        .windows(4)
        .position(|w| w == b"data")
        .expect("data chunk");
    let mut samples: Vec<i16> = wav[data_at + 8..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    samples.resize(samples.len() + 24_000, 0);
    samples.as_chunks::<FRAME_SAMPLES>().0.to_vec()
}

#[tokio::test]
#[ignore = "needs the speech model files in THALVI_SPEECH_MODEL"]
async fn the_clip_is_recognized_in_full_as_partials_that_grow_and_a_final() {
    let dir = PathBuf::from(std::env::var("THALVI_SPEECH_MODEL").expect("THALVI_SPEECH_MODEL"));
    let model = Transducer::load(
        &TransducerFiles {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
        },
        &WordList::default(),
    )
    .expect("model loads");
    let mut stream = model.stream();
    let mut events = Vec::new();
    for frame in clip_frames().await {
        events.extend(stream.accept(&frame).expect("frame is recognized"));
    }
    let finals: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            RecognizerEvent::Final(text) => Some(text.as_str()),
            RecognizerEvent::Partial(_) => None,
        })
        .collect();
    assert_eq!(
        finals,
        [
            "After early nightfall the yellow lamps would light up here and there the squalid quarter of the brothels"
        ]
    );
    assert!(events.contains(&RecognizerEvent::Partial(
        "After early nightfall".to_owned()
    )));
}

const SENTENCE: &str = "after early nightfall the yellow lamps would light up here and there the squalid quarter of the brothels";

fn unified() -> Transducer {
    let dir = PathBuf::from(std::env::var("THALVI_UNIFIED_MODEL").expect("THALVI_UNIFIED_MODEL"));
    Transducer::load(&TransducerFiles::in_folder(&dir), &WordList::default()).expect("model loads")
}

/// What the finals say, without capitals and punctuation.
fn spoken(frames: &[Frame]) -> Vec<String> {
    let mut stream = unified().stream();
    let mut finals = Vec::new();
    for frame in frames {
        for event in stream.accept(frame).expect("frame is recognized") {
            if let RecognizerEvent::Final(text) = event {
                finals.push(
                    text.to_lowercase()
                        .chars()
                        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
                        .collect::<String>()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
        }
    }
    finals
}

#[tokio::test]
#[ignore = "needs the unified model files in THALVI_UNIFIED_MODEL"]
async fn the_unified_model_recognizes_the_clip() {
    assert_eq!(spoken(&clip_frames().await), [SENTENCE]);
}

#[tokio::test]
#[ignore = "needs the unified model files in THALVI_UNIFIED_MODEL"]
async fn the_unified_model_recovers_a_clip_after_thirty_seconds_of_digital_silence() {
    let clip = clip_frames().await;
    let silence = std::iter::repeat_n([0; FRAME_SAMPLES], 30 * 16_000 / FRAME_SAMPLES);
    let frames: Vec<Frame> = clip
        .iter()
        .copied()
        .chain(silence)
        .chain(clip.iter().copied())
        .collect();
    assert_eq!(spoken(&frames), [SENTENCE, SENTENCE]);
}

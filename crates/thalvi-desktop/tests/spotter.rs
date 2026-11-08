//! The real ending phrase model hears a configured phrase in a recorded clip where sherpa-onnx does.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::collections::BTreeMap;
use std::path::PathBuf;

use thalvi_desktop::capture::{Ending, Endings};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::models::{Asset, ModelKind, ModelStore};
use thalvi_desktop::spotter::{KeywordSpotter, OnnxSpotter, SpotterError, SpotterHit, Unspeakable};

/// A `LibriSpeech` test utterance (CC BY 4.0) shipped with sherpa-onnx's model repositories; it
/// says "... the yellow lamps would light up here and there ...".
const CLIP: Asset<'static> = Asset {
    kind: ModelKind::Spotter,
    file_name: "librispeech-test-0.wav",
    url: "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26/resolve/672fbf1b30579d6585301139bb363f42a0ad4a24/test_wavs/0.wav",
    size: 212_044,
    sha256: "6bc58a4efdf20daac252b6b1502632601a71efe0308f6757dc1eda34891a7e4f",
};

/// Another of them, which says "... a lovely child ..." and, later, "... for ever ...".
const LONG_CLIP: Asset<'static> = Asset {
    kind: ModelKind::Spotter,
    file_name: "librispeech-test-1.wav",
    url: "https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-en-2023-06-26/resolve/672fbf1b30579d6585301139bb363f42a0ad4a24/test_wavs/1.wav",
    size: 534_924,
    sha256: "5143a6ba93c4b274e2c4ac22deb75c2c48936c853f0519add1de828b6c79cc5a",
};

fn store() -> ModelStore {
    ModelStore::in_dir(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("models"))
}

async fn clip_frames() -> Vec<Frame> {
    frames_of(&CLIP).await
}

async fn frames_of(clip: &Asset<'_>) -> Vec<Frame> {
    let wav = std::fs::read(store().fetch(clip).await.expect("clip downloads")).expect("readable");
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

/// The ending phrases sherpa-onnx's own spotter was run with to produce the expected positions.
fn reference_endings() -> Endings {
    Endings::try_from(BTreeMap::from([
        ("light up".to_owned(), Ending::Paste),
        ("lovely child".to_owned(), Ending::Submit),
        ("forever".to_owned(), Ending::Task),
        ("submit".to_owned(), Ending::Scratch),
        ("paste that".to_owned(), Ending::Cancel),
    ]))
    .expect("valid endings")
}

async fn spotter(endings: &Endings) -> OnnxSpotter {
    OnnxSpotter::load(endings, &store())
        .await
        .expect("model loads")
}

/// Every hit with the index of the frame that produced it.
fn hits(spotter: &mut OnnxSpotter, frames: &[Frame]) -> Vec<(usize, SpotterHit)> {
    frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| {
            spotter
                .push(frame)
                .expect("frame is spotted")
                .map(|hit| (index, hit))
        })
        .collect()
}

fn silence(frames: usize) -> Vec<Frame> {
    vec![[0; FRAME_SAMPLES]; frames]
}

/// sherpa-onnx's spotter, run on this clip with the same phrases and defaults, reports "light up"
/// after the frame at index 45, its tokens emitted on encoder frames 75 to 79 (3.0 s to 3.16 s).
/// Int8 kernels differ a little between CPUs, so a position may move by one frame.
fn assert_light_up(index: usize, hit: &SpotterHit, shifted_by: usize) {
    const ENCODER_FRAME: u64 = 640;
    let shift = u64::try_from(shifted_by * FRAME_SAMPLES).expect("fits");
    assert_eq!(
        (hit.phrase.as_str(), hit.ending),
        ("light up", Ending::Paste)
    );
    assert!(
        (45 + shifted_by).abs_diff(index) <= 1,
        "heard after frame {index}"
    );
    assert!(
        hit.start_sample.abs_diff(48_000 + shift) <= ENCODER_FRAME,
        "starts at {}",
        hit.start_sample
    );
    assert!(
        hit.end_sample.abs_diff(51_200 + shift) <= ENCODER_FRAME,
        "ends at {}",
        hit.end_sample
    );
    assert!((0.25..=1.0).contains(&hit.confidence), "{}", hit.confidence);
}

#[tokio::test]
async fn a_configured_phrase_in_the_clip_is_heard_once_where_sherpa_onnx_hears_it() {
    let frames = clip_frames().await;
    let mut spotter = spotter(&reference_endings()).await;
    let heard = hits(&mut spotter, &frames);
    let [(index, hit)] = &heard[..] else {
        panic!("expected one hit, got {heard:?}");
    };
    assert_light_up(*index, hit, 0);
}

#[tokio::test]
async fn two_phrases_in_a_longer_clip_are_each_heard_once_where_sherpa_onnx_hears_them() {
    // sherpa-onnx reports "lovely child" after the frame at index 81, its tokens emitted from 5.4 s
    // to 6.04 s, and "forever" after index 141 with tokens from 10.88 s to 11.08 s.
    let mut spotter = spotter(&reference_endings()).await;
    let heard = hits(&mut spotter, &frames_of(&LONG_CLIP).await);
    let expected: [(&str, Ending, usize, u64, u64); 2] = [
        ("lovely child", Ending::Submit, 81, 86_400, 97_280),
        ("forever", Ending::Task, 141, 174_080, 177_920),
    ];
    assert_eq!(heard.len(), expected.len(), "got {heard:?}");
    for ((at, hit), (phrase, ending, index, start, end)) in heard.iter().zip(expected) {
        assert_eq!((hit.phrase.as_str(), hit.ending), (phrase, ending));
        assert!(index.abs_diff(*at) <= 1, "{phrase} heard after frame {at}");
        assert!(
            hit.start_sample.abs_diff(start) <= 640,
            "{phrase} starts at {}",
            hit.start_sample
        );
        assert!(
            hit.end_sample.abs_diff(end) <= 640,
            "{phrase} ends at {}",
            hit.end_sample
        );
    }
}

#[tokio::test]
async fn a_phrase_the_model_is_too_unsure_of_is_not_heard() {
    // The clip says "place" at 6.9 s, but sherpa-onnx's spotter only reports it when its acoustic
    // threshold is lowered from 0.25 to 0.01; it does report the other two, which are said clearly.
    let endings = Endings::try_from(BTreeMap::from([
        ("god".to_owned(), Ending::Paste),
        ("place".to_owned(), Ending::Submit),
        ("blessed".to_owned(), Ending::Task),
        ("lamp mortals".to_owned(), Ending::Scratch),
        ("direct finish".to_owned(), Ending::Cancel),
    ]))
    .expect("valid endings");
    let mut spotter = spotter(&endings).await;
    let heard = hits(&mut spotter, &frames_of(&LONG_CLIP).await);
    let phrases: Vec<&str> = heard.iter().map(|(_, hit)| hit.phrase.as_str()).collect();
    assert_eq!(phrases, ["god", "blessed"]);
}

#[tokio::test]
async fn phrases_the_clip_does_not_say_are_not_heard() {
    let frames = clip_frames().await;
    let mut spotter = spotter(&Endings::default()).await;
    assert_eq!(hits(&mut spotter, &frames), []);
}

#[tokio::test]
async fn positions_count_from_the_last_reset_and_from_the_start_across_silence() {
    let frames = clip_frames().await;
    let mut spotter = spotter(&reference_endings()).await;
    let fresh = hits(&mut spotter, &frames);

    spotter.reset();
    let _ = hits(&mut spotter, &frames[..30]);
    spotter.reset();
    assert_eq!(
        hits(&mut spotter, &frames),
        fresh,
        "audio before a reset is forgotten"
    );

    spotter.reset();
    let quiet = 40;
    let padded: Vec<Frame> = silence(quiet).into_iter().chain(frames).collect();
    let heard = hits(&mut spotter, &padded);
    let [(index, hit)] = &heard[..] else {
        panic!("expected one hit, got {heard:?}");
    };
    assert_light_up(*index, hit, quiet);
}

#[tokio::test]
async fn phrases_the_model_cannot_hear_are_refused_by_name() {
    let endings = Endings::try_from(BTreeMap::from([
        ("route 66".to_owned(), Ending::Paste),
        ("submit".to_owned(), Ending::Submit),
        ("do that".to_owned(), Ending::Task),
        ("scratch that".to_owned(), Ending::Scratch),
        ("never mind".to_owned(), Ending::Cancel),
    ]))
    .expect("valid endings");
    let Err(error) = OnnxSpotter::load(&endings, &store()).await else {
        panic!("a phrase with digits must be refused");
    };
    assert!(
        matches!(&error, SpotterError::Phrases(bad) if bad == &[Unspeakable { phrase: "route 66".into(), characters: "66".into() }]),
        "{error:?}"
    );
    assert!(error.to_string().contains("\"route 66\""), "{error}");
}

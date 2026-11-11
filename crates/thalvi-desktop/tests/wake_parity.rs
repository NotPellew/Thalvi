//! Dictation wake scores match Python openwakeword 0.6.0 and ignore the saved conversation choice.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::models::{Asset, ModelKind, ModelStore};
use thalvi_desktop::wake::{WakeConfig, WakeEngine, WakeEvent, WakeFrame};

/// Largest accepted absolute difference between a Rust and a Python score (scores are 0 to 1).
const TOLERANCE: f32 = 1e-4;

/// openWakeWord's own positive test clip, from its Apache-2.0 repository, pinned by commit.
const CLIP: Asset<'static> = Asset {
    kind: ModelKind::Wake,
    file_name: "hey_mycroft_test.wav",
    url: "https://raw.githubusercontent.com/dscripka/openWakeWord/368c03716d1e92591906a84949bc477f3a834455/tests/data/hey_mycroft_test.wav",
    size: 30_508,
    sha256: "8d728ce1dbd54a97c6cddf0c80acec3113285407a967368f4abe476119c1a8c6",
};

#[derive(Deserialize)]
struct Reference {
    formula: Scenario,
    clip: Scenario,
}

#[derive(Deserialize)]
struct Scenario {
    reset_before: Vec<usize>,
    scores: BTreeMap<String, Vec<f32>>,
}

fn store() -> ModelStore {
    ModelStore::in_dir(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("models"))
}

fn config(json: &str) -> WakeConfig {
    serde_json::from_str(json).expect("valid wake config")
}

async fn engine(json: &str) -> WakeEngine {
    WakeEngine::load(&config(json), &store())
        .await
        .expect("wake engine loads")
}

fn frames(samples: &[i16]) -> Vec<Frame> {
    samples.as_chunks::<FRAME_SAMPLES>().0.to_vec()
}

/// Integer-only so it matches the Python generator exactly.
fn formula_frames(count: usize) -> Vec<Frame> {
    let mut noise_state: u32 = 7;
    let samples: Vec<i16> = (0..count * FRAME_SAMPLES)
        .map(|n| {
            noise_state = noise_state
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            let n = i64::try_from(n).expect("small index");
            let saw = (n * 61) % 4001 - 2000;
            let envelope = (n % 32_000 - 16_000).abs();
            let noise = i64::from(noise_state >> 16) % 4001 - 2000;
            i16::try_from(saw.div_euclid(2) + (noise * envelope).div_euclid(16_000))
                .expect("within 16 bits")
        })
        .collect();
    frames(&samples)
}

async fn clip_frames() -> Vec<Frame> {
    let path = store().fetch(&CLIP).await.expect("clip downloads");
    let wav = std::fs::read(path).expect("clip readable");
    let data_at = wav
        .windows(4)
        .position(|w| w == b"data")
        .expect("data chunk");
    let data = &wav[data_at + 8..];
    let mut samples = vec![0; 16_000];
    samples.extend(
        data.as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair)),
    );
    samples.resize(samples.len() + 16_000, 0);
    frames(&samples)
}

fn run(engine: &mut WakeEngine, frames: &[Frame], reset_before: &[usize]) -> Vec<WakeFrame> {
    frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            if reset_before.contains(&index) {
                engine.reset();
            }
            engine.process(frame).expect("frame scores")
        })
        .collect()
}

fn max_error(expected: &[f32], actual: impl Iterator<Item = f32>) -> f32 {
    let actual: Vec<f32> = actual.collect();
    assert_eq!(actual.len(), expected.len(), "one score per frame");
    expected
        .iter()
        .zip(&actual)
        .map(|(want, got)| (want - got).abs())
        .fold(0.0, f32::max)
}

#[tokio::test]
async fn dictation_scores_match_openwakeword_for_catalog_and_path_heads() {
    let reference: Reference =
        serde_json::from_str(include_str!("fixtures/wake_reference.json")).expect("fixture");
    let thalvi = store()
        .fetch(&thalvi_desktop::models::CatalogHead::HeyThalvi.asset())
        .await
        .expect("hey_thalvi downloads");
    let by_name = "{}";
    let by_path = format!(
        r#"{{"capture":{{"model":{:?}}}}}"#,
        thalvi.display().to_string()
    );
    let inputs = [
        ("formula", &reference.formula, formula_frames(80)),
        ("clip", &reference.clip, clip_frames().await),
    ];
    let mut worst = 0.0_f32;
    for (name, scenario, input) in &inputs {
        let mut default = engine(by_name).await;
        let ran = run(&mut default, input, &scenario.reset_before);
        let capture = &scenario.scores["hey_thalvi_v0.0.14"];
        let capture = max_error(capture, ran.iter().map(|f| f.capture));

        let mut jarvis_engine = engine(r#"{"capture":{"model":"hey_jarvis"}}"#).await;
        let ran = run(&mut jarvis_engine, input, &scenario.reset_before);
        let jarvis = max_error(
            &scenario.scores["hey_jarvis_v0.1"],
            ran.iter().map(|f| f.capture),
        );

        let mut path_engine = engine(&by_path).await;
        let ran = run(&mut path_engine, input, &scenario.reset_before);
        let path_thalvi = max_error(
            &scenario.scores["hey_thalvi_v0.0.14"],
            ran.iter().map(|f| f.capture),
        );
        eprintln!(
            "{name}: max abs error thalvi {capture:e}, jarvis {jarvis:e}, thalvi by path {path_thalvi:e}"
        );
        worst = worst.max(capture).max(jarvis).max(path_thalvi);
    }
    assert!(
        worst <= TOLERANCE,
        "scores differ from openwakeword by {worst}, more than {TOLERANCE}"
    );
}

#[tokio::test]
async fn the_desktop_only_wakes_for_its_selected_dictation_head() {
    let input = clip_frames().await;
    let events = |frames: Vec<WakeFrame>| -> Vec<WakeEvent> {
        frames.into_iter().filter_map(|f| f.event).collect()
    };

    let mut default = engine("{}").await;
    let conversation = events(run(&mut default, &input, &[]));
    assert!(
        conversation.is_empty(),
        "the saved hey_mycroft conversation choice must not wake the desktop, got {conversation:?}"
    );

    let mut swapped =
        engine(r#"{"capture":{"model":"hey_mycroft"},"conversation":{"model":"hey_jarvis"}}"#)
            .await;
    let capture = events(run(&mut swapped, &input, &[]));
    assert!(
        !capture.is_empty() && capture.iter().all(|e| *e == WakeEvent::Capture),
        "the same clip should wake the capture route when hey_mycroft is the capture head, got {capture:?}"
    );
}

#[tokio::test]
async fn only_the_dictation_threshold_controls_wakes() {
    // hey_mycroft peaks at 0.528 on this signal, at frame 51 (see the reference fixture).
    let input = formula_frames(80);
    let woken = |frames: Vec<WakeFrame>| -> Vec<(usize, WakeEvent)> {
        frames
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.event.map(|e| (i, e)))
            .collect()
    };

    let mut default = engine("{}").await;
    assert_eq!(woken(run(&mut default, &input, &[40])), []);

    let mut at_half = engine(r#"{"capture":{"model":"hey_mycroft","threshold":0.5}}"#).await;
    assert_eq!(
        woken(run(&mut at_half, &input, &[40])),
        [(51, WakeEvent::Capture)]
    );

    let mut at_sixty = engine(r#"{"capture":{"model":"hey_mycroft","threshold":0.6}}"#).await;
    assert_eq!(woken(run(&mut at_sixty, &input, &[40])), []);
}

#[tokio::test]
async fn a_dictation_crossing_wakes_with_a_saved_conversation_head() {
    let input = clip_frames().await;
    let mut both = engine(
        r#"{"capture":{"model":"hey_mycroft","threshold":0.5},"conversation":{"model":"hey_jarvis","threshold":0.000001}}"#,
    )
    .await;
    let ran = run(&mut both, &input, &[]);
    let crossing = ran
        .iter()
        .find(|f| f.capture >= 0.5)
        .expect("hey_mycroft crosses");
    assert_eq!(crossing.event, Some(WakeEvent::Capture));
}

#[tokio::test]
async fn a_start_or_reset_scores_zero_for_five_frames_then_real_scores() {
    // hey_jarvis scores above zero on the formula signal; hey_thalvi rounds to exactly zero there.
    let mut engine = engine(r#"{"capture":{"model":"hey_jarvis"}}"#).await;
    let ran = run(&mut engine, &formula_frames(50), &[40]);
    for first in [0, 40] {
        assert!(
            ran[first..first + 5].iter().all(|f| f.capture == 0.0),
            "frames {first}..{} should score zero",
            first + 5
        );
        assert!(
            ran[first + 5].capture > 0.0,
            "frame {} should score for real",
            first + 5
        );
    }
}

#[tokio::test]
async fn reset_forgets_the_clip_so_the_next_run_scores_like_a_fresh_start() {
    let input = clip_frames().await;
    let mut engine = engine("{}").await;
    let first = run(&mut engine, &input, &[]);
    let again = run(&mut engine, &input, &[0]);
    let scores = |frames: &[WakeFrame]| -> Vec<f32> { frames.iter().map(|f| f.capture).collect() };
    assert_eq!(scores(&first), scores(&again));
}

//! The ending spotter on recorded speech: false alarms and recall per ending, and how many notices the engine's rules would give.
//!
//! Needs `THALVI_RECORDINGS_DIR`, a folder tree of 16 kHz mono 16-bit WAVs. Where a folder has a
//! `speech.wav` with a `text.txt` beside it (the desktop's own recordings), the `heard:` lines say
//! which endings the transcript had; any other tree is scored without transcripts, so every hit
//! counts as a candidate false alarm. `THALVI_SPOT_MODELS` is where the models are downloaded to
//! (default: the test target's `models`), `THALVI_SPOT_OUT` a file for the places of spotter-only
//! hits, and `THALVI_SPOT_LIMIT` caps the number of recordings. Setting `THALVI_SPEECH_MODEL` to a
//! catalog name or a model folder adds the second half: the local recognizer's finals and partials
//! go through the engine's own `claim::Watch` with the same voice activity detector, giving each hit's
//! outcome, the notices per hour and how late the covering final comes after the phrase.
//!
//! Prints counts and durations only, never texts. Run with `--release --ignored --nocapture`.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thalvi_desktop::capture::{
    CaptureSession, Ending, EndingPauses, Endings, FinalStep, has_words,
};
use thalvi_desktop::claim::{Effect, Outcome, Resolution, Watch};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame, SAMPLE_RATE};
use thalvi_desktop::local::{ModelSource, SpeechModel, SpeechStream};
use thalvi_desktop::models::{CatalogSpeech, ModelStore};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::RecognizerEvent;
use thalvi_desktop::spotter::{KeywordSpotter, OnnxSpotter, SpotterHit};
use thalvi_desktop::transducer::{SpeechChoice, Transducer, TransducerSource};
use thalvi_desktop::vad::VadDetector;
use tokio::time::Instant;

/// Silence after each recording, 3.6 s: long enough for the recognizer to endpoint and for a late final.
const TAIL_FRAMES: usize = 45;
/// A hit this close to the end of a recording, or nearer, is where an ending is said.
const TAIL: u64 = 4 * SAMPLE_RATE as u64;
const THRESHOLDS: [f32; 3] = [0.25, 0.30, 0.35];

fn store() -> ModelStore {
    ModelStore::in_dir(std::env::var("THALVI_SPOT_MODELS").map_or_else(
        |_| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("models"),
        PathBuf::from,
    ))
}

fn wavs(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("folder is readable")
        .map(|entry| entry.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            wavs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "wav") {
            out.push(path);
        }
    }
}

/// The recordings to score: every `speech.wav` when there are any, else every WAV.
fn recordings(root: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    wavs(root, &mut all);
    let speech: Vec<PathBuf> = all
        .iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "speech.wav"))
        .cloned()
        .collect();
    if speech.is_empty() { all } else { speech }
}

/// The samples of a 16 kHz mono 16-bit WAV, or why it cannot be scored.
fn read_wav(path: &Path) -> Result<Vec<i16>, String> {
    let wav = std::fs::read(path).map_err(|error| error.to_string())?;
    let mut at = 12;
    let mut format = None;
    while at + 8 <= wav.len() {
        let size = u32::from_le_bytes(wav[at + 4..at + 8].try_into().expect("four bytes")) as usize;
        let body = &wav[at + 8..(at + 8 + size).min(wav.len())];
        match &wav[at..at + 4] {
            b"fmt " if body.len() >= 16 => {
                format = Some((
                    u16::from_le_bytes([body[0], body[1]]),
                    u16::from_le_bytes([body[2], body[3]]),
                    u32::from_le_bytes(body[4..8].try_into().expect("four bytes")),
                    u16::from_le_bytes([body[14], body[15]]),
                ));
            }
            b"data" => {
                return match format {
                    Some((1, 1, 16_000, 16)) => Ok(body
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| i16::from_le_bytes(*pair))
                        .collect()),
                    other => Err(format!("format {other:?}")),
                };
            }
            _ => {}
        }
        at += 8 + size + size % 2;
    }
    Err("no data chunk".to_owned())
}

/// The recording's samples as frames, padded to a whole frame, with the release silence after.
fn frames_of(samples: &[i16]) -> Vec<Frame> {
    let mut padded = samples.to_vec();
    padded.resize(
        samples.len().div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES + TAIL_FRAMES * FRAME_SAMPLES,
        0,
    );
    padded.as_chunks::<FRAME_SAMPLES>().0.to_vec()
}

/// The finals of a capture as the desktop recorded them in `text.txt`.
fn heard(path: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path.with_file_name("text.txt")).ok()?;
    Some(
        text.lines()
            .filter_map(|line| line.strip_prefix("heard: "))
            .map(str::to_owned)
            .collect(),
    )
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace('\u{2019}', "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-'))
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The phrase `text` ends with, longest phrase first; trailing artifacts are not skipped, so this
/// is an approximation of the capture's own rule that can only miss.
fn phrase_of<'a>(endings: &'a Endings, text: &str) -> Option<(&'a str, Ending)> {
    let spoken = words(text);
    endings
        .entries()
        .find(|(phrase, _)| spoken.ends_with(&words(phrase)))
}

type Step = Result<FinalStep, thalvi_desktop::capture::CaptureFullError>;

/// The ending that `step`, which applied `text` to `before`, carried out.
fn literal<'a>(
    endings: &'a Endings,
    before: &CaptureSession,
    text: &str,
    step: &Step,
) -> Option<(&'a str, Ending)> {
    let (phrase, ending) = phrase_of(endings, &format!("{} {text}", before.text()))?;
    let acted = match step {
        Ok(FinalStep::Armed(_)) => !matches!(ending, Ending::Scratch | Ending::Cancel),
        Ok(FinalStep::Cancelled { .. }) => ending == Ending::Cancel,
        Ok(FinalStep::Continue { .. }) => ending == Ending::Scratch,
        Err(_) => false,
    };
    acted.then_some((phrase, ending))
}

fn session() -> CaptureSession {
    CaptureSession::new(Endings::default(), EndingPauses::default(), Vec::new())
}

/// What literal endings the transcript of one capture carried, by phrase.
fn literal_phrases(endings: &Endings, finals: &[String]) -> Vec<String> {
    let mut capture = session();
    let mut found = Vec::new();
    for text in finals {
        let before = capture.clone();
        let step = capture.on_final(text, Instant::now(), false);
        if let Some((phrase, _)) = literal(endings, &before, text, &step) {
            found.push(phrase.to_owned());
        }
    }
    found
}

#[derive(Default)]
struct Class {
    literal: u32,
    agreed: u32,
    only_body: u32,
    only_tail: u32,
    agreed_confidence: Vec<f32>,
    only_confidence: Vec<f32>,
}

fn percentile<T: Copy + PartialOrd>(values: &mut [T], per_cent: usize) -> Option<T> {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    values
        .get((values.len() * per_cent / 100).min(values.len().saturating_sub(1)))
        .copied()
}

fn spread(values: &[f32]) -> String {
    let mut sorted = values.to_vec();
    match (
        percentile(&mut sorted, 0),
        percentile(&mut sorted, 50),
        percentile(&mut sorted, 100),
    ) {
        (Some(min), Some(median), Some(max)) => format!("{min:.2}/{median:.2}/{max:.2}"),
        _ => "-".to_owned(),
    }
}

fn at_least(values: &[f32]) -> String {
    THRESHOLDS
        .iter()
        .map(|threshold| {
            values
                .iter()
                .filter(|value| **value >= *threshold)
                .count()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The engine's handling of hits, run on one recording as it streams through a speech model.
struct Mirror<'a, M: SpeechModel> {
    frames: &'a [Frame],
    stream: M::Stream,
    capture: CaptureSession,
    watch: Watch,
    vad: VadDetector,
    start: Instant,
    outcomes: Vec<(Resolution, usize)>,
    /// The frame each notice came at.
    notices: Vec<usize>,
}

impl<'a, M: SpeechModel> Mirror<'a, M> {
    fn new(model: &M, frames: &'a [Frame]) -> Self {
        Self {
            frames,
            stream: model.stream(),
            capture: session(),
            watch: Watch::new(EndingPauses::default()),
            vad: VadDetector::new(),
            start: now(),
            outcomes: Vec::new(),
            notices: Vec::new(),
        }
    }

    /// One frame: sent to the recognizer and the spotter, then what the recognizer made of it.
    fn frame(&mut self, index: usize, hit: Option<&SpotterHit>) {
        let at = self.start + Duration::from_millis(80 * (index as u64 + 1));
        let frame = &self.frames[index];
        let speech = self.vad.speech(
            frame,
            self.capture.pending().is_some() || self.watch.disputed(),
        );
        if speech {
            self.watch.speaking();
        } else {
            self.watch.quiet_frame(at);
        }
        self.watch.sent(1, at, speech);
        if let Some(hit) = hit {
            self.outcomes
                .extend(self.watch.hit(hit, at, speech).map(|done| (done, index)));
        }
        for event in self.stream.accept(frame).expect("frame is recognized") {
            match event {
                RecognizerEvent::Partial(text) if has_words(&text) => {
                    self.outcomes
                        .extend(self.watch.words(at).map(|done| (done, index)));
                }
                RecognizerEvent::Partial(_) => {}
                RecognizerEvent::Final(text) => {
                    let before = self.capture.clone();
                    let step = self.capture.on_final(&text, at, speech);
                    let effect = Effect::of(&before, &self.capture, &step);
                    self.outcomes.extend(
                        self.watch
                            .final_arrived(effect, at, speech)
                            .map(|done| (done, index)),
                    );
                }
            }
        }
        if let Some((_, done)) = self.watch.notice(at) {
            self.notices.push(index);
            self.outcomes.push((done, index));
        }
    }

    fn finish(mut self) -> (Vec<(Resolution, usize)>, Vec<usize>) {
        let at = self.start + Duration::from_millis(80 * self.frames.len() as u64);
        self.outcomes
            .extend(self.watch.finish(at).map(|done| (done, self.frames.len())));
        (self.outcomes, self.notices)
    }
}

fn now() -> Instant {
    Instant::now()
}

fn seconds(samples: u64) -> f64 {
    f64::from(u32::try_from(samples).expect("a recording is shorter than 74 hours"))
        / f64::from(SAMPLE_RATE)
}

/// What the engine's logic did over all recordings.
#[derive(Default)]
struct Judged {
    outcomes: BTreeMap<&'static str, u32>,
    /// Milliseconds from a heard phrase's end to the final that covered it.
    covering: Vec<f64>,
    /// Notices at or before the end of the audio, and only in the silence added after it.
    notices: u32,
    notices_in_tail: u32,
}

impl Judged {
    fn add(&mut self, outcomes: &[(Resolution, usize)], notices: &[usize], real_frames: usize) {
        for (done, _) in outcomes {
            *self.outcomes.entry(done.outcome.name()).or_default() += 1;
            if matches!(done.outcome, Outcome::Agreed | Outcome::AgreedLate) {
                self.covering.extend(
                    done.covering_after
                        .map(|after| after.as_secs_f64() * 1000.0),
                );
            }
        }
        for at in notices {
            if *at < real_frames {
                self.notices += 1;
            } else {
                self.notices_in_tail += 1;
            }
        }
    }

    fn print(&self, hours: f64) {
        println!("hits by outcome: {:?}", self.outcomes);
        println!(
            "notices: {} within the recording ({:.1}/h), {} only in the silence added after it",
            self.notices,
            f64::from(self.notices) / hours,
            self.notices_in_tail,
        );
        let mut delays = self.covering.clone();
        println!(
            "covering final after the phrase ended, for agreed hits: n {}  min {:.0}  median {:.0}  p90 {:.0}  max {:.0} ms",
            delays.len(),
            percentile(&mut delays, 0).unwrap_or(0.0),
            percentile(&mut delays, 50).unwrap_or(0.0),
            percentile(&mut delays, 90).unwrap_or(0.0),
            percentile(&mut delays, 100).unwrap_or(0.0),
        );
    }
}

/// What the spotter did over all recordings.
#[derive(Default)]
struct Scores {
    classes: BTreeMap<String, Class>,
    files: u32,
    skipped: u32,
    with_text: u32,
    samples: u64,
    pushes: Vec<f64>,
    /// Where spotter-only hits are: a kind, a recording, seconds, the phrase and its confidence.
    places: String,
}

impl Scores {
    /// Counts one recording's hits against the endings its transcript carried.
    fn count(
        &mut self,
        endings: &Endings,
        label: &str,
        samples: u64,
        hits: &[(usize, SpotterHit)],
        said: Option<&[String]>,
    ) {
        let mut by_phrase: BTreeMap<&str, Vec<&SpotterHit>> = BTreeMap::new();
        for (_, hit) in hits {
            by_phrase.entry(hit.phrase.as_str()).or_default().push(hit);
        }
        for (phrase, _) in endings.entries() {
            let said_here = said.map_or(0, |found| {
                found.iter().filter(|name| name.as_str() == phrase).count()
            });
            let heard = by_phrase.get(phrase).map_or(&[][..], Vec::as_slice);
            let agreed = said_here.min(heard.len());
            let class = self.classes.entry(phrase.to_owned()).or_default();
            class.literal += u32::try_from(said_here).expect("small");
            class.agreed += u32::try_from(agreed).expect("small");
            let (only, agreeing) = heard.split_at(heard.len() - agreed);
            class
                .agreed_confidence
                .extend(agreeing.iter().map(|hit| hit.confidence));
            for hit in only {
                class.only_confidence.push(hit.confidence);
                let tail = hit.end_sample + TAIL >= samples;
                if tail {
                    class.only_tail += 1;
                } else {
                    class.only_body += 1;
                }
                let _ = writeln!(
                    self.places,
                    "{}\t{label}\t{:.1}\t{phrase}\t{:.2}",
                    if tail {
                        "spotter_only_tail"
                    } else {
                        "spotter_only_body"
                    },
                    seconds(hit.start_sample),
                    hit.confidence,
                );
            }
        }
    }

    fn print(&self, spotter_load: Duration) {
        let hours = seconds(self.samples) / 3600.0;
        println!(
            "recordings {} (skipped {}, with transcripts {})  audio {:.1} min  spotter load {} ms  thresholds {THRESHOLDS:?}",
            self.files,
            self.skipped,
            self.with_text,
            hours * 60.0,
            spotter_load.as_millis(),
        );
        println!(
            "phrase | literal agreed (recall) [agreed confidence min/med/max] | spotter-only (body+tail) per hour [confidence min/med/max] left at thresholds"
        );
        for (phrase, class) in &self.classes {
            let only = class.only_body + class.only_tail;
            let recall = if class.literal == 0 {
                "n/a".to_owned()
            } else {
                format!(
                    "{:.0}%",
                    100.0 * f64::from(class.agreed) / f64::from(class.literal)
                )
            };
            println!(
                "{phrase:>16} | {} {} ({recall}) [{}] | {only} ({}+{}) {:.1}/h [{}] {}",
                class.literal,
                class.agreed,
                spread(&class.agreed_confidence),
                class.only_body,
                class.only_tail,
                f64::from(only) / hours,
                spread(&class.only_confidence),
                at_least(&class.only_confidence),
            );
        }
        let mut sorted = self.pushes.clone();
        let frames = f64::from(u32::try_from(self.pushes.len()).expect("fewer than 2^32 frames"));
        println!(
            "spotter per frame: n {}  mean {:.2} ms  p99 {:.2} ms  step_max_ms {:.2}",
            self.pushes.len(),
            self.pushes.iter().sum::<f64>() / frames.max(1.0),
            percentile(&mut sorted, 99).unwrap_or(0.0),
            percentile(&mut sorted, 100).unwrap_or(0.0),
        );
    }
}

async fn speech_model(store: &ModelStore) -> Option<Transducer> {
    let choice = std::env::var("THALVI_SPEECH_MODEL").ok()?;
    let choice = CatalogSpeech::named(&choice).map_or_else(
        || SpeechChoice::Folder(PathBuf::from(&choice)),
        SpeechChoice::Catalog,
    );
    let source = TransducerSource::new(store.clone(), choice, &WordList::default());
    Some(source.load().await.expect("speech model loads"))
}

#[tokio::test]
#[ignore = "needs recordings in THALVI_RECORDINGS_DIR and network for the model files"]
async fn false_alarms_recall_and_notices_on_recordings() {
    let root =
        PathBuf::from(std::env::var("THALVI_RECORDINGS_DIR").expect("THALVI_RECORDINGS_DIR"));
    let limit = std::env::var("THALVI_SPOT_LIMIT")
        .ok()
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    let endings = Endings::default();
    let store = store();
    let loaded = Instant::now();
    let mut spotter = OnnxSpotter::load(&endings, &store)
        .await
        .expect("spotter loads");
    let spotter_load = loaded.elapsed();
    let model = speech_model(&store).await;

    let mut scores = Scores::default();
    let mut judged = Judged::default();
    for path in recordings(&root).into_iter().take(limit) {
        let Ok(samples) = read_wav(&path) else {
            scores.skipped += 1;
            continue;
        };
        let label = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let frames = frames_of(&samples);
        let length = samples.len() as u64;
        scores.files += 1;
        scores.samples += length;
        spotter.reset();
        let mut hits: Vec<(usize, SpotterHit)> = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let started = Instant::now();
            let hit = spotter.push(frame).expect("frame is spotted");
            scores.pushes.push(started.elapsed().as_secs_f64() * 1000.0);
            hits.extend(hit.map(|hit| (index, hit)));
        }
        let said = heard(&path).map(|finals| literal_phrases(&endings, &finals));
        scores.with_text += u32::from(said.is_some());
        scores.count(&endings, &label, length, &hits, said.as_deref());
        if let Some(model) = &model {
            let mut mirror = Mirror::<Transducer>::new(model, &frames);
            for index in 0..frames.len() {
                let hit = hits.iter().find(|(at, _)| *at == index).map(|(_, hit)| hit);
                mirror.frame(index, hit);
            }
            let (outcomes, notices) = mirror.finish();
            judged.add(&outcomes, &notices, samples.len().div_ceil(FRAME_SAMPLES));
        }
    }
    scores.print(spotter_load);
    if model.is_some() {
        judged.print(seconds(scores.samples) / 3600.0);
    }
    if let Ok(out) = std::env::var("THALVI_SPOT_OUT") {
        std::fs::write(out, scores.places).expect("places are written");
    }
}

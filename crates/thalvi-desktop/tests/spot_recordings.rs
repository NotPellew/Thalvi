//! The ending spotter on recorded speech: false alarms and recall per ending, and how often hearing a disputed ending again rescues it.
//!
//! Needs `THALVI_RECORDINGS_DIR`, a folder tree of 16 kHz mono 16-bit WAVs. Where a folder has a
//! `speech.wav` with a `text.txt` beside it (the desktop's own recordings), the `heard:` lines say
//! which endings the transcript had; any other tree is scored without transcripts, so every hit
//! counts as a candidate false alarm. `THALVI_SPOT_MODELS` is where the models are downloaded to
//! (default: the test target's `models`), `THALVI_SPOT_OUT` a file for the places of spotter-only
//! hits, and `THALVI_SPOT_LIMIT` caps the number of recordings. Setting `THALVI_SPEECH_MODEL` to a
//! catalog name or a model folder adds phase B: the local recognizer's finals decide which hits the
//! transcript disputes, and each is transcribed again from the last final before the hit.
//!
//! Prints counts and durations only, never texts. Run with `--release --ignored --nocapture`.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thalvi_desktop::capture::{CaptureSession, Ending, EndingPauses, Endings, FinalStep};
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame, SAMPLE_RATE};
use thalvi_desktop::local::{ModelSource, SpeechModel, SpeechStream};
use thalvi_desktop::models::{CatalogSpeech, ModelStore};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::RecognizerEvent;
use thalvi_desktop::spotter::{KeywordSpotter, OnnxSpotter, SpotterHit};
use thalvi_desktop::transducer::{SpeechChoice, Transducer, TransducerSource};

/// The 800 ms of silence after a capture that lets the recognizer finish and endpoint.
const RELEASE_FRAMES: usize = 10;
/// A hit this close to the end of a recording, or nearer, is where an ending is said.
const TAIL: u64 = 4 * SAMPLE_RATE as u64;
/// How long the engine lets a recheck run: a live final that carries the ending in this time ends it.
const RECHECK_LIMIT: u64 = 5 * SAMPLE_RATE as u64;
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
        samples.len().div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES + RELEASE_FRAMES * FRAME_SAMPLES,
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
    CaptureSession::new(Endings::default(), EndingPauses::default())
}

/// What literal endings the transcript of one capture carried, by phrase.
fn literal_phrases(endings: &Endings, finals: &[String]) -> Vec<String> {
    let mut capture = session();
    let mut found = Vec::new();
    for text in finals {
        let before = capture.clone();
        let step = capture.on_final(text, tokio::time::Instant::now(), false);
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

/// One recheck: a disputed hit and what hearing its audio again gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Redo {
    /// The new transcript ends with the hit's ending.
    Rescued,
    /// The new transcript says the same words again, with no ending.
    Repeated,
    /// The new transcript ends with a different ending.
    OtherEnding,
    /// The new transcript has different words and no ending.
    OtherWords,
    /// The recognizer gave nothing to apply.
    Nothing,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about one recheck, each a line of the report"
)]
struct Recheck {
    redo: Redo,
    /// No final covered the phrase, so the recheck started after the release silence.
    backstop: bool,
    tail: bool,
    /// The user said the same ending literally later in the capture.
    said_again: bool,
    /// The live transcript carried the ending itself while the recheck would still have run.
    agreed_late: bool,
    /// Where the recheck started, in samples of the recording.
    at: u64,
    millis: f64,
    audio_seconds: f64,
}

/// A hit waiting for the final that covers it.
struct Claim {
    ending: Ending,
    end: u64,
    /// How many finals had arrived by the first token of the phrase.
    anchor: usize,
}

struct Final {
    arrived: u64,
    text: String,
    ending: Option<Ending>,
}

/// The claim rules of the engine, applied to one recording as it streams through a speech model.
struct Replay<'a, M: SpeechModel> {
    model: &'a M,
    endings: &'a Endings,
    frames: &'a [Frame],
    samples: u64,
    stream: M::Stream,
    live: CaptureSession,
    /// The capture after each final, so `sessions[n]` is the state with `n` finals applied.
    sessions: Vec<CaptureSession>,
    finals: Vec<Final>,
    claim: Option<Claim>,
    /// Each recheck with the ending it was for and the index of the first final after the disputed ones.
    rechecks: Vec<(Recheck, Ending, usize)>,
    agreed: u32,
    conflicts: u32,
}

fn normalized(texts: impl Iterator<Item = String>) -> Vec<String> {
    words(&texts.collect::<Vec<_>>().join(" "))
}

fn now() -> tokio::time::Instant {
    tokio::time::Instant::now()
}

impl<'a, M: SpeechModel> Replay<'a, M> {
    fn new(model: &'a M, endings: &'a Endings, frames: &'a [Frame], samples: u64) -> Self {
        Self {
            model,
            endings,
            frames,
            samples,
            stream: model.stream(),
            live: session(),
            sessions: vec![session()],
            finals: Vec::new(),
            claim: None,
            rechecks: Vec::new(),
            agreed: 0,
            conflicts: 0,
        }
    }

    /// Takes frame `index` into the recognizer, then the spotter's hit on it, if any.
    fn frame(&mut self, index: usize, hit: Option<&SpotterHit>) {
        let sent = (index as u64 + 1) * FRAME_SAMPLES as u64;
        for event in self
            .stream
            .accept(&self.frames[index])
            .expect("frame is recognized")
        {
            if let RecognizerEvent::Final(text) = event {
                let before = self.live.clone();
                let step = self.live.on_final(&text, now(), false);
                self.finals.push(Final {
                    arrived: sent,
                    ending: literal(self.endings, &before, &text, &step).map(|(_, ending)| ending),
                    text,
                });
                self.sessions.push(self.live.clone());
            }
        }
        if let Some(hit) = hit {
            self.hear(hit);
        }
        self.settle(index, sent);
    }

    fn hear(&mut self, hit: &SpotterHit) {
        match &mut self.claim {
            Some(open) => {
                open.ending = hit.ending;
                open.end = hit.end_sample;
            }
            None => {
                self.claim = Some(Claim {
                    ending: hit.ending,
                    end: hit.end_sample,
                    anchor: self
                        .finals
                        .iter()
                        .take_while(|done| done.arrived <= hit.start_sample)
                        .count(),
                });
            }
        }
    }

    /// Judges the open claim by the first final that covers it and changed the capture.
    fn settle(&mut self, index: usize, sent: u64) {
        let Some(open) = &self.claim else { return };
        let covering = self
            .finals
            .iter()
            .enumerate()
            .skip(open.anchor)
            .filter(|(_, done)| done.arrived >= open.end)
            .find(|(at, done)| {
                done.ending.is_some() || self.sessions[at + 1].text() != self.sessions[*at].text()
            })
            .map(|(at, _)| at);
        let backstop =
            covering.is_none() && sent >= open.end + (RELEASE_FRAMES * FRAME_SAMPLES) as u64;
        match covering.map(|at| self.finals[at].ending) {
            Some(Some(ending)) if ending == open.ending => {
                self.agreed += 1;
                self.claim = None;
            }
            Some(Some(_)) => {
                self.conflicts += 1;
                self.claim = None;
            }
            None if !backstop => {}
            Some(None) | None => self.recheck(index, covering, backstop),
        }
    }

    /// Transcribes the audio since the anchor again, in a fresh stream, and applies the result.
    fn recheck(&mut self, index: usize, covering: Option<usize>, backstop: bool) {
        let open = self.claim.take().expect("a claim is open");
        let anchor_sample = open
            .anchor
            .checked_sub(1)
            .map_or(0, |last| self.finals[last].arrived);
        let first = usize::try_from(anchor_sample / FRAME_SAMPLES as u64).expect("fits");
        let mut audio: Vec<Frame> = self.frames[first..=index].to_vec();
        audio.extend(std::iter::repeat_n([0; FRAME_SAMPLES], RELEASE_FRAMES));
        let started = Instant::now();
        let mut stream = self.model.stream();
        let mut again = Vec::new();
        for frame in &audio {
            for event in stream.accept(frame).expect("frame is recognized") {
                if let RecognizerEvent::Final(text) = event {
                    again.push(text);
                }
            }
        }
        let millis = started.elapsed().as_secs_f64() * 1000.0;
        let mut redone = self.sessions[open.anchor].clone();
        let mut said = None;
        for text in &again {
            let before = redone.clone();
            let step = redone.on_final(text, now(), false);
            if let Some((_, ending)) = literal(self.endings, &before, text, &step) {
                said = Some(ending);
            }
        }
        let later_from = covering.map_or(self.finals.len(), |at| at + 1);
        let disputed = normalized(
            self.finals[open.anchor..later_from]
                .iter()
                .map(|done| done.text.clone()),
        );
        let redo = match said {
            Some(ending) if ending == open.ending => Redo::Rescued,
            Some(_) => Redo::OtherEnding,
            None if again.is_empty() => Redo::Nothing,
            None if normalized(again.into_iter()) == disputed => Redo::Repeated,
            None => Redo::OtherWords,
        };
        self.rechecks.push((
            Recheck {
                redo,
                backstop,
                tail: open.end + TAIL >= self.samples,
                said_again: false,
                agreed_late: false,
                at: (index as u64 + 1) * FRAME_SAMPLES as u64,
                millis,
                audio_seconds: seconds(audio.len() as u64 * FRAME_SAMPLES as u64),
            },
            open.ending,
            later_from,
        ));
    }

    /// The rechecks, each told whether the capture's later finals said its ending literally.
    fn finish(self) -> (Vec<Recheck>, u32, u32) {
        let finals = self.finals;
        let rechecks = self
            .rechecks
            .into_iter()
            .map(|(mut check, ending, later_from)| {
                let mut later = finals
                    .iter()
                    .skip(later_from)
                    .filter(|done| done.ending == Some(ending));
                let first = later.next();
                check.said_again = first.is_some();
                check.agreed_late =
                    first.is_some_and(|done| done.arrived <= check.at + RECHECK_LIMIT);
                check
            })
            .collect();
        (rechecks, self.agreed, self.conflicts)
    }
}

fn seconds(samples: u64) -> f64 {
    f64::from(u32::try_from(samples).expect("a recording is shorter than 74 hours"))
        / f64::from(SAMPLE_RATE)
}

fn describe(redos: &[&Recheck]) -> String {
    let count = |wanted: Redo| redos.iter().filter(|check| check.redo == wanted).count();
    format!(
        "rechecks {}  rescued {}  repeated {}  other ending {}  other words {}  nothing {}",
        redos.len(),
        count(Redo::Rescued),
        count(Redo::Repeated),
        count(Redo::OtherEnding),
        count(Redo::OtherWords),
        count(Redo::Nothing),
    )
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

/// Which rechecks a line of the report is about.
type Keep = fn(&Recheck) -> bool;

fn print_rechecks(rechecks: &[Recheck], agreed: u32, other: u32) {
    let all: Vec<&Recheck> = rechecks.iter().collect();
    println!(
        "phase B hits judged: agreed {agreed}  other ending {other}  disputed {}",
        all.len()
    );
    println!("all disputed: {}", describe(&all));
    let subsets: [(&str, Keep); 7] = [
        ("live transcript agreed late", |check| check.agreed_late),
        ("not agreed late (the transcript never had it)", |check| {
            !check.agreed_late
        }),
        ("not agreed late, in the tail", |check| {
            !check.agreed_late && check.tail
        }),
        ("not agreed late, said again by the user", |check| {
            !check.agreed_late && check.said_again
        }),
        ("tail", |check| check.tail),
        ("said again later", |check| check.said_again),
        ("backstop", |check| check.backstop),
    ];
    for (name, keep) in subsets {
        let subset: Vec<&Recheck> = rechecks.iter().filter(|check| keep(check)).collect();
        println!("{name}: {}", describe(&subset));
    }
    let mut millis: Vec<f64> = rechecks.iter().map(|check| check.millis).collect();
    let audio: f64 = rechecks.iter().map(|check| check.audio_seconds).sum();
    println!(
        "recheck compute: median {:.0} ms  max {:.0} ms  audio {audio:.0} s in total",
        percentile(&mut millis, 50).unwrap_or(0.0),
        percentile(&mut millis, 100).unwrap_or(0.0),
    );
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
async fn false_alarms_recall_and_rechecks_on_recordings() {
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
    let mut rechecks: Vec<Recheck> = Vec::new();
    let (mut agreed, mut other) = (0, 0);
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
        if let (Some(model), Some(_)) = (&model, &said) {
            let mut replay = Replay::new(model, &endings, &frames, length);
            for index in 0..frames.len() {
                let hit = hits.iter().find(|(at, _)| *at == index).map(|(_, hit)| hit);
                replay.frame(index, hit);
            }
            let (found, agreeing, conflicting) = replay.finish();
            rechecks.extend(found);
            agreed += agreeing;
            other += conflicting;
        }
    }
    scores.print(spotter_load);
    if model.is_some() {
        print_rechecks(&rechecks, agreed, other);
    }
    if let Ok(out) = std::env::var("THALVI_SPOT_OUT") {
        std::fs::write(out, scores.places).expect("places are written");
    }
}

//! Opt-in local metrics and the recordings of each dictation, written off the capture path.

use std::fmt::{self, Display};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::report::UserError;

/// How many events may wait for the writer before new ones are dropped.
pub const QUEUE_CAPACITY: usize = 256;

/// The size at which `metrics.jsonl` rotates; with two backups it holds at most 15 MiB.
const LOG_LIMIT: u64 = 5 * 1024 * 1024;

const RECORDING_RATE: u32 = 16_000;

/// Which kinds of data are recorded; all are off by default because they are personal data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordKinds {
    /// Timing and outcome events.
    pub metrics: bool,
    /// What was heard and how each dictation ended, as `text.txt` in the dictation's folder.
    pub text: bool,
    /// Microphone audio of each dictation: `speech.wav` after the wake cue, and `wake.wav` with
    /// about 2 s before the wake.
    pub audio: bool,
}

/// A metrics event. Model and endpoint calls report their real latency; the rest describe what
/// the user perceives and are never merged with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    WakeLoad,
    /// Every minute of wake listening: how long scoring a frame took, and how long the microphone
    /// heard sound or quiet.
    WakeInference,
    /// A wake head's score rose near its threshold and fell back without crossing it.
    WakeNearMiss,
    /// A wake that led to no delivery: the dictation's folder and why.
    WakeNothing,
    WakeReset,
    TranscriptionReady,
    SecondaryStt,
    /// Fetching and loading the local speech model.
    SpeechLoad,
    /// Downloading a catalog speech model from the settings window.
    SpeechDownload,
    /// Asking the server about its word list (`check`) or saving the list there (`share`).
    WordList,
    /// One local recognition: its engine, device, model latency and outcome.
    LocalStt,
    ReadyCue,
    Cue,
    EndingState,
    EndingPause,
    EndCue,
    CaptureFlow,
    /// The dictation key went down: it started a capture, or was refused and why.
    HoldStart,
    /// The dictation key's capture ended; the duration is how long the key was down, or until
    /// the capture ended when a spoken ending came first.
    HoldEnd,
    /// One call to the desktop host; fields hold only the call's name.
    HostCall,
    /// Microphone audio was lost: `xrun` for a discontinuity the system reported, `queue_full`
    /// when the engine stopped reading for ten seconds.
    AudioGap,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Self::WakeLoad => "desktop_wake_load",
            Self::WakeInference => "desktop_wake_inference",
            Self::WakeNearMiss => "desktop_wake_near_miss",
            Self::WakeNothing => "desktop_wake_nothing",
            Self::WakeReset => "desktop_wake_reset",
            Self::TranscriptionReady => "desktop_transcription_ready",
            Self::SecondaryStt => "desktop_secondary_stt",
            Self::SpeechLoad => "desktop_speech_load",
            Self::SpeechDownload => "desktop_speech_download",
            Self::WordList => "desktop_word_list",
            Self::LocalStt => "desktop_local_stt",
            Self::ReadyCue => "desktop_ready_cue",
            Self::Cue => "desktop_cue",
            Self::EndingState => "desktop_ending_state",
            Self::EndingPause => "desktop_ending_pause",
            Self::EndCue => "desktop_end_cue",
            Self::CaptureFlow => "desktop_capture_flow",
            Self::HoldStart => "desktop_hold_start",
            Self::HoldEnd => "desktop_hold_end",
            Self::HostCall => "desktop_host_call",
            Self::AudioGap => "desktop_audio_gap",
        }
    }
}

/// Why recording could not start.
#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("This system has no per-user data directory to record into.")]
    NoDataDir,
    #[error("The recording writer could not start: {0}")]
    Writer(#[source] io::Error),
}

impl UserError for RecorderError {
    fn headline(&self) -> &'static str {
        match self {
            Self::NoDataDir => "No folder to record into",
            Self::Writer(_) => "Recording can't start",
        }
    }
}

enum Message {
    Metric(String),
    Text {
        dictation: Dictation,
        line: String,
    },
    Audio {
        dictation: Dictation,
        name: &'static str,
        pcm: Vec<i16>,
    },
    Shutdown,
}

/// The folder of one wake's recordings: `<date>/<time>` in local time, with `-2`, `-3` for
/// wakes within the same second. Nothing exists on disk until a file is written into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dictation(String);

impl Display for Dictation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names dictation folders by the second they began.
#[derive(Default)]
struct Names {
    last: String,
    repeats: u32,
}

impl Names {
    fn next<Tz: TimeZone>(&mut self, at: &DateTime<Tz>) -> Dictation
    where
        Tz::Offset: Display,
    {
        let name = at.format("%Y-%m-%d/%H-%M-%S").to_string();
        if name == self.last {
            self.repeats += 1;
            return Dictation(format!("{name}-{}", self.repeats + 1));
        }
        self.last.clone_from(&name);
        self.repeats = 0;
        Dictation(name)
    }
}

/// The kinds that are on, shared by a recorder and its handles so a change reaches them all.
#[derive(Default)]
struct LiveKinds {
    metrics: AtomicBool,
    text: AtomicBool,
    audio: AtomicBool,
}

impl LiveKinds {
    fn get(&self) -> RecordKinds {
        RecordKinds {
            metrics: self.metrics.load(Ordering::Relaxed),
            text: self.text.load(Ordering::Relaxed),
            audio: self.audio.load(Ordering::Relaxed),
        }
    }

    fn set(&self, kinds: RecordKinds) {
        self.metrics.store(kinds.metrics, Ordering::Relaxed);
        self.text.store(kinds.text, Ordering::Relaxed);
        self.audio.store(kinds.audio, Ordering::Relaxed);
    }
}

/// Records enabled kinds to the per-user data directory on a writer thread. Recording never
/// blocks the caller: when the writer falls behind, new events are dropped and counted.
pub struct Recorder {
    kinds: Arc<LiveKinds>,
    sender: Option<SyncSender<Message>>,
    writer: Option<JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
    names: Mutex<Names>,
}

impl Recorder {
    /// A recorder that records nothing and creates nothing; [`set_kinds`](Self::set_kinds) does
    /// not change that.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            kinds: Arc::default(),
            sender: None,
            writer: None,
            dropped: Arc::default(),
            names: Mutex::default(),
        }
    }

    /// Records into [`dir`](Self::dir); nothing is created until a kind is on and written.
    ///
    /// # Errors
    /// Fails when the platform has no per-user data directory or the writer cannot start.
    pub fn per_user(kinds: RecordKinds) -> Result<Self, RecorderError> {
        Self::in_dir(kinds, Self::dir()?)
    }

    /// The folder [`per_user`](Self::per_user) records into. It holds nothing but recordings.
    ///
    /// # Errors
    /// Fails when the platform has no per-user data directory.
    pub fn dir() -> Result<PathBuf, RecorderError> {
        Ok(dirs::data_local_dir()
            .ok_or(RecorderError::NoDataDir)?
            .join("thalvi")
            .join("recordings"))
    }

    /// Records into `root`; files and directories appear on the first event written.
    ///
    /// # Errors
    /// Fails when the writer thread cannot start.
    pub fn in_dir(kinds: RecordKinds, root: impl Into<PathBuf>) -> Result<Self, RecorderError> {
        Self::start(kinds, root.into(), None)
    }

    fn start(
        kinds: RecordKinds,
        root: PathBuf,
        gate: Option<Receiver<()>>,
    ) -> Result<Self, RecorderError> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let writer = std::thread::Builder::new()
            .name("thalvi-recorder".to_owned())
            .spawn(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
                Writer::new(root).run(&receiver);
            })
            .map_err(RecorderError::Writer)?;
        let live = LiveKinds::default();
        live.set(kinds);
        Ok(Self {
            kinds: Arc::new(live),
            sender: Some(sender),
            writer: Some(writer),
            dropped: Arc::default(),
            names: Mutex::default(),
        })
    }

    /// Which kinds are on, so callers can skip work that only feeds a recording.
    #[must_use]
    pub fn kinds(&self) -> RecordKinds {
        self.kinds.get()
    }

    /// Switches kinds on or off for this recorder and the [`Metrics`] handles made from it.
    /// Events queued so far are written as they were queued. A caller that records a unit of work
    /// as a whole changes the kinds only between units.
    pub fn set_kinds(&self, kinds: RecordKinds) {
        if self.sender.is_some() {
            self.kinds.set(kinds);
        }
    }

    /// How many events were dropped because the writer fell behind.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues one metrics line: time, event, `outcome`, the optional `duration` in milliseconds,
    /// and the keys of `fields` when it is a JSON object (`Value::Null` for none).
    pub fn metric(&self, event: Event, outcome: &str, duration: Option<Duration>, fields: Value) {
        self.metrics().metric(event, outcome, duration, fields);
    }

    /// A handle that records metrics from other threads, for as long as the writer runs.
    #[must_use]
    pub fn metrics(&self) -> Metrics {
        Metrics {
            kinds: Arc::clone(&self.kinds),
            sender: self.sender.clone(),
            dropped: Arc::clone(&self.dropped),
        }
    }

    /// Names the folder for the dictation that begins now.
    pub fn begin(&self) -> Dictation {
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next(&Local::now())
    }

    /// Queues one line for the dictation's `text.txt`.
    pub fn text(&self, dictation: &Dictation, line: String) {
        if self.kinds().text {
            self.send(Message::Text {
                dictation: dictation.clone(),
                line,
            });
        }
    }

    /// Queues the 16 kHz mono audio heard up to the wake as the dictation's `wake.wav`.
    pub fn wake_audio(&self, dictation: &Dictation, pcm: Vec<i16>) {
        self.audio(dictation, "wake.wav", pcm);
    }

    /// Queues the 16 kHz mono audio of the capture as the dictation's `speech.wav`.
    pub fn speech(&self, dictation: &Dictation, pcm: Vec<i16>) {
        self.audio(dictation, "speech.wav", pcm);
    }

    fn audio(&self, dictation: &Dictation, name: &'static str, pcm: Vec<i16>) {
        if self.kinds().audio {
            self.send(Message::Audio {
                dictation: dictation.clone(),
                name,
                pcm,
            });
        }
    }

    fn send(&self, message: Message) {
        send(self.sender.as_ref(), &self.dropped, message);
    }

    /// Writes everything queued so far and stops the writer; call once at shutdown.
    pub async fn finish(mut self) {
        let dropped = self.dropped();
        if dropped > 0 {
            tracing::info!(dropped, "recording events dropped");
        }
        if let (Some(sender), Some(writer)) = (self.sender.take(), self.writer.take()) {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = sender.send(Message::Shutdown);
                let _ = writer.join();
            })
            .await;
        }
    }
}

fn send(sender: Option<&SyncSender<Message>>, dropped: &AtomicU64, message: Message) {
    let Some(sender) = sender else {
        return;
    };
    if sender.try_send(message).is_err() && dropped.fetch_add(1, Ordering::Relaxed) == 0 {
        tracing::warn!("recording events are being dropped because the writer is behind");
    }
}

/// Records metrics like [`Recorder::metric`] from a thread that does not own the recorder.
#[derive(Clone)]
pub struct Metrics {
    kinds: Arc<LiveKinds>,
    sender: Option<SyncSender<Message>>,
    dropped: Arc<AtomicU64>,
}

impl Metrics {
    /// Queues one metrics line: time, event, `outcome`, the optional `duration` in milliseconds,
    /// and the keys of `fields` when it is a JSON object (`Value::Null` for none).
    pub fn metric(&self, event: Event, outcome: &str, duration: Option<Duration>, fields: Value) {
        if !self.kinds.get().metrics {
            return;
        }
        let mut line = json!({"ts_ms": now_ms(), "event": event.name(), "outcome": outcome});
        if let Some(duration) = duration {
            line["duration_ms"] = json!(duration.as_secs_f64() * 1000.0);
        }
        send(
            self.sender.as_ref(),
            &self.dropped,
            Message::Metric(finish_line(line, fields)),
        );
    }
}

fn finish_line(mut line: Value, fields: Value) -> String {
    if let (Some(target), Value::Object(extra)) = (line.as_object_mut(), fields) {
        target.extend(extra);
    }
    line.to_string()
}

struct Writer {
    root: PathBuf,
    metrics: Option<(File, u64)>,
    failed: bool,
}

impl Writer {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            metrics: None,
            failed: false,
        }
    }

    fn run(&mut self, receiver: &Receiver<Message>) {
        for message in receiver {
            let result = match message {
                Message::Shutdown => break,
                Message::Metric(line) => self.metric(&line),
                Message::Text { dictation, line } => self.text(&dictation, &line),
                Message::Audio {
                    dictation,
                    name,
                    pcm,
                } => self.audio(&dictation, name, &pcm),
            };
            if let Err(error) = result
                && !self.failed
            {
                self.failed = true;
                tracing::warn!(%error, "recording write failed; later failures are not reported");
            }
        }
    }

    fn metric(&mut self, line: &str) -> io::Result<()> {
        let path = self.root.join("metrics.jsonl");
        let bytes = u64::try_from(line.len() + 1).unwrap_or(u64::MAX);
        let (mut file, mut size) = if let Some(open) = self.metrics.take() {
            open
        } else {
            create_private_dir(&self.root)?;
            let file = open_private(&path)?;
            let size = file.metadata()?.len();
            (file, size)
        };
        if size + bytes > LOG_LIMIT {
            drop(file);
            let first = backup(&path, 1);
            if first.exists() {
                std::fs::rename(&first, backup(&path, 2))?;
            }
            std::fs::rename(&path, first)?;
            file = open_private(&path)?;
            size = 0;
        }
        writeln!(file, "{line}")?;
        self.metrics = Some((file, size + bytes));
        Ok(())
    }

    fn text(&self, dictation: &Dictation, line: &str) -> io::Result<()> {
        let dir = self.folder(dictation)?;
        writeln!(open_private(&dir.join("text.txt"))?, "{line}")
    }

    fn audio(&self, dictation: &Dictation, name: &str, pcm: &[i16]) -> io::Result<()> {
        let dir = self.folder(dictation)?;
        open_private(&dir.join(name))?.write_all(&wav(pcm))
    }

    fn folder(&self, dictation: &Dictation) -> io::Result<PathBuf> {
        let dir = self.root.join(&dictation.0);
        create_private_dir(&dir)?;
        Ok(dir)
    }
}

fn backup(path: &Path, number: u8) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{number}"));
    PathBuf::from(name)
}

fn open_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    // Windows files inherit the private ACL of the per-user data directory.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// 16-bit mono PCM in a WAV container.
fn wav(pcm: &[i16]) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len() * 2).unwrap_or(u32::MAX);
    let mut bytes = Vec::with_capacity(44 + pcm.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&data_len.saturating_add(36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&RECORDING_RATE.to_le_bytes());
    bytes.extend_from_slice(&(RECORDING_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in pcm {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("thalvi-metrics-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        /// The root the recorder will create on first write.
        fn root(&self) -> PathBuf {
            self.0.join("recordings")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ALL: RecordKinds = RecordKinds {
        metrics: true,
        text: true,
        audio: true,
    };

    const METRICS: RecordKinds = RecordKinds {
        metrics: true,
        text: false,
        audio: false,
    };

    fn lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Every file and directory under `root`, relative, with the date and time elided.
    fn tree(root: &Path) -> BTreeSet<String> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries {
                let entry = entry.unwrap();
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if name.len() == 10 && name.as_bytes()[4] == b'-' {
                    name = "<date>".to_owned();
                } else if name.len() == 8 && name.as_bytes()[2] == b'-' {
                    name = "<time>".to_owned();
                }
                let path = format!("{prefix}{name}");
                out.insert(path.clone());
                walk(&entry.path(), &format!("{path}/"), out);
            }
        }
        let mut out = BTreeSet::new();
        walk(root, "", &mut out);
        out
    }

    fn emit_everything(recorder: &Recorder) {
        let dictation = recorder.begin();
        recorder.metric(Event::CaptureFlow, "ok", None, Value::Null);
        recorder.text(&dictation, "heard: hello".to_owned());
        recorder.wake_audio(&dictation, vec![4]);
        recorder.speech(&dictation, vec![1, 2, 3]);
    }

    /// The one dictation folder under `root`.
    fn dictation_dir(root: &Path) -> PathBuf {
        let day = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        std::fs::read_dir(day)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn event_names_keep_model_latency_and_perceived_latency_apart() {
        for (event, name) in [
            (Event::WakeLoad, "desktop_wake_load"),
            (Event::WakeInference, "desktop_wake_inference"),
            (Event::WakeNearMiss, "desktop_wake_near_miss"),
            (Event::WakeNothing, "desktop_wake_nothing"),
            (Event::WakeReset, "desktop_wake_reset"),
            (Event::TranscriptionReady, "desktop_transcription_ready"),
            (Event::SecondaryStt, "desktop_secondary_stt"),
            (Event::SpeechLoad, "desktop_speech_load"),
            (Event::SpeechDownload, "desktop_speech_download"),
            (Event::WordList, "desktop_word_list"),
            (Event::LocalStt, "desktop_local_stt"),
            (Event::ReadyCue, "desktop_ready_cue"),
            (Event::Cue, "desktop_cue"),
            (Event::EndingState, "desktop_ending_state"),
            (Event::EndingPause, "desktop_ending_pause"),
            (Event::EndCue, "desktop_end_cue"),
            (Event::CaptureFlow, "desktop_capture_flow"),
            (Event::HoldStart, "desktop_hold_start"),
            (Event::HoldEnd, "desktop_hold_end"),
            (Event::HostCall, "desktop_host_call"),
        ] {
            assert_eq!(event.name(), name);
        }
    }

    #[test]
    fn folders_are_named_by_local_date_and_time_and_repeats_within_a_second_are_numbered() {
        let zone = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
        let at = |second| zone.with_ymd_and_hms(2026, 11, 3, 23, 59, second).unwrap();
        let mut names = Names::default();
        let seen: Vec<_> = [58, 58, 58, 59]
            .into_iter()
            .map(|second| names.next(&at(second)).to_string())
            .collect();
        assert_eq!(
            seen,
            [
                "2026-11-03/23-59-58",
                "2026-11-03/23-59-58-2",
                "2026-11-03/23-59-58-3",
                "2026-11-03/23-59-59",
            ]
        );
    }

    #[tokio::test]
    async fn a_metric_line_carries_time_event_outcome_duration_and_fields() {
        let dir = TempDir::new("line");
        let recorder = Recorder::in_dir(METRICS, dir.root()).unwrap();
        recorder.metric(
            Event::SecondaryStt,
            "timeout",
            Some(Duration::from_micros(1500)),
            json!({"model": "m", "api": "openai"}),
        );
        recorder.metric(Event::EndingState, "pending", None, Value::Null);
        recorder.finish().await;

        let written = lines(&dir.root().join("metrics.jsonl"));
        assert_eq!(written.len(), 2);
        assert!(written[0]["ts_ms"].as_u64().unwrap() > 1_700_000_000_000);
        let mut first = written[0].clone();
        first.as_object_mut().unwrap().remove("ts_ms");
        assert_eq!(
            first,
            json!({
                "event": "desktop_secondary_stt", "outcome": "timeout",
                "duration_ms": 1.5, "model": "m", "api": "openai"
            })
        );
        let mut second = written[1].clone();
        second.as_object_mut().unwrap().remove("ts_ms");
        assert_eq!(
            second,
            json!({"event": "desktop_ending_state", "outcome": "pending"})
        );
    }

    #[tokio::test]
    async fn each_kind_writes_only_its_own_files_and_nothing_is_created_when_all_are_off() {
        let all = [
            "metrics.jsonl",
            "<date>",
            "<date>/<time>",
            "<date>/<time>/speech.wav",
            "<date>/<time>/text.txt",
            "<date>/<time>/wake.wav",
        ];
        for (tag, kinds, expected) in [
            ("off", RecordKinds::default(), set(&[])),
            ("metrics", METRICS, set(&["metrics.jsonl"])),
            (
                "text",
                RecordKinds {
                    text: true,
                    ..RecordKinds::default()
                },
                set(&["<date>", "<date>/<time>", "<date>/<time>/text.txt"]),
            ),
            (
                "audio",
                RecordKinds {
                    audio: true,
                    ..RecordKinds::default()
                },
                set(&[
                    "<date>",
                    "<date>/<time>",
                    "<date>/<time>/speech.wav",
                    "<date>/<time>/wake.wav",
                ]),
            ),
            ("all", ALL, set(&all)),
        ] {
            let dir = TempDir::new(tag);
            let recorder = Recorder::in_dir(kinds, dir.root()).unwrap();
            assert_eq!(recorder.kinds(), kinds);
            emit_everything(&recorder);
            recorder.finish().await;
            assert_eq!(tree(&dir.root()), expected, "{tag}");
        }
    }

    #[tokio::test]
    async fn changed_kinds_apply_to_later_events_only_including_through_earlier_handles() {
        let dir = TempDir::new("live-kinds");
        let recorder = Recorder::in_dir(METRICS, dir.root()).unwrap();
        let handle = recorder.metrics();
        let dictation = recorder.begin();
        handle.metric(Event::CaptureFlow, "before", None, Value::Null);
        recorder.text(&dictation, "before".to_owned());

        recorder.set_kinds(RecordKinds {
            text: true,
            ..RecordKinds::default()
        });
        handle.metric(Event::CaptureFlow, "after", None, Value::Null);
        recorder.text(&dictation, "after".to_owned());
        recorder.finish().await;

        let metrics = lines(&dir.root().join("metrics.jsonl"));
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0]["outcome"], "before");
        let text = std::fs::read_to_string(dictation_dir(&dir.root()).join("text.txt")).unwrap();
        assert_eq!(text, "after\n");
    }

    #[test]
    fn a_disabled_recorder_stays_off_when_kinds_are_set() {
        let recorder = Recorder::disabled();
        recorder.set_kinds(ALL);
        assert_eq!(recorder.kinds(), RecordKinds::default());
    }

    #[tokio::test]
    async fn a_dictation_folder_holds_its_lines_in_order_and_its_audio_as_wav_files() {
        let dir = TempDir::new("recordings");
        let recorder = Recorder::in_dir(ALL, dir.root()).unwrap();
        let dictation = recorder.begin();
        recorder.text(&dictation, "heard: hello".to_owned());
        recorder.text(&dictation, "cancelled".to_owned());
        recorder.speech(&dictation, vec![1, -2, 0x0304]);
        recorder.wake_audio(&dictation, vec![1, -2]);
        recorder.finish().await;

        let folder = dictation_dir(&dir.root());
        assert_eq!(
            std::fs::read_to_string(folder.join("text.txt")).unwrap(),
            "heard: hello\ncancelled\n"
        );
        assert_eq!(
            std::fs::read(folder.join("wake.wav")).unwrap(),
            wav(&[1, -2])
        );
        let wav_path = folder.join("speech.wav");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"RIFF");
        expected.extend_from_slice(&42_u32.to_le_bytes());
        expected.extend_from_slice(b"WAVEfmt ");
        expected.extend_from_slice(&16_u32.to_le_bytes());
        expected.extend_from_slice(&[1, 0, 1, 0]);
        expected.extend_from_slice(&16_000_u32.to_le_bytes());
        expected.extend_from_slice(&32_000_u32.to_le_bytes());
        expected.extend_from_slice(&[2, 0, 16, 0]);
        expected.extend_from_slice(b"data");
        expected.extend_from_slice(&6_u32.to_le_bytes());
        expected.extend_from_slice(&[1, 0, 0xFE, 0xFF, 0x04, 0x03]);
        assert_eq!(std::fs::read(wav_path).unwrap(), expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn directories_are_private_to_the_user_and_files_readable_only_by_them() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("modes");
        let recorder = Recorder::in_dir(ALL, dir.root()).unwrap();
        emit_everything(&recorder);
        recorder.finish().await;

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let folder = dictation_dir(&dir.root());
        assert_eq!(mode(&dir.root()), 0o700);
        assert_eq!(mode(folder.parent().unwrap()), 0o700);
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&dir.root().join("metrics.jsonl")), 0o600);
        for entry in std::fs::read_dir(&folder).unwrap() {
            assert_eq!(mode(&entry.unwrap().path()), 0o600);
        }
    }

    #[tokio::test]
    async fn restarting_appends_to_the_existing_log() {
        let dir = TempDir::new("append");
        for outcome in ["first", "second"] {
            let recorder = Recorder::in_dir(METRICS, dir.root()).unwrap();
            recorder.metric(Event::CaptureFlow, outcome, None, Value::Null);
            recorder.finish().await;
        }
        let outcomes: Vec<_> = lines(&dir.root().join("metrics.jsonl"))
            .iter()
            .map(|line| line["outcome"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(outcomes, ["first", "second"]);
    }

    #[tokio::test]
    async fn the_log_rotates_at_five_mebibytes_and_keeps_two_backups() {
        let dir = TempDir::new("rotate");
        let recorder = Recorder::in_dir(METRICS, dir.root()).unwrap();
        let padding = "x".repeat(4 * 1024 * 1024 + 512 * 1024);
        for n in 1..=4 {
            recorder.metric(
                Event::CaptureFlow,
                "ok",
                None,
                json!({"n": n, "padding": padding}),
            );
        }
        recorder.finish().await;

        let log = dir.root().join("metrics.jsonl");
        for (path, n) in [(log.clone(), 4), (backup(&log, 1), 3), (backup(&log, 2), 2)] {
            assert!(std::fs::metadata(&path).unwrap().len() <= LOG_LIMIT);
            let written = lines(&path);
            assert_eq!(written.len(), 1, "{}", path.display());
            assert_eq!(written[0]["n"], n, "{}", path.display());
        }
        assert!(!backup(&log, 3).exists());
    }

    #[tokio::test]
    async fn an_oversized_log_from_an_earlier_run_is_kept_as_a_backup() {
        let dir = TempDir::new("oversized");
        let log = dir.root().join("metrics.jsonl");
        std::fs::create_dir_all(dir.root()).unwrap();
        let padding = "x".repeat(5 * 1024 * 1024);
        let previous = format!("{}\n", json!({ "padding": padding }));
        std::fs::write(&log, &previous).unwrap();

        let recorder = Recorder::in_dir(METRICS, dir.root()).unwrap();
        recorder.metric(Event::CaptureFlow, "ok", None, Value::Null);
        recorder.finish().await;

        assert_eq!(std::fs::read_to_string(backup(&log, 1)).unwrap(), previous);
        let current = lines(&log);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0]["event"], "desktop_capture_flow");
    }

    #[tokio::test]
    async fn a_writer_that_falls_behind_drops_new_events_instead_of_blocking() {
        let dir = TempDir::new("behind");
        let (release, gate) = mpsc::channel();
        let recorder = Recorder::start(METRICS, dir.root(), Some(gate)).unwrap();
        let extra = 10;
        for n in 0..QUEUE_CAPACITY + extra {
            recorder.metric(Event::WakeInference, "none", None, json!({"n": n}));
        }
        assert_eq!(recorder.dropped(), extra as u64);

        drop(release);
        recorder.finish().await;
        let written = lines(&dir.root().join("metrics.jsonl"));
        assert_eq!(written.len(), QUEUE_CAPACITY);
        assert_eq!(written[QUEUE_CAPACITY - 1]["n"], QUEUE_CAPACITY - 1);
    }
}

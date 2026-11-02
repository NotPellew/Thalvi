//! Opt-in local metrics and capture recordings, written off the capture path.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How many events may wait for the writer before new ones are dropped.
pub const QUEUE_CAPACITY: usize = 256;

/// The size at which `metrics.jsonl` rotates; with two backups it holds at most 15 MiB.
const LOG_LIMIT: u64 = 5 * 1024 * 1024;

const RECORDING_RATE: u32 = 16_000;

/// Which kinds of data are recorded; all are off by default because they are personal data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordKinds {
    /// Timing and outcome events.
    pub metrics: bool,
    /// Dictation text and its outcome.
    pub text: bool,
    /// Microphone audio of the capture, after the wake cue, and about 2 s before each wake.
    pub audio: bool,
}

impl RecordKinds {
    #[must_use]
    pub fn any(self) -> bool {
        self.metrics || self.text || self.audio
    }
}

/// A metrics event. Model and endpoint calls report their real latency; the rest describe what
/// the user perceives and are never merged with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    WakeLoad,
    WakeInference,
    WakeClip,
    WakeReset,
    TranscriptionReady,
    SecondaryStt,
    WakeLevel,
    ReadyCue,
    Cue,
    EndingState,
    EndingPause,
    EndCue,
    CaptureFlow,
    /// One call to the desktop host; fields hold only the call's name.
    HostCall,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Self::WakeLoad => "desktop_wake_load",
            Self::WakeInference => "desktop_wake_inference",
            Self::WakeClip => "desktop_wake_clip",
            Self::WakeReset => "desktop_wake_reset",
            Self::TranscriptionReady => "desktop_transcription_ready",
            Self::SecondaryStt => "desktop_secondary_stt",
            Self::WakeLevel => "desktop_wake_level",
            Self::ReadyCue => "desktop_ready_cue",
            Self::Cue => "desktop_cue",
            Self::EndingState => "desktop_ending_state",
            Self::EndingPause => "desktop_ending_pause",
            Self::EndCue => "desktop_end_cue",
            Self::CaptureFlow => "desktop_capture_flow",
            Self::HostCall => "desktop_host_call",
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

enum Message {
    Metric(String),
    Text {
        ts_ms: u64,
        line: String,
    },
    Audio {
        ts_ms: u64,
        name: String,
        pcm: Vec<i16>,
    },
    Shutdown,
}

/// Records enabled kinds to the per-user data directory on a writer thread. Recording never
/// blocks the caller: when the writer falls behind, new events are dropped and counted.
pub struct Recorder {
    kinds: RecordKinds,
    sender: Option<SyncSender<Message>>,
    writer: Option<JoinHandle<()>>,
    dropped: AtomicU64,
}

impl Recorder {
    /// A recorder that records nothing and creates nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            kinds: RecordKinds::default(),
            sender: None,
            writer: None,
            dropped: AtomicU64::new(0),
        }
    }

    /// Records under `<user data dir>/thalvi`; nothing is started or created when no kind is on.
    ///
    /// # Errors
    /// Fails when the platform has no per-user data directory or the writer cannot start.
    pub fn per_user(kinds: RecordKinds) -> Result<Self, RecorderError> {
        Self::in_dir(kinds, Self::user_dir()?)
    }

    /// The folder [`per_user`](Self::per_user) records into.
    ///
    /// # Errors
    /// Fails when the platform has no per-user data directory.
    pub fn user_dir() -> Result<PathBuf, RecorderError> {
        Ok(dirs::data_local_dir()
            .ok_or(RecorderError::NoDataDir)?
            .join("thalvi"))
    }

    /// Records under `root`; files and directories appear on the first event written.
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
        if !kinds.any() {
            return Ok(Self::disabled());
        }
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
        Ok(Self {
            kinds,
            sender: Some(sender),
            writer: Some(writer),
            dropped: AtomicU64::new(0),
        })
    }

    /// Which kinds are on, so callers can skip work that only feeds a recording.
    #[must_use]
    pub fn kinds(&self) -> RecordKinds {
        self.kinds
    }

    /// How many events were dropped because the writer fell behind.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues one metrics line: time, event, `outcome`, the optional `duration` in milliseconds,
    /// and the keys of `fields` when it is a JSON object (`Value::Null` for none).
    pub fn metric(&self, event: Event, outcome: &str, duration: Option<Duration>, fields: Value) {
        if !self.kinds.metrics {
            return;
        }
        let mut line = json!({"ts_ms": now_ms(), "event": event.name(), "outcome": outcome});
        if let Some(duration) = duration {
            line["duration_ms"] = json!(duration.as_secs_f64() * 1000.0);
        }
        self.send(Message::Metric(finish_line(line, fields)));
    }

    /// Queues one line of dictation text for today's `desktop.jsonl`, with the keys of `fields`.
    pub fn text(&self, event: &str, fields: Value) {
        if !self.kinds.text {
            return;
        }
        let ts_ms = now_ms();
        let line = finish_line(json!({"ts_ms": ts_ms, "event": event}), fields);
        self.send(Message::Text { ts_ms, line });
    }

    /// Queues one capture's 16 kHz mono audio as a WAV file.
    pub fn audio(&self, pcm: Vec<i16>) {
        if self.kinds.audio {
            let ts_ms = now_ms();
            self.send(Message::Audio {
                ts_ms,
                name: format!("capture-{ts_ms}.wav"),
                pcm,
            });
        }
    }

    /// Queues the 16 kHz mono audio heard up to a wake of `kind` as a WAV file, and a metrics line
    /// carrying the file name and the keys of `fields`.
    pub fn wake_clip(&self, kind: &str, pcm: Vec<i16>, fields: Value) {
        if !self.kinds.audio {
            return;
        }
        let ts_ms = now_ms();
        let name = format!("wake-{ts_ms}-{kind}.wav");
        self.send(Message::Audio {
            ts_ms,
            name: name.clone(),
            pcm,
        });
        let mut linked = json!({"file": name});
        if let (Some(target), Value::Object(extra)) = (linked.as_object_mut(), fields) {
            target.extend(extra);
        }
        self.metric(Event::WakeClip, kind, None, linked);
    }

    fn send(&self, message: Message) {
        let Some(sender) = &self.sender else {
            return;
        };
        if sender.try_send(message).is_err() && self.dropped.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::warn!("recording events are being dropped because the writer is behind");
        }
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
                Message::Text { ts_ms, line } => self.text(ts_ms, &line),
                Message::Audio { ts_ms, name, pcm } => self.audio(ts_ms, &name, &pcm),
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

    fn text(&self, ts_ms: u64, line: &str) -> io::Result<()> {
        let dir = self.day_dir(ts_ms)?;
        writeln!(open_private(&dir.join("desktop.jsonl"))?, "{line}")
    }

    fn audio(&self, ts_ms: u64, name: &str, pcm: &[i16]) -> io::Result<()> {
        let dir = self.day_dir(ts_ms)?;
        open_private(&dir.join(name))?.write_all(&wav(pcm))
    }

    fn day_dir(&self, ts_ms: u64) -> io::Result<PathBuf> {
        let dir = self.root.join("recordings").join(utc_date(ts_ms));
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

fn create_private_dir(path: &Path) -> io::Result<()> {
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The UTC calendar date (`YYYY-MM-DD`) of an epoch-millisecond instant.
fn utc_date(epoch_ms: u64) -> String {
    let days = i64::try_from(epoch_ms / 86_400_000).unwrap_or(i64::MAX);
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
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
            self.0.join("thalvi")
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

    /// Every file and directory under `root`, relative, with the date and timestamp elided.
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
                } else if name.starts_with("capture-") {
                    name = "capture-<ts>.wav".to_owned();
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
        recorder.metric(Event::CaptureFlow, "ok", None, Value::Null);
        recorder.text("final", json!({"text": "hello"}));
        recorder.audio(vec![1, 2, 3]);
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn event_names_keep_model_latency_and_perceived_latency_apart() {
        for (event, name) in [
            (Event::WakeLoad, "desktop_wake_load"),
            (Event::WakeInference, "desktop_wake_inference"),
            (Event::WakeClip, "desktop_wake_clip"),
            (Event::WakeReset, "desktop_wake_reset"),
            (Event::TranscriptionReady, "desktop_transcription_ready"),
            (Event::SecondaryStt, "desktop_secondary_stt"),
            (Event::WakeLevel, "desktop_wake_level"),
            (Event::ReadyCue, "desktop_ready_cue"),
            (Event::Cue, "desktop_cue"),
            (Event::EndingState, "desktop_ending_state"),
            (Event::EndingPause, "desktop_ending_pause"),
            (Event::EndCue, "desktop_end_cue"),
            (Event::CaptureFlow, "desktop_capture_flow"),
            (Event::HostCall, "desktop_host_call"),
        ] {
            assert_eq!(event.name(), name);
        }
    }

    #[test]
    fn utc_dates_follow_the_calendar() {
        for (ms, date) in [
            (0, "1970-01-01"),
            (1_709_164_800_000, "2024-02-29"),
            (1_709_251_199_999, "2024-02-29"),
            (1_793_577_600_000, "2026-11-02"),
        ] {
            assert_eq!(utc_date(ms), date);
        }
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
            "recordings",
            "recordings/<date>",
            "recordings/<date>/capture-<ts>.wav",
            "recordings/<date>/desktop.jsonl",
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
                set(&[
                    "recordings",
                    "recordings/<date>",
                    "recordings/<date>/desktop.jsonl",
                ]),
            ),
            (
                "audio",
                RecordKinds {
                    audio: true,
                    ..RecordKinds::default()
                },
                set(&[
                    "recordings",
                    "recordings/<date>",
                    "recordings/<date>/capture-<ts>.wav",
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
    async fn a_wake_clip_is_a_wav_named_for_its_kind_and_linked_by_a_metric_line() {
        let dir = TempDir::new("wake-clip");
        let recorder = Recorder::in_dir(ALL, dir.root()).unwrap();
        recorder.wake_clip("capture", vec![1, -2], json!({"capture": 0.5}));
        recorder.finish().await;

        let day = std::fs::read_dir(dir.root().join("recordings"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let files: Vec<_> = std::fs::read_dir(&day)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        let stamp = name
            .strip_prefix("wake-")
            .and_then(|rest| rest.strip_suffix("-capture.wav"))
            .unwrap();
        assert!(stamp.parse::<u64>().unwrap() > 1_700_000_000_000);
        assert_eq!(std::fs::read(&files[0]).unwrap(), wav(&[1, -2]));

        let written = lines(&dir.root().join("metrics.jsonl"));
        assert_eq!(written.len(), 1);
        assert_eq!(written[0]["event"], "desktop_wake_clip");
        assert_eq!(written[0]["outcome"], "capture");
        assert_eq!(written[0]["file"], name.as_str());
        assert_eq!(written[0]["capture"], 0.5);
    }

    #[tokio::test]
    async fn a_wake_clip_needs_audio_and_its_metric_line_needs_metrics() {
        for (tag, kinds, files) in [
            ("clip-metrics-only", METRICS, set(&[])),
            (
                "clip-audio-only",
                RecordKinds {
                    audio: true,
                    ..RecordKinds::default()
                },
                set(&[
                    "recordings",
                    "recordings/<date>",
                    "recordings/<date>/wake.wav",
                ]),
            ),
        ] {
            let dir = TempDir::new(tag);
            let recorder = Recorder::in_dir(kinds, dir.root()).unwrap();
            recorder.wake_clip("conversation", vec![0], Value::Null);
            recorder.finish().await;
            let seen: BTreeSet<_> = tree(&dir.root())
                .into_iter()
                .map(|path| {
                    if path.contains("/wake-") {
                        "recordings/<date>/wake.wav".to_owned()
                    } else {
                        path
                    }
                })
                .collect();
            assert_eq!(seen, files, "{tag}");
        }
    }

    #[tokio::test]
    async fn text_and_audio_recordings_are_written_for_the_capture() {
        let dir = TempDir::new("recordings");
        let recorder = Recorder::in_dir(ALL, dir.root()).unwrap();
        recorder.text("final", json!({"text": "hello"}));
        recorder.audio(vec![1, -2, 0x0304]);
        recorder.finish().await;

        let day = std::fs::read_dir(dir.root().join("recordings"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = lines(&day.join("desktop.jsonl"));
        assert_eq!(text.len(), 1);
        assert_eq!(text[0]["event"], "final");
        assert_eq!(text[0]["text"], "hello");

        let wav_path = std::fs::read_dir(&day)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "wav"))
            .unwrap();
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
        let day = std::fs::read_dir(dir.root().join("recordings"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(mode(&dir.root()), 0o700);
        assert_eq!(mode(&dir.root().join("recordings")), 0o700);
        assert_eq!(mode(&day), 0o700);
        assert_eq!(mode(&dir.root().join("metrics.jsonl")), 0o600);
        assert_eq!(mode(&day.join("desktop.jsonl")), 0o600);
        for entry in std::fs::read_dir(&day).unwrap() {
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

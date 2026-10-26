//! Owns opt-in session recordings written off the reply path.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::mpsc;

/// How many recording events may wait for the writer before drops start.
pub const CHANNEL_CAPACITY: usize = 128;

/// Which kinds of data a session records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordKinds {
    /// Timing and outcome events.
    pub metrics: bool,
    /// Turn and reply text.
    pub text: bool,
    /// Per-turn microphone WAV files.
    pub audio: bool,
}

impl RecordKinds {
    /// Whether anything is recorded at all.
    #[must_use]
    pub fn any(self) -> bool {
        self.metrics || self.text || self.audio
    }
}

/// Static recording settings for one session.
#[derive(Debug, Clone)]
pub struct RecordingConfig {
    /// Which kinds to record.
    pub kinds: RecordKinds,
    /// Root data directory; recordings live under `<data_dir>/recordings`.
    pub data_dir: PathBuf,
    /// Recognizer model name.
    pub stt_model: String,
    /// Chat model name.
    pub llm_model: String,
    /// Chat reasoning effort, if configured.
    pub reasoning_effort: Option<String>,
    /// Speech model name.
    pub tts_model: String,
    /// Speech voice name.
    pub tts_voice: String,
}

impl RecordingConfig {
    /// A configuration that records nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            kinds: RecordKinds::default(),
            data_dir: PathBuf::from("data"),
            stt_model: String::new(),
            llm_model: String::new(),
            reasoning_effort: None,
            tts_model: String::new(),
            tts_voice: String::new(),
        }
    }
}

enum RecordMessage {
    Line(Value),
    Wav { turn: u64, pcm: Vec<u8> },
}

/// A handle for emitting recording events without ever blocking.
#[derive(Clone)]
pub struct Recorder {
    sender: Option<mpsc::Sender<RecordMessage>>,
    dropped: Arc<AtomicU64>,
    kinds: RecordKinds,
    session: String,
    config: Arc<RecordingConfig>,
}

impl Recorder {
    /// Whether metrics events are written.
    #[must_use]
    pub fn metrics_enabled(&self) -> bool {
        self.kinds.metrics
    }

    /// Whether turn and reply text is written.
    #[must_use]
    pub fn text_enabled(&self) -> bool {
        self.kinds.text
    }

    /// Whether microphone audio is written.
    #[must_use]
    pub fn audio_enabled(&self) -> bool {
        self.kinds.audio
    }

    /// Recognizer model name for connect events.
    #[must_use]
    pub fn stt_model(&self) -> &str {
        &self.config.stt_model
    }

    /// Chat model name for reply events.
    #[must_use]
    pub fn llm_model(&self) -> &str {
        &self.config.llm_model
    }

    /// Chat reasoning effort for reply events.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.config.reasoning_effort.as_deref()
    }

    /// Speech model name for reply events.
    #[must_use]
    pub fn tts_model(&self) -> &str {
        &self.config.tts_model
    }

    /// Speech voice name for reply events.
    #[must_use]
    pub fn tts_voice(&self) -> &str {
        &self.config.tts_voice
    }

    /// How many events have been dropped because the writer fell behind.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Emits one named event; never blocks, drops and counts when behind.
    pub fn emit(&self, event: &str, fields: Value) {
        let Some(sender) = &self.sender else {
            return;
        };
        let writes_line =
            self.kinds.metrics || (self.kinds.text && matches!(event, "turn" | "reply"));
        if !writes_line {
            return;
        }
        let mut line = json!({
            "ts_ms": now_ms(),
            "session": self.session,
            "event": event,
        });
        if let (Some(target), Value::Object(source)) = (line.as_object_mut(), fields) {
            for (key, value) in source {
                target.insert(key, value);
            }
        }
        if sender.try_send(RecordMessage::Line(line)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Queues one turn's microphone audio as a WAV file; never blocks.
    pub fn emit_wav(&self, turn: u64, pcm: Vec<u8>) {
        if !self.kinds.audio {
            return;
        }
        let Some(sender) = &self.sender else {
            return;
        };
        if sender.try_send(RecordMessage::Wav { turn, pcm }).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A running recording: the emit handle plus the writer to await at close.
pub struct ActiveRecording {
    /// Handle for emitting events.
    pub recorder: Recorder,
    writer: Option<tokio::task::JoinHandle<()>>,
}

impl ActiveRecording {
    /// Emits the close event, then waits for the writer to flush and finish.
    pub async fn finish(mut self, duration_ms: u64) {
        self.recorder.emit(
            "session_close",
            json!({"duration_ms": duration_ms, "dropped": self.recorder.dropped()}),
        );
        let dropped = self.recorder.dropped();
        if dropped > 0 {
            tracing::info!(dropped, "recording events dropped");
        }
        drop(self.recorder);
        if let Some(writer) = self.writer.take() {
            let _ = writer.await;
        }
    }
}

/// Starts recording for one session; a disabled config starts no writer.
#[must_use]
pub fn start_recording(config: RecordingConfig, session: String) -> ActiveRecording {
    start_recording_with_gate(config, session, None)
}

fn start_recording_with_gate(
    config: RecordingConfig,
    session: String,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> ActiveRecording {
    let kinds = config.kinds;
    if !kinds.any() {
        return ActiveRecording {
            recorder: Recorder {
                sender: None,
                dropped: Arc::new(AtomicU64::new(0)),
                kinds,
                session,
                config: Arc::new(config),
            },
            writer: None,
        };
    }
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let date = utc_date_string(now_ms());
    let base = config
        .data_dir
        .join("recordings")
        .join(&date)
        .join(&session);
    let dropped = Arc::new(AtomicU64::new(0));
    let recorder = Recorder {
        sender: Some(sender),
        dropped: Arc::clone(&dropped),
        kinds,
        session: session.clone(),
        config: Arc::new(config),
    };
    let writer = tokio::spawn(write_loop(receiver, base, session, gate));
    ActiveRecording {
        recorder,
        writer: Some(writer),
    }
}

async fn write_loop(
    mut receiver: mpsc::Receiver<RecordMessage>,
    session_dir: PathBuf,
    session: String,
    gate: Option<Arc<tokio::sync::Notify>>,
) {
    if let Some(gate) = gate {
        gate.notified().await;
    }
    let jsonl_path = session_dir.parent().map_or_else(
        || PathBuf::from("session.jsonl"),
        |dir| dir.join(format!("{session}.jsonl")),
    );
    let mut jsonl: Option<tokio::fs::File> = None;
    let mut error_logged = false;
    while let Some(message) = receiver.recv().await {
        let result = match &message {
            RecordMessage::Line(line) => write_line(&mut jsonl, &jsonl_path, line).await,
            RecordMessage::Wav { turn, pcm } => write_wav(&session_dir, *turn, pcm).await,
        };
        if let Err(error) = result
            && !error_logged
        {
            error_logged = true;
            tracing::warn!(%error, "recording write failed; further recording errors are silent");
        }
    }
    if let Some(mut file) = jsonl {
        use tokio::io::AsyncWriteExt;
        let _ = file.flush().await;
    }
}

async fn write_line(
    jsonl: &mut Option<tokio::fs::File>,
    path: &PathBuf,
    line: &Value,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if jsonl.is_none() {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        *jsonl = Some(
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await?,
        );
    }
    let file = jsonl.as_mut().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "recording file missing")
    })?;
    file.write_all(format!("{line}\n").as_bytes()).await
}

async fn write_wav(session_dir: &PathBuf, turn: u64, pcm: &[u8]) -> std::io::Result<()> {
    tokio::fs::create_dir_all(session_dir).await?;
    let path = session_dir.join(format!("{turn}.wav"));
    tokio::fs::write(path, wav_bytes(pcm)).await
}

/// Wraps 24 kHz s16le mono PCM in a WAV container.
#[must_use]
pub fn wav_bytes(pcm: &[u8]) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let mut bytes = Vec::with_capacity(44 + pcm.len());
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36_u32 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&24_000_u32.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.extend_from_slice(pcm);
    bytes
}

/// Current Unix epoch time in milliseconds.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The UTC calendar date (`YYYY-MM-DD`) containing an epoch-millisecond instant.
#[must_use]
pub fn utc_date_string(epoch_ms: u64) -> String {
    let days = i64::try_from(epoch_ms / 86_400_000).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    })
    .unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Accumulates forwarded microphone audio and cuts padded turn clips.
#[derive(Debug, Default)]
pub struct AudioBuffer {
    base_ms: u64,
    data: Vec<u8>,
}

impl AudioBuffer {
    /// Discards all buffered audio (a new recognizer became ready).
    pub fn reset(&mut self) {
        self.base_ms = 0;
        self.data.clear();
    }

    /// Appends one forwarded frame.
    pub fn push(&mut self, frame: &[u8]) {
        self.data.extend_from_slice(frame);
    }

    /// How many milliseconds of audio are buffered from the base.
    fn duration_ms(&self) -> u64 {
        u64::try_from(self.data.len()).unwrap_or(u64::MAX) / 48
    }

    /// The recognizer-processed time at the end of the buffered audio.
    #[must_use]
    pub fn end_ms(&self) -> u64 {
        self.base_ms + self.duration_ms()
    }

    /// Cuts `start_ms..end_ms` (recognizer-processed time) with padding.
    #[must_use]
    pub fn cut(&self, start_ms: u64, end_ms: u64, padding_ms: u64) -> Vec<u8> {
        let start = start_ms.saturating_sub(padding_ms + self.base_ms);
        let end = (end_ms + padding_ms).saturating_sub(self.base_ms);
        let start_byte = usize::try_from(start * 48).unwrap_or(usize::MAX);
        let end_byte = usize::try_from(end * 48)
            .unwrap_or(usize::MAX)
            .min(self.data.len());
        if start_byte >= end_byte {
            return Vec::new();
        }
        self.data[start_byte.min(self.data.len())..end_byte].to_vec()
    }

    /// Drops audio before `keep_from_ms`, keeping padding for the next turn.
    pub fn prune(&mut self, keep_from_ms: u64) {
        if keep_from_ms <= self.base_ms {
            return;
        }
        let drop_ms = keep_from_ms - self.base_ms;
        let drop_bytes = usize::try_from(drop_ms * 48)
            .unwrap_or(usize::MAX)
            .min(self.data.len());
        self.data.drain(..drop_bytes);
        self.base_ms += u64::try_from(drop_bytes).unwrap_or(u64::MAX) / 48;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioBuffer, RecordKinds, RecordingConfig, start_recording_with_gate, utc_date_string,
        wav_bytes,
    };
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn utc_dates_match_known_days() {
        let cases: Vec<(u64, &str)> = vec![
            (0, "1970-01-01"),
            (86_400_000, "1970-01-02"),
            (1_735_689_600_000, "2025-01-01"),
            (1_782_000_000_000, "2026-06-21"),
        ];
        for (ms, expected) in cases {
            assert_eq!(utc_date_string(ms), expected, "ms={ms}");
        }
    }

    #[test]
    fn wav_header_describes_24khz_s16le_mono() {
        let wav = wav_bytes(&[1, 2, 3, 4]);
        assert_eq!(wav.len(), 48);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[44..], &[1, 2, 3, 4]);
    }

    #[test]
    fn cut_applies_padding_and_clamps_to_the_buffer() {
        let mut buffer = AudioBuffer::default();
        buffer.push(&vec![7_u8; 48_000]);
        assert_eq!(buffer.cut(100, 500, 300).len(), 38_400);
        assert_eq!(buffer.cut(900, 990, 300).len(), 48_000 - 600 * 48);
    }

    #[tokio::test]
    async fn a_writer_that_cannot_keep_up_drops_events_without_blocking_the_emitter() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let data_dir =
            std::env::temp_dir().join(format!("thalvi-recorder-drop-test-{}", std::process::id()));
        let config = RecordingConfig {
            kinds: RecordKinds {
                metrics: true,
                text: false,
                audio: false,
            },
            data_dir: data_dir.clone(),
            stt_model: String::new(),
            llm_model: String::new(),
            reasoning_effort: None,
            tts_model: String::new(),
            tts_voice: String::new(),
        };
        let active =
            start_recording_with_gate(config, "drop-test".to_string(), Some(Arc::clone(&gate)));
        let recorder = active.recorder.clone();
        for _ in 0..200 {
            recorder.emit("turn", json!({"turn": 1}));
        }
        assert!(
            recorder.dropped() > 0,
            "events beyond the bounded channel must be dropped, not queued"
        );
        gate.notify_one();
        drop(recorder);
        active.finish(0).await;
        std::fs::remove_dir_all(&data_dir).unwrap();
    }
}

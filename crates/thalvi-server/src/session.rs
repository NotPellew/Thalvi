//! Owns one voice session: recognizer, reply pipeline, and the client socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::Instrument;

use crate::model::{ChatMessage, ChatModel, Role};
use crate::recognizer::{Recognizer, RecognizerEvent, RecognizerFactory};
use crate::recorder::{AudioBuffer, Recorder, RecordingConfig, start_recording};
use crate::sentence::SentenceSplitter;
use crate::store::{Attachment, Detach, SessionStore};
use crate::title::{Titler, spawn_title};
use crate::tts::Tts;

const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const ACK_DELAY: Duration = Duration::from_millis(500);
/// Audio kept on both sides of a spoken turn, as the recognizer's boundaries are tight.
const CLIP_PADDING_MS: u64 = 300;
const ACK_LINES: [&str; 3] = [
    "Got it, one moment.",
    "Let me think about that.",
    "One moment.",
];

type AckCache = Arc<std::sync::Mutex<std::collections::HashMap<&'static str, Bytes>>>;

#[derive(Clone)]
struct AckSettings {
    enabled: bool,
    cache: AckCache,
}

enum ReplyEvent {
    Delta(String),
    Sentence(String),
    Audio(Bytes),
    AckAudio(Bytes),
    FirstSentence(Instant),
    ModelFinished(Instant),
    Finished,
    Failed(String),
}

enum RecognizerMessage {
    Event(RecognizerEvent),
    Disconnected { message: String, fatal: bool },
    Ready,
}

struct ActiveReply {
    handle: JoinHandle<()>,
    turn: u64,
    started_at: Instant,
    sent_sentences: Vec<String>,
    text: String,
    /// Whether every sentence of the reply so far was sent to speech.
    spoken_throughout: bool,
    speaking_sent: bool,
    first_delta_at: Option<Instant>,
    model_finished_at: Option<Instant>,
    first_sentence_at: Option<Instant>,
    first_audio_at: Option<Instant>,
    ack_first_audio_at: Option<Instant>,
    audio_bytes: usize,
    /// Length of the user's audio sent to the model, when it heard the turn.
    input_audio_ms: Option<u64>,
}

/// Reply audio sent on this connection that the client may still be playing.
#[derive(Default)]
struct ClientAudio {
    sent_bytes: u64,
    pending: bool,
}

impl ClientAudio {
    fn sent(&mut self, bytes: usize) {
        self.sent_bytes += u64::try_from(bytes).unwrap_or(u64::MAX);
        self.pending = true;
    }

    /// The client has played or dropped the first `bytes` it received; a stale count changes nothing.
    fn played(&mut self, bytes: u64) {
        if bytes == self.sent_bytes {
            self.pending = false;
        }
    }

    fn clear(&mut self) {
        self.pending = false;
    }
}

#[derive(Default)]
struct PendingVoice {
    start_ms: Option<u64>,
    end_ms: Option<u64>,
}

/// Maps recognizer-processed audio time to when that audio was forwarded.
#[derive(Default)]
struct ForwardClock {
    forwarded_ms: u64,
    marks: Vec<(u64, Instant)>,
}

impl ForwardClock {
    fn reset(&mut self) {
        self.forwarded_ms = 0;
        self.marks.clear();
    }

    fn push(&mut self, bytes: usize, at: Instant) {
        self.forwarded_ms += u64::try_from(bytes).unwrap_or(u64::MAX) / 48;
        self.marks.push((self.forwarded_ms, at));
    }

    fn instant_for(&self, processed_ms: u64) -> Option<Instant> {
        self.marks
            .iter()
            .find(|(mark_ms, _)| *mark_ms >= processed_ms)
            .map(|(_, at)| *at)
    }
}

impl TurnLog {
    fn new(recorder: Recorder, next_turn: u64, audio_input: bool) -> Self {
        Self {
            recorder,
            audio_input,
            pending: std::collections::HashMap::new(),
            audio: AudioBuffer::default(),
            forward_clock: ForwardClock::default(),
            next_turn,
            last_reply_turn: 0,
            last_reply_audio_bytes: 0,
        }
    }

    /// Forgets buffered mic audio and timing; a new recognizer starts its clock at zero.
    fn reset_mic(&mut self) {
        self.audio.reset();
        self.forward_clock.reset();
    }

    fn buffers_audio(&self) -> bool {
        self.audio_input || self.recorder.audio_enabled()
    }

    /// Takes a finished voice turn's boundaries and its padded mic clip, and drops the
    /// audio the next turn no longer needs.
    fn take_voice_turn(&mut self, turn_id: &str) -> (PendingVoice, Vec<u8>) {
        let pending = self.pending.remove(turn_id).unwrap_or_default();
        if !self.buffers_audio() {
            return (pending, Vec::new());
        }
        let start = pending.start_ms.unwrap_or(0);
        let end = pending.end_ms.unwrap_or_else(|| self.audio.end_ms());
        let clip = self.audio.cut(start, end, CLIP_PADDING_MS);
        self.audio.prune(end.saturating_sub(CLIP_PADDING_MS));
        (pending, clip)
    }
}

/// Per-session recording state: pending voice turns and forwarded audio.
struct TurnLog {
    recorder: Recorder,
    audio_input: bool,
    pending: std::collections::HashMap<String, PendingVoice>,
    audio: AudioBuffer,
    forward_clock: ForwardClock,
    next_turn: u64,
    last_reply_turn: u64,
    last_reply_audio_bytes: usize,
}

struct RecognizerHandle {
    audio_tx: mpsc::Sender<Vec<u8>>,
    task: JoinHandle<()>,
    ready: bool,
}

/// Whether the user wants voice and whether the recognizer can hear them.
#[derive(Clone, Copy)]
struct Voice {
    on: bool,
    ready: bool,
}

impl Voice {
    fn new(on: bool, recognizer: Option<&RecognizerHandle>) -> Self {
        Self {
            on,
            ready: recognizer.is_some_and(|handle| handle.ready),
        }
    }

    /// The state to report when no reply is running.
    fn resting_state(self) -> &'static str {
        match (self.on, self.ready) {
            (false, _) => "idle",
            (true, true) => "listening",
            (true, false) => "starting",
        }
    }
}

/// Keeps the store in step with the history and asks for a title after the first reply.
struct Persistence<M> {
    store: Arc<SessionStore>,
    attachment: Attachment,
    titler: Option<Arc<Titler<M>>>,
    saved_len: usize,
    title_tried: bool,
}

impl<M: ChatModel + 'static> Persistence<M> {
    async fn attach(
        store: Arc<SessionStore>,
        resume: Option<&str>,
        titler: Option<Arc<Titler<M>>>,
    ) -> (Self, Vec<ChatMessage>) {
        let mut attachment = store.attach(resume).await;
        let history = std::mem::take(&mut attachment.history);
        let persistence = Self {
            store,
            attachment,
            titler,
            saved_len: history.len(),
            title_tried: false,
        };
        (persistence, history)
    }

    fn finish(mut self, history: &[ChatMessage], next_turn: u64, recorder: &Recorder) {
        self.sync(history, next_turn, recorder);
        self.store.release(self.attachment);
    }

    fn sync(&mut self, history: &[ChatMessage], next_turn: u64, recorder: &Recorder) {
        if history.len() == self.saved_len {
            return;
        }
        self.saved_len = history.len();
        self.store.save(&self.attachment, history, next_turn);
        let (Some(titler), false) = (&self.titler, self.title_tried) else {
            return;
        };
        let first_user = history.iter().find(|message| message.role == Role::User);
        let first_reply = history
            .iter()
            .find(|message| message.role == Role::Assistant);
        if let (Some(user), Some(reply)) = (first_user, first_reply)
            && self.store.needs_title(&self.attachment.id)
        {
            self.title_tried = true;
            spawn_title(
                Arc::clone(titler),
                Arc::clone(&self.store),
                self.attachment.id.clone(),
                &user.content,
                &reply.content,
                recorder.clone(),
            );
        }
    }
}

const MAX_TEXT_CHARS: usize = 20_000;
/// How long a client that is being let go may hold up the end of its session.
const ENDED_SEND_WAIT: Duration = Duration::from_secs(2);

/// Runs one client session until its socket closes.
#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
pub async fn handle_session<F, M, T>(
    socket: WebSocket,
    factory: F,
    model: Arc<M>,
    tts: Arc<T>,
    store: Arc<SessionStore>,
    resume: Option<String>,
    titler: Option<Arc<Titler<M>>>,
    recording_config: RecordingConfig,
    ack_enabled: bool,
    audio_input: bool,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let factory = Arc::new(factory);
    let session_started = Instant::now();
    let (mut persistence, mut history) =
        Persistence::attach(store, resume.as_deref(), titler).await;
    let session_id = persistence.attachment.id.clone();
    let resumed = !history.is_empty();
    let recording = start_recording(recording_config, session_id.clone());
    let mut turn_log = TurnLog::new(
        recording.recorder.clone(),
        persistence.attachment.next_turn,
        audio_input,
    );
    turn_log
        .recorder
        .emit("session_open", json!({"resumed": resumed}));
    send_session_opening(&mut sink, &session_id, &history).await;
    send_state(&mut sink, "idle").await;
    tracing::info!("session opened");

    let ack = AckSettings {
        enabled: ack_enabled,
        cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };
    let voice = watch::Sender::new(false);
    let mut voice_on = false;
    let mut recognizer: Option<RecognizerHandle> = None;
    let mut rec_rx: Option<mpsc::Receiver<RecognizerMessage>> = None;
    let mut reply: Option<ActiveReply> = None;
    let mut reply_rx: Option<mpsc::Receiver<ReplyEvent>> = None;
    let mut client_audio = ClientAudio::default();
    let mut detached = None;

    loop {
        persistence.sync(&history, turn_log.next_turn, &turn_log.recorder);
        let reply_event = async {
            match &mut reply_rx {
                Some(receiver) => receiver.recv().await,
                None => std::future::pending().await,
            }
        };
        let recognizer_event = async {
            match &mut rec_rx {
                Some(receiver) => receiver.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            reason = persistence.attachment.detached() => {
                detached = Some(reason);
                break;
            }
            client_message = stream.next() => {
                match client_message {
                    Some(Ok(Message::Binary(audio))) => {
                        forward_mic(&audio, recognizer.as_ref(), &mut turn_log);
                    }
                    Some(Ok(Message::Text(text))) => {
                        handle_client_text(&text, &mut sink, &mut recognizer, &mut rec_rx, &factory, &voice, &mut voice_on, &mut history, &mut reply, &mut reply_rx, &mut client_audio, &model, &tts, &mut turn_log, ack.clone()).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::debug!(%error, "client socket error");
                        break;
                    }
                }
            }
            recognizer_message = recognizer_event => {
                handle_recognizer_message(recognizer_message, &mut sink, &mut recognizer, &mut rec_rx, &mut history, &mut reply, &mut reply_rx, &mut client_audio, &model, &tts, &voice, &mut voice_on, &mut turn_log, ack.clone()).await;
            }
            event = reply_event => {
                let Some(event) = event else {
                    reply_rx = None;
                    continue;
                };
                handle_reply_event(event, &mut sink, &mut history, &mut reply, &mut reply_rx, &mut client_audio, Voice::new(voice_on, recognizer.as_ref()), &mut turn_log).await;
            }
        }
    }

    cut_off_reply(&mut history, &mut reply, &mut reply_rx, &turn_log);
    recognizer.take().inspect(|handle| handle.task.abort());
    close_session(
        persistence,
        &history,
        turn_log.next_turn,
        &turn_log.recorder,
        detached,
        &mut sink,
    )
    .await;
    drop(turn_log);
    recording
        .finish(u64::try_from(session_started.elapsed().as_millis()).unwrap_or(u64::MAX))
        .await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn handle_client_text<F, M, T>(
    text: &str,
    sink: &mut (impl SinkExt<Message> + Unpin),
    recognizer: &mut Option<RecognizerHandle>,
    rec_rx: &mut Option<mpsc::Receiver<RecognizerMessage>>,
    factory: &Arc<F>,
    voice: &watch::Sender<bool>,
    voice_on: &mut bool,
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    client_audio: &mut ClientAudio,
    model: &Arc<M>,
    tts: &Arc<T>,
    turn_log: &mut TurnLog,
    ack: AckSettings,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    match parse_client_text(text) {
        ClientText::Cancel => {
            interrupt(
                sink,
                history,
                reply,
                reply_rx,
                client_audio,
                Voice::new(*voice_on, recognizer.as_ref()),
                "cancel",
                turn_log,
            )
            .await;
        }
        ClientText::Played(bytes) => client_audio.played(bytes),
        ClientText::Voice(on) => {
            *voice_on = on;
            set_voice(
                on,
                sink,
                recognizer,
                rec_rx,
                factory,
                voice,
                reply,
                client_audio,
                turn_log.recorder.clone(),
            )
            .await;
        }
        ClientText::Text(raw) => match validate_text(&raw) {
            Ok(text) => {
                let turn = begin_turn(
                    sink,
                    history,
                    reply,
                    reply_rx,
                    client_audio,
                    model,
                    tts,
                    voice,
                    Voice::new(*voice_on, recognizer.as_ref()),
                    "text",
                    text.clone(),
                    &[],
                    turn_log,
                    ack.clone(),
                )
                .await;
                let mut fields = json!({"turn": turn, "source": "text"});
                if turn_log.recorder.text_enabled() {
                    fields["text"] = json!(text);
                }
                turn_log.recorder.emit("turn", fields);
            }
            Err(message) => {
                send_json(sink, &json!({"type": "error", "message": message})).await;
            }
        },
        ClientText::Other => {}
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn handle_recognizer_message<M, T>(
    recognizer_message: Option<RecognizerMessage>,
    sink: &mut (impl SinkExt<Message> + Unpin),
    recognizer: &mut Option<RecognizerHandle>,
    rec_rx: &mut Option<mpsc::Receiver<RecognizerMessage>>,
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    client_audio: &mut ClientAudio,
    model: &Arc<M>,
    tts: &Arc<T>,
    voice: &watch::Sender<bool>,
    voice_on: &mut bool,
    turn_log: &mut TurnLog,
    ack: AckSettings,
) where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    if let Some(RecognizerMessage::Event(event)) = &recognizer_message {
        tracing::debug!(?event, "recognizer event");
    }
    match recognizer_message {
        Some(RecognizerMessage::Event(RecognizerEvent::SpeechStart {
            turn_id,
            audio_processed_ms,
        })) => {
            turn_log.pending.insert(
                turn_id,
                PendingVoice {
                    start_ms: audio_processed_ms,
                    end_ms: None,
                },
            );
            interrupt(
                sink,
                history,
                reply,
                reply_rx,
                client_audio,
                Voice::new(*voice_on, recognizer.as_ref()),
                "speech start",
                turn_log,
            )
            .await;
        }
        Some(RecognizerMessage::Event(RecognizerEvent::SpeechEnd {
            turn_id,
            audio_processed_ms,
        })) => {
            turn_log.pending.entry(turn_id).or_default().end_ms = audio_processed_ms;
        }
        Some(RecognizerMessage::Event(RecognizerEvent::Partial { text })) => {
            send_json(sink, &json!({"type": "partial", "text": text})).await;
        }
        Some(RecognizerMessage::Event(RecognizerEvent::SpeechComplete { turn_id, text })) => {
            if !text.trim().is_empty() {
                let (pending, clip) = turn_log.take_voice_turn(&turn_id);
                let turn = begin_turn(
                    sink,
                    history,
                    reply,
                    reply_rx,
                    client_audio,
                    model,
                    tts,
                    voice,
                    Voice::new(*voice_on, recognizer.as_ref()),
                    "voice",
                    text.clone(),
                    &clip,
                    turn_log,
                    ack.clone(),
                )
                .await;
                record_voice_turn(turn_log, turn, &pending, &text, clip);
            }
        }
        Some(RecognizerMessage::Event(RecognizerEvent::Error { message })) => {
            send_json(
                sink,
                &json!({"type": "error", "message": plain_recognizer_error(&message)}),
            )
            .await;
        }
        Some(RecognizerMessage::Disconnected { message, fatal }) => {
            if let Some(handle) = recognizer {
                handle.ready = false;
            }
            tracing::warn!(%message, fatal, "recognizer disconnected");
            let message = if fatal {
                format!(
                    "Speech recognition stopped and won't retry: {message}. Check the speech settings, then tap Start again."
                )
            } else {
                plain_recognizer_error(&message)
            };
            send_json(sink, &json!({"type": "error", "message": message})).await;
            if fatal {
                *voice_on = false;
                send_json(sink, &json!({"type": "voice", "on": false})).await;
                stop_voice(sink, recognizer, rec_rx, voice, reply, client_audio).await;
            } else if *voice_on && reply.is_none() {
                send_state(sink, "starting").await;
            }
        }
        Some(RecognizerMessage::Ready) => {
            turn_log.reset_mic();
            if let Some(handle) = recognizer {
                handle.ready = true;
            }
            if *voice_on && reply.is_none() {
                send_state(sink, "listening").await;
            }
        }
        None => {}
    }
}

/// Passes one mic frame to a ready recognizer, keeping it for turn clips.
fn forward_mic(audio: &[u8], recognizer: Option<&RecognizerHandle>, turn_log: &mut TurnLog) {
    let forwarded = recognizer
        .is_some_and(|handle| handle.ready && handle.audio_tx.try_send(audio.to_vec()).is_ok());
    if !forwarded {
        tracing::debug!("mic frame dropped");
        return;
    }
    turn_log.forward_clock.push(audio.len(), Instant::now());
    if turn_log.buffers_audio() {
        turn_log.audio.push(audio);
    }
}

fn record_voice_turn(
    turn_log: &TurnLog,
    turn: u64,
    pending: &PendingVoice,
    text: &str,
    clip: Vec<u8>,
) {
    let recorder = &turn_log.recorder;
    let mut fields = json!({"turn": turn, "source": "voice"});
    if let (Some(start), Some(end)) = (pending.start_ms, pending.end_ms) {
        fields["speech_ms"] = json!(end.saturating_sub(start));
    }
    if let Some(end_ms) = pending.end_ms
        && let Some(forwarded_at) = turn_log.forward_clock.instant_for(end_ms)
    {
        fields["endpoint_lag_ms"] =
            json!(u64::try_from(forwarded_at.elapsed().as_millis()).unwrap_or(u64::MAX));
    }
    if recorder.text_enabled() {
        fields["text"] = json!(text);
    }
    if recorder.audio_enabled() && !clip.is_empty() {
        recorder.emit_wav(turn, clip);
        if recorder.metrics_enabled() || recorder.text_enabled() {
            fields["audio"] = json!(format!("{turn}.wav"));
        }
    }
    recorder.emit("turn", fields);
}

/// Saves and releases the conversation first, so a stalled client cannot hold up the
/// handoff, then tells a client that was let go why.
async fn close_session<M: ChatModel + 'static>(
    persistence: Persistence<M>,
    history: &[ChatMessage],
    next_turn: u64,
    recorder: &Recorder,
    detached: Option<Detach>,
    sink: &mut (impl SinkExt<Message> + Unpin),
) {
    persistence.finish(history, next_turn, recorder);
    tracing::info!("session closed");
    if let Some(reason) = detached {
        let _ = tokio::time::timeout(ENDED_SEND_WAIT, send_ended(sink, reason)).await;
    }
}

/// Tells the client why its conversation was taken away, then closes its socket.
async fn send_ended(sink: &mut (impl SinkExt<Message> + Unpin), reason: Detach) {
    let (reason, message) = match reason {
        Detach::Moved => ("moved", "This conversation continued on another device."),
        Detach::Deleted => ("deleted", "This conversation was deleted."),
    };
    send_json(
        sink,
        &json!({"type": "ended", "reason": reason, "message": message}),
    )
    .await;
    let _ = sink.send(Message::Close(None)).await;
}

async fn send_session_opening(
    sink: &mut (impl SinkExt<Message> + Unpin),
    session_id: &str,
    history: &[ChatMessage],
) {
    let turns: Vec<serde_json::Value> = history
        .iter()
        .map(|message| json!({"role": message.role, "text": message.content}))
        .collect();
    send_json(
        sink,
        &json!({"type": "session", "id": session_id, "turns": turns}),
    )
    .await;
}

fn spawn_recognizer<F>(
    factory: &Arc<F>,
    recorder: Recorder,
) -> (RecognizerHandle, mpsc::Receiver<RecognizerMessage>)
where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
{
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(32);
    let (rec_tx, rec_rx) = mpsc::channel::<RecognizerMessage>(32);
    let task = tokio::spawn(
        run_recognizer(Arc::clone(factory), audio_rx, rec_tx, recorder).in_current_span(),
    );
    (
        RecognizerHandle {
            audio_tx,
            task,
            ready: false,
        },
        rec_rx,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn set_voice<F>(
    on: bool,
    sink: &mut (impl SinkExt<Message> + Unpin),
    recognizer: &mut Option<RecognizerHandle>,
    rec_rx: &mut Option<mpsc::Receiver<RecognizerMessage>>,
    factory: &Arc<F>,
    voice: &watch::Sender<bool>,
    reply: &mut Option<ActiveReply>,
    client_audio: &mut ClientAudio,
    recorder: Recorder,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
{
    if !on {
        stop_voice(sink, recognizer, rec_rx, voice, reply, client_audio).await;
        return;
    }
    voice.send_replace(true);
    if recognizer.is_none() {
        let (handle, receiver) = spawn_recognizer(factory, recorder);
        *recognizer = Some(handle);
        *rec_rx = Some(receiver);
        if reply.is_none() {
            send_state(sink, "starting").await;
        }
    }
}

/// Turns voice off: drops the recognizer and stops speaking the running reply.
async fn stop_voice(
    sink: &mut (impl SinkExt<Message> + Unpin),
    recognizer: &mut Option<RecognizerHandle>,
    rec_rx: &mut Option<mpsc::Receiver<RecognizerMessage>>,
    voice: &watch::Sender<bool>,
    reply: &mut Option<ActiveReply>,
    client_audio: &mut ClientAudio,
) {
    voice.send_replace(false);
    if let Some(handle) = recognizer.take() {
        handle.task.abort();
    }
    *rec_rx = None;
    client_audio.clear();
    if let Some(active) = reply {
        active.spoken_throughout = false;
        send_state(sink, "thinking").await;
    } else {
        send_state(sink, "idle").await;
    }
}

enum ClientText {
    Cancel,
    Played(u64),
    Voice(bool),
    Text(String),
    Other,
}

fn parse_client_text(text: &str) -> ClientText {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return ClientText::Other;
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("cancel") => ClientText::Cancel,
        Some("played") => value
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .map_or(ClientText::Other, ClientText::Played),
        Some("voice") => value
            .get("on")
            .and_then(serde_json::Value::as_bool)
            .map_or(ClientText::Other, ClientText::Voice),
        Some("text") => ClientText::Text(
            value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        _ => ClientText::Other,
    }
}

fn validate_text(raw: &str) -> Result<String, &'static str> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("That message was empty, so it was not sent.");
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err("That message is too long (at most 20000 characters), so it was not sent.");
    }
    Ok(text.to_string())
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn begin_turn<M, T>(
    sink: &mut (impl SinkExt<Message> + Unpin),
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    client_audio: &mut ClientAudio,
    model: &Arc<M>,
    tts: &Arc<T>,
    voice: &watch::Sender<bool>,
    state: Voice,
    source: &'static str,
    text: String,
    clip: &[u8],
    turn_log: &mut TurnLog,
    ack: AckSettings,
) -> u64
where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    tracing::info!(source, chars = text.chars().count(), "user turn");
    interrupt(
        sink,
        history,
        reply,
        reply_rx,
        client_audio,
        state,
        "new turn",
        turn_log,
    )
    .await;
    turn_log.next_turn += 1;
    let turn = turn_log.next_turn;
    let audio = (turn_log.audio_input && !clip.is_empty()).then(|| clip.to_vec());
    history.push(ChatMessage::user(text.clone()));
    send_json(sink, &json!({"type": "user", "text": text})).await;
    send_state(sink, "thinking").await;
    let (active, events_rx) = start_reply(
        Arc::clone(model),
        Arc::clone(tts),
        voice.subscribe(),
        history.clone(),
        audio,
        turn,
        turn_log.recorder.clone(),
        ack,
    );
    *reply = Some(active);
    *reply_rx = Some(events_rx);
    turn
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
fn start_reply<M, T>(
    model: Arc<M>,
    tts: Arc<T>,
    voice: watch::Receiver<bool>,
    history: Vec<ChatMessage>,
    audio: Option<Vec<u8>>,
    turn: u64,
    recorder: Recorder,
    ack: AckSettings,
) -> (ActiveReply, mpsc::Receiver<ReplyEvent>)
where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (events_tx, events_rx) = mpsc::channel::<ReplyEvent>(64);
    let spoken_throughout = *voice.borrow();
    let input_audio_ms = audio
        .as_ref()
        .map(|pcm| u64::try_from(pcm.len()).unwrap_or(u64::MAX) / 48);
    let active = ActiveReply {
        handle: tokio::spawn(
            run_reply(
                model, tts, voice, history, audio, events_tx, turn, recorder, ack,
            )
            .in_current_span(),
        ),
        turn,
        started_at: Instant::now(),
        sent_sentences: Vec::new(),
        text: String::new(),
        spoken_throughout,
        speaking_sent: false,
        first_delta_at: None,
        model_finished_at: None,
        first_sentence_at: None,
        first_audio_at: None,
        ack_first_audio_at: None,
        audio_bytes: 0,
        input_audio_ms,
    };
    (active, events_rx)
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn handle_reply_event(
    event: ReplyEvent,
    sink: &mut (impl SinkExt<Message> + Unpin),
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    client_audio: &mut ClientAudio,
    state: Voice,
    turn_log: &mut TurnLog,
) {
    match event {
        ReplyEvent::Delta(text) => {
            if let Some(active) = reply {
                if active.first_delta_at.is_none() {
                    active.first_delta_at = Some(Instant::now());
                }
                active.text.push_str(&text);
            }
            send_json(sink, &json!({"type": "reply", "text": text})).await;
        }
        ReplyEvent::ModelFinished(at) => {
            if let Some(active) = reply {
                active.model_finished_at = Some(at);
            }
        }
        ReplyEvent::FirstSentence(at) => {
            if let Some(active) = reply
                && active.first_sentence_at.is_none()
            {
                active.first_sentence_at = Some(at);
            }
        }
        ReplyEvent::Sentence(sentence) => {
            if let Some(active) = reply {
                active.sent_sentences.push(sentence);
            }
        }
        ReplyEvent::AckAudio(_) | ReplyEvent::Audio(_) if !state.on => {}
        ReplyEvent::AckAudio(audio) => {
            if reply.as_ref().is_some_and(|active| !active.speaking_sent) {
                send_state(sink, "speaking").await;
            }
            if let Some(active) = reply {
                active.speaking_sent = true;
                if active.ack_first_audio_at.is_none() {
                    active.ack_first_audio_at = Some(Instant::now());
                }
            }
            client_audio.sent(audio.len());
            let _ = sink.send(Message::Binary(audio)).await;
        }
        ReplyEvent::Audio(audio) => {
            if reply.as_ref().is_some_and(|active| !active.speaking_sent) {
                send_state(sink, "speaking").await;
            }
            if let Some(active) = reply {
                active.speaking_sent = true;
                if active.first_audio_at.is_none() {
                    active.first_audio_at = Some(Instant::now());
                }
                active.audio_bytes += audio.len();
            }
            client_audio.sent(audio.len());
            let _ = sink.send(Message::Binary(audio)).await;
        }
        ReplyEvent::Finished => {
            if let Some(active) = reply.take() {
                tracing::info!(
                    first_audio_ms = active
                        .first_audio_at
                        .map(|at| at.duration_since(active.started_at).as_millis()),
                    audio_bytes = active.audio_bytes,
                    "reply finished"
                );
                record_assistant_history(history, &active, true);
                turn_log.last_reply_turn = active.turn;
                turn_log.last_reply_audio_bytes = active.audio_bytes;
                emit_reply_record(&turn_log.recorder, &active, "done", None);
            }
            *reply_rx = None;
            send_json(sink, &json!({"type": "reply_end"})).await;
            send_state(sink, state.resting_state()).await;
        }
        ReplyEvent::Failed(message) => {
            tracing::warn!(%message, "reply failed");
            if let Some(active) = reply.take() {
                record_assistant_history(history, &active, false);
                emit_reply_record(&turn_log.recorder, &active, "failed", Some(&message));
            }
            *reply_rx = None;
            send_json(sink, &json!({"type": "reply_end"})).await;
            send_json(sink, &json!({"type": "error", "message": message})).await;
            send_state(sink, state.resting_state()).await;
        }
    }
}

fn model_ms(started_at: Instant, model_finished_at: Option<Instant>, recorded_at: Instant) -> u64 {
    let end = model_finished_at.unwrap_or(recorded_at);
    u64::try_from(end.duration_since(started_at).as_millis()).unwrap_or(u64::MAX)
}

fn emit_reply_record(
    recorder: &Recorder,
    active: &ActiveReply,
    outcome: &str,
    error: Option<&str>,
) {
    let ms_since = |at: Option<Instant>| {
        at.map(|at| {
            u64::try_from(at.duration_since(active.started_at).as_millis()).unwrap_or(u64::MAX)
        })
    };
    let mut fields = json!({
        "turn": active.turn,
        "model": recorder.llm_model(),
        "reasoning_effort": recorder.reasoning_effort(),
        "first_token_ms": ms_since(active.first_delta_at),
        "first_sentence_ms": ms_since(active.first_sentence_at),
        "model_ms": model_ms(active.started_at, active.model_finished_at, Instant::now()),
        "tts_model": recorder.tts_model(),
        "voice": recorder.tts_voice(),
        "first_audio_ms": ms_since(active.first_audio_at),
        "ack_first_audio_ms": ms_since(active.ack_first_audio_at),
        "sentences": active.sent_sentences.len(),
        "audio_ms": u64::try_from(active.audio_bytes).unwrap_or(u64::MAX) * 1000 / 48_000,
        "input_audio_ms": active.input_audio_ms,
        "outcome": outcome,
        "error": error,
    });
    if recorder.text_enabled() {
        fields["text"] = json!(active.text.trim());
        if outcome == "interrupted" {
            fields["tts_text"] = json!(active.sent_sentences.join(" "));
        }
    }
    recorder.emit("reply", fields);
}

/// Stores what the user got: a finished reply in full, a cut-off one as far as it was spoken.
fn record_assistant_history(history: &mut Vec<ChatMessage>, active: &ActiveReply, finished: bool) {
    let spoken = active.sent_sentences.join(" ");
    let heard_only_ack = active.speaking_sent && spoken.trim().is_empty();
    let text =
        if finished || !active.spoken_throughout || (spoken.trim().is_empty() && !heard_only_ack) {
            active.text.trim().to_string()
        } else {
            spoken
        };
    if !text.is_empty() {
        history.push(ChatMessage::assistant(text));
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn interrupt(
    sink: &mut (impl SinkExt<Message> + Unpin),
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    client_audio: &mut ClientAudio,
    state: Voice,
    reason: &'static str,
    turn_log: &mut TurnLog,
) {
    if reply.is_none() && !client_audio.pending {
        return;
    }
    let (turn, audio_bytes) = match reply.as_ref() {
        Some(active) => (active.turn, active.audio_bytes),
        None => (turn_log.last_reply_turn, turn_log.last_reply_audio_bytes),
    };
    turn_log.recorder.emit(
        "interrupt",
        json!({
            "turn": turn,
            "reason": reason,
            "audio_sent_ms": u64::try_from(audio_bytes).unwrap_or(u64::MAX) * 1000 / 48_000,
        }),
    );
    cut_off_reply(history, reply, reply_rx, turn_log);
    client_audio.clear();
    tracing::info!(reason, "reply interrupted");
    send_json(sink, &json!({"type": "interrupted"})).await;
    send_state(sink, state.resting_state()).await;
}

/// Aborts the running reply, if any, and keeps what it produced so far in history.
fn cut_off_reply(
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    turn_log: &TurnLog,
) {
    if let Some(mut active) = reply.take() {
        active.handle.abort();
        if let Some(receiver) = reply_rx.as_mut() {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    ReplyEvent::Sentence(sentence) => active.sent_sentences.push(sentence),
                    ReplyEvent::Delta(text) => active.text.push_str(&text),
                    ReplyEvent::FirstSentence(at) => {
                        if active.first_sentence_at.is_none() {
                            active.first_sentence_at = Some(at);
                        }
                    }
                    ReplyEvent::ModelFinished(at) => active.model_finished_at = Some(at),
                    _ => {}
                }
            }
        }
        record_assistant_history(history, &active, false);
        emit_reply_record(&turn_log.recorder, &active, "interrupted", None);
    }
    *reply_rx = None;
}

fn plain_recognizer_error(message: &str) -> String {
    if message.trim().is_empty() {
        "Speech recognition hit a problem. You can keep talking.".to_string()
    } else {
        format!("Speech recognition hit a problem: {message}")
    }
}

async fn send_state(sink: &mut (impl SinkExt<Message> + Unpin), state: &str) {
    send_json(sink, &json!({"type": "state", "state": state})).await;
}

async fn send_json(sink: &mut (impl SinkExt<Message> + Unpin), value: &serde_json::Value) {
    let _ = sink.send(Message::Text(value.to_string().into())).await;
}

struct AbortOnDrop {
    handles: Vec<tokio::task::AbortHandle>,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn run_reply<M, T>(
    model: Arc<M>,
    tts: Arc<T>,
    voice: watch::Receiver<bool>,
    history: Vec<ChatMessage>,
    audio: Option<Vec<u8>>,
    events: mpsc::Sender<ReplyEvent>,
    turn: u64,
    recorder: Recorder,
    ack: AckSettings,
) where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (sentence_tx, sentence_rx) = mpsc::unbounded_channel::<String>();
    let failure_sent = Arc::new(AtomicBool::new(false));
    let mut producer = tokio::spawn(
        produce_sentences(
            model,
            history,
            audio,
            events.clone(),
            sentence_tx,
            Arc::clone(&failure_sent),
        )
        .in_current_span(),
    );
    let mut consumer = tokio::spawn(
        consume_sentences(
            sentence_rx,
            tts,
            voice,
            events.clone(),
            turn,
            recorder,
            failure_sent,
            ack,
        )
        .in_current_span(),
    );
    let _abort_on_drop = AbortOnDrop {
        handles: vec![producer.abort_handle(), consumer.abort_handle()],
    };
    let mut producer_done = false;
    let mut consumer_done = false;
    let mut succeeded = true;
    while !producer_done || !consumer_done {
        tokio::select! {
            result = &mut producer, if !producer_done => {
                producer_done = true;
                if !matches!(result, Ok(true)) {
                    succeeded = false;
                    consumer.abort();
                }
            }
            result = &mut consumer, if !consumer_done => {
                consumer_done = true;
                if !matches!(result, Ok(true)) {
                    succeeded = false;
                    producer.abort();
                }
            }
        }
    }
    if succeeded {
        let _ = events.send(ReplyEvent::Finished).await;
    }
}

async fn send_failed(
    events: &mpsc::Sender<ReplyEvent>,
    failure_sent: &AtomicBool,
    message: String,
) {
    if !failure_sent.swap(true, Ordering::SeqCst) {
        let _ = events.send(ReplyEvent::Failed(message)).await;
    }
}

async fn produce_sentences<M>(
    model: Arc<M>,
    history: Vec<ChatMessage>,
    audio: Option<Vec<u8>>,
    events: mpsc::Sender<ReplyEvent>,
    sentences: mpsc::UnboundedSender<String>,
    failure_sent: Arc<AtomicBool>,
) -> bool
where
    M: ChatModel,
{
    let mut deltas = match model.stream_reply(&history, audio.as_deref()).await {
        Ok(deltas) => deltas,
        Err(error) => {
            send_failed(
                &events,
                &failure_sent,
                format!("The language model could not reply: {error}"),
            )
            .await;
            return false;
        }
    };
    let mut splitter = SentenceSplitter::new();
    let mut first_sentence_sent = false;
    while let Some(delta) = deltas.next().await {
        let delta = match delta {
            Ok(delta) => delta,
            Err(error) => {
                send_failed(
                    &events,
                    &failure_sent,
                    format!("The language model could not reply: {error}"),
                )
                .await;
                return false;
            }
        };
        if events.send(ReplyEvent::Delta(delta.clone())).await.is_err() {
            return false;
        }
        for sentence in splitter.push(&delta) {
            if !first_sentence_sent {
                first_sentence_sent = true;
                if events
                    .send(ReplyEvent::FirstSentence(Instant::now()))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            if sentences.send(sentence).is_err() {
                return false;
            }
        }
    }
    if let Some(tail) = splitter.finish() {
        if !first_sentence_sent
            && events
                .send(ReplyEvent::FirstSentence(Instant::now()))
                .await
                .is_err()
        {
            return false;
        }
        if sentences.send(tail).is_err() {
            return false;
        }
    }
    if events
        .send(ReplyEvent::ModelFinished(Instant::now()))
        .await
        .is_err()
    {
        return false;
    }
    true
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn consume_sentences<T>(
    mut sentences: mpsc::UnboundedReceiver<String>,
    tts: Arc<T>,
    mut voice: watch::Receiver<bool>,
    events: mpsc::Sender<ReplyEvent>,
    turn: u64,
    recorder: Recorder,
    failure_sent: Arc<AtomicBool>,
    ack: AckSettings,
) -> bool
where
    T: Tts,
{
    let mut pending_first: Option<String> = None;
    if ack.enabled && *voice.borrow() {
        match tokio::time::timeout(ACK_DELAY, sentences.recv()).await {
            Ok(Some(sentence)) => pending_first = Some(sentence),
            Ok(None) => return true,
            Err(_) => {
                let line = ACK_LINES[usize::try_from(turn.saturating_sub(1) % 3).unwrap_or(0)];
                if !speak_ack(&tts, &mut voice, &events, line, turn, &recorder, &ack.cache).await {
                    return false;
                }
            }
        }
    }
    if let Some(sentence) = pending_first
        && *voice.borrow()
        && !speak_sentence(
            &tts,
            &mut voice,
            &events,
            &sentence,
            turn,
            &recorder,
            &failure_sent,
        )
        .await
    {
        return false;
    }
    while let Some(sentence) = sentences.recv().await {
        if *voice.borrow()
            && !speak_sentence(
                &tts,
                &mut voice,
                &events,
                &sentence,
                turn,
                &recorder,
                &failure_sent,
            )
            .await
        {
            return false;
        }
    }
    true
}

fn lock_ack_cache(
    ack_cache: &AckCache,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<&'static str, Bytes>> {
    ack_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn speak_ack<T: Tts>(
    tts: &Arc<T>,
    voice: &mut watch::Receiver<bool>,
    events: &mpsc::Sender<ReplyEvent>,
    line: &'static str,
    turn: u64,
    recorder: &Recorder,
    ack_cache: &AckCache,
) -> bool {
    tokio::select! {
        biased;
        () = voice_turned_off(voice) => true,
        keep_going = synthesize_ack(tts, events, line, turn, recorder, ack_cache) => keep_going,
    }
}

async fn synthesize_ack<T: Tts>(
    tts: &Arc<T>,
    events: &mpsc::Sender<ReplyEvent>,
    line: &'static str,
    turn: u64,
    recorder: &Recorder,
    ack_cache: &AckCache,
) -> bool {
    let started = Instant::now();
    let total_ms =
        |started: Instant| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let cached = lock_ack_cache(ack_cache).get(line).cloned();
    if let Some(pcm) = cached {
        if events.send(ReplyEvent::AckAudio(pcm)).await.is_err() {
            return false;
        }
        recorder.emit(
            "ack",
            json!({"turn": turn, "cached": true, "total_ms": total_ms(started)}),
        );
        return true;
    }
    let mut audio = match tts.synthesize(line).await {
        Ok(audio) => audio,
        Err(error) => {
            tracing::warn!(%error, "acknowledgement synthesis failed");
            recorder.emit(
                "ack",
                json!({"turn": turn, "cached": false, "total_ms": total_ms(started), "outcome": "failed"}),
            );
            return true;
        }
    };
    let mut pcm: Vec<u8> = Vec::new();
    while let Some(chunk) = audio.next().await {
        match chunk {
            Ok(chunk) if chunk.is_empty() => {}
            Ok(chunk) => {
                pcm.extend_from_slice(&chunk);
                if events.send(ReplyEvent::AckAudio(chunk)).await.is_err() {
                    return false;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "acknowledgement synthesis failed");
                recorder.emit(
                    "ack",
                    json!({"turn": turn, "cached": false, "total_ms": total_ms(started), "outcome": "failed"}),
                );
                return true;
            }
        }
    }
    if pcm.is_empty() {
        recorder.emit(
            "ack",
            json!({"turn": turn, "cached": false, "total_ms": total_ms(started), "outcome": "failed"}),
        );
        return true;
    }
    lock_ack_cache(ack_cache).insert(line, Bytes::from(pcm));
    recorder.emit(
        "ack",
        json!({"turn": turn, "cached": false, "total_ms": total_ms(started)}),
    );
    true
}

/// Speaks one sentence, giving it up as soon as voice turns off.
/// Returns false when the reply should stop (failure reported or session gone).
async fn speak_sentence<T: Tts>(
    tts: &Arc<T>,
    voice: &mut watch::Receiver<bool>,
    events: &mpsc::Sender<ReplyEvent>,
    sentence: &str,
    turn: u64,
    recorder: &Recorder,
    failure_sent: &AtomicBool,
) -> bool {
    tokio::select! {
        biased;
        () = voice_turned_off(voice) => true,
        keep_going = synthesize_sentence(tts, events, sentence, turn, recorder, failure_sent) => keep_going,
    }
}

async fn voice_turned_off(voice: &mut watch::Receiver<bool>) {
    if voice.wait_for(|on| !*on).await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn synthesize_sentence<T: Tts>(
    tts: &Arc<T>,
    events: &mpsc::Sender<ReplyEvent>,
    sentence: &str,
    turn: u64,
    recorder: &Recorder,
    failure_sent: &AtomicBool,
) -> bool {
    let tts_started = Instant::now();
    let mut first_byte_at: Option<Instant> = None;
    let mut audio_bytes: usize = 0;
    if events
        .send(ReplyEvent::Sentence(sentence.to_string()))
        .await
        .is_err()
    {
        return false;
    }
    let mut audio = match tts.synthesize(sentence).await {
        Ok(audio) => audio,
        Err(error) => {
            send_failed(
                events,
                failure_sent,
                format!("Speech synthesis could not speak: {error}"),
            )
            .await;
            return false;
        }
    };
    while let Some(chunk) = audio.next().await {
        match chunk {
            Ok(chunk) => {
                if first_byte_at.is_none() {
                    first_byte_at = Some(Instant::now());
                }
                audio_bytes += chunk.len();
                if events.send(ReplyEvent::Audio(chunk)).await.is_err() {
                    return false;
                }
            }
            Err(error) => {
                send_failed(
                    events,
                    failure_sent,
                    format!("Speech synthesis could not speak: {error}"),
                )
                .await;
                return false;
            }
        }
    }
    let ms_since = |at: Option<Instant>| {
        at.map(|at| u64::try_from(at.duration_since(tts_started).as_millis()).unwrap_or(u64::MAX))
    };
    recorder.emit(
        "tts_sentence",
        json!({
            "turn": turn,
            "chars": sentence.chars().count(),
            "first_byte_ms": ms_since(first_byte_at),
            "total_ms": u64::try_from(tts_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "audio_ms": u64::try_from(audio_bytes).unwrap_or(u64::MAX) * 1000 / 48_000,
        }),
    );
    true
}

async fn note_disconnect(
    events: &mpsc::Sender<RecognizerMessage>,
    message: String,
    is_fatal: bool,
    backoff: &mut Duration,
    reconnect_at: &mut Option<Instant>,
    fatal: &mut bool,
    recorder: &Recorder,
) {
    recorder.emit("stt_disconnect", json!({"reason": message}));
    let _ = events
        .send(RecognizerMessage::Disconnected {
            message,
            fatal: is_fatal,
        })
        .await;
    if is_fatal {
        *fatal = true;
    } else {
        *backoff = INITIAL_BACKOFF;
        *reconnect_at = Some(Instant::now() + *backoff);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "recognizer state is passed explicitly rather than bundled into a context type"
)]
async fn attempt_reconnect<F>(
    factory: &F,
    recognizer: &mut Option<F::Rec>,
    backoff: &mut Duration,
    reconnect_at: &mut Option<Instant>,
    events: &mpsc::Sender<RecognizerMessage>,
    fatal: &mut bool,
    recorder: &Recorder,
    attempt: &mut u64,
) where
    F: RecognizerFactory,
{
    *attempt += 1;
    let started = Instant::now();
    match factory.connect().await {
        Ok(rec) => {
            emit_stt_connect(recorder, started, "ok", *attempt);
            *recognizer = Some(rec);
            *backoff = INITIAL_BACKOFF;
            let _ = events.send(RecognizerMessage::Ready).await;
        }
        Err(error) => {
            emit_stt_connect(
                recorder,
                started,
                if error.is_fatal() {
                    "fatal"
                } else {
                    "retryable"
                },
                *attempt,
            );
            if error.is_fatal() {
                *fatal = true;
                recorder.emit("stt_disconnect", json!({"reason": error.to_string()}));
                let _ = events
                    .send(RecognizerMessage::Disconnected {
                        message: error.to_string(),
                        fatal: true,
                    })
                    .await;
            } else {
                *backoff = (*backoff * 2).min(MAX_BACKOFF);
                *reconnect_at = Some(Instant::now() + *backoff);
            }
        }
    }
}

fn emit_stt_connect(recorder: &Recorder, started: Instant, outcome: &str, attempt: u64) {
    recorder.emit(
        "stt_connect",
        json!({
            "model": recorder.stt_model(),
            "ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "outcome": outcome,
            "attempt": attempt,
        }),
    );
}

async fn initial_connect<F>(
    factory: &F,
    events: &mpsc::Sender<RecognizerMessage>,
    backoff: &mut Duration,
    reconnect_at: &mut Option<Instant>,
    fatal: &mut bool,
    recorder: &Recorder,
    attempt: &mut u64,
) -> Option<F::Rec>
where
    F: RecognizerFactory,
{
    *attempt += 1;
    let started = Instant::now();
    match factory.connect().await {
        Ok(rec) => {
            emit_stt_connect(recorder, started, "ok", *attempt);
            let _ = events.send(RecognizerMessage::Ready).await;
            Some(rec)
        }
        Err(error) => {
            emit_stt_connect(
                recorder,
                started,
                if error.is_fatal() {
                    "fatal"
                } else {
                    "retryable"
                },
                *attempt,
            );
            let is_fatal = error.is_fatal();
            note_disconnect(
                events,
                error.to_string(),
                is_fatal,
                backoff,
                reconnect_at,
                fatal,
                recorder,
            )
            .await;
            None
        }
    }
}

async fn run_recognizer<F>(
    factory: Arc<F>,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Sender<RecognizerMessage>,
    recorder: Recorder,
) where
    F: RecognizerFactory,
    F::Rec: Send,
{
    let mut backoff = INITIAL_BACKOFF;
    let mut reconnect_at: Option<Instant> = None;
    let mut fatal = false;

    let mut attempt: u64 = 0;
    let mut recognizer = initial_connect(
        &*factory,
        &events,
        &mut backoff,
        &mut reconnect_at,
        &mut fatal,
        &recorder,
        &mut attempt,
    )
    .await;

    loop {
        if recognizer.is_none() && !fatal && reconnect_at.is_none() {
            reconnect_at = Some(Instant::now() + backoff);
        }
        let reconnect_sleep = async {
            match reconnect_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let next_recognizer_event = async {
            match &mut recognizer {
                Some(rec) => rec.next_event().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            audio = audio_rx.recv() => {
                let Some(audio) = audio else {
                    if let Some(mut rec) = recognizer.take() {
                        rec.close().await;
                    }
                    return;
                };
                let send_result = match &mut recognizer {
                    Some(rec) => Some(rec.send_audio(&audio).await),
                    None => None,
                };
                if let Some(Err(error)) = send_result {
                    recognizer = None;
                    let is_fatal = error.is_fatal();
                    note_disconnect(
                        &events,
                        error.to_string(),
                        is_fatal,
                        &mut backoff,
                        &mut reconnect_at,
                        &mut fatal,
                        &recorder,
                    )
                    .await;
                }
            }
            event = next_recognizer_event => {
                match event {
                    Some(Ok(event)) => {
                        if events.send(RecognizerMessage::Event(event)).await.is_err() {
                            if let Some(mut rec) = recognizer.take() {
                                rec.close().await;
                            }
                            return;
                        }
                    }
                    other => {
                        recognizer = None;
                        let (message, is_fatal) = match other {
                            Some(Err(error)) => (error.to_string(), error.is_fatal()),
                            _ => ("recognizer connection closed".to_string(), false),
                        };
                        note_disconnect(
                            &events,
                            message,
                            is_fatal,
                            &mut backoff,
                            &mut reconnect_at,
                            &mut fatal,
                            &recorder,
                        )
                        .await;
                    }
                }
            }
            () = reconnect_sleep => {
                reconnect_at = None;
                attempt_reconnect(&*factory, &mut recognizer, &mut backoff, &mut reconnect_at, &events, &mut fatal, &recorder, &mut attempt).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ACK_LINES, AckCache, AckSettings, ActiveReply, ForwardClock, ReplyEvent, model_ms,
        record_assistant_history, run_reply,
    };
    use crate::model::{ChatMessage, ChatModel, DeltaStream, ModelError};
    use crate::recorder::{RecordingConfig, start_recording};
    use crate::tts::{AudioStream, Tts, TtsError};
    use bytes::Bytes;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{Notify, mpsc, watch};
    use tokio::time::Instant;

    struct FakeModel;

    impl ChatModel for FakeModel {
        fn stream_reply(
            &self,
            _history: &[ChatMessage],
            _audio: Option<&[u8]>,
        ) -> impl std::future::Future<Output = Result<DeltaStream, ModelError>> + Send {
            let deltas = vec![
                Ok("First. ".to_string()),
                Ok("Second. ".to_string()),
                Ok("Third.".to_string()),
            ];
            let stream: DeltaStream = Box::pin(futures_util::stream::iter(deltas));
            std::future::ready(Ok(stream))
        }
    }

    struct ManySentenceModel;

    impl ChatModel for ManySentenceModel {
        fn stream_reply(
            &self,
            _history: &[ChatMessage],
            _audio: Option<&[u8]>,
        ) -> impl std::future::Future<Output = Result<DeltaStream, ModelError>> + Send {
            let deltas: Vec<Result<String, ModelError>> = (1..=12)
                .map(|index| Ok(format!("Sentence {index}. ")))
                .collect();
            let stream: DeltaStream = Box::pin(futures_util::stream::iter(deltas));
            std::future::ready(Ok(stream))
        }
    }

    struct FakeTts {
        spoken: Arc<Mutex<Vec<String>>>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        block_first: AtomicBool,
    }

    impl Tts for FakeTts {
        async fn synthesize(&self, text: &str) -> Result<AudioStream, TtsError> {
            self.spoken
                .lock()
                .expect("spoken lock")
                .push(text.to_string());
            self.entered.notify_one();
            if self.block_first.swap(false, Ordering::SeqCst) {
                self.release.notified().await;
            }
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                Bytes::from_static(b"audio"),
            )])))
        }
    }

    struct GatedModel {
        release: Arc<Notify>,
    }

    impl ChatModel for GatedModel {
        fn stream_reply(
            &self,
            _history: &[ChatMessage],
            _audio: Option<&[u8]>,
        ) -> impl std::future::Future<Output = Result<DeltaStream, ModelError>> + Send {
            let release = Arc::clone(&self.release);
            let stream: DeltaStream = Box::pin(futures_util::stream::once(async move {
                release.notified().await;
                Ok("First answer.".to_string())
            }));
            std::future::ready(Ok(stream))
        }
    }

    struct AckFailTts {
        spoken: Arc<Mutex<Vec<String>>>,
        entered: Arc<Notify>,
    }

    impl Tts for AckFailTts {
        fn synthesize(
            &self,
            text: &str,
        ) -> impl std::future::Future<Output = Result<AudioStream, TtsError>> + Send {
            self.spoken
                .lock()
                .expect("spoken lock")
                .push(text.to_string());
            self.entered.notify_one();
            let result = if ACK_LINES.contains(&text) {
                Err(TtsError("ack failed".to_string()))
            } else {
                Ok(
                    Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from_static(
                        b"audio",
                    ))])) as AudioStream,
                )
            };
            std::future::ready(result)
        }
    }

    fn new_ack_cache() -> AckCache {
        Arc::new(Mutex::new(std::collections::HashMap::new()))
    }

    fn describe(event: &ReplyEvent) -> String {
        match event {
            ReplyEvent::Delta(text) => format!("delta:{text}"),
            ReplyEvent::Sentence(sentence) => format!("sentence:{sentence}"),
            ReplyEvent::Audio(_) => "audio".to_string(),
            ReplyEvent::AckAudio(_) => "ack_audio".to_string(),
            ReplyEvent::Finished => "finished".to_string(),
            ReplyEvent::Failed(message) => format!("failed:{message}"),
            ReplyEvent::FirstSentence(_) => "first_sentence".to_string(),
            ReplyEvent::ModelFinished(_) => "model_finished".to_string(),
        }
    }

    async fn next_description(events_rx: &mut mpsc::Receiver<ReplyEvent>) -> String {
        let event = tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
            .await
            .expect("event should arrive within timeout")
            .expect("events channel should stay open");
        describe(&event)
    }

    #[tokio::test]
    async fn deltas_arrive_while_first_sentence_is_blocked_in_tts() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let tts = Arc::new(FakeTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            block_first: AtomicBool::new(true),
        });
        let (_voice_tx, voice_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(FakeModel),
            tts,
            voice_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            AckSettings {
                enabled: false,
                cache: new_ack_cache(),
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("tts should start within timeout");
        let mut seen = Vec::new();
        loop {
            let description = next_description(&mut events_rx).await;
            let is_later_delta = description == "delta:Second. ";
            seen.push(description);
            if is_later_delta {
                break;
            }
        }
        assert_eq!(
            spoken.lock().expect("spoken lock").as_slice(),
            ["First."],
            "later delta should arrive while only the first sentence is in tts: {seen:?}"
        );
        assert!(
            !seen.iter().any(|event| event == "finished"),
            "finished must not arrive while tts is blocked: {seen:?}"
        );
        release.notify_one();
        let mut rest = Vec::new();
        loop {
            let description = next_description(&mut events_rx).await;
            let is_finished = description == "finished";
            rest.push(description);
            if is_finished {
                break;
            }
        }
        handle.await.expect("reply task should finish");
        assert_eq!(
            spoken.lock().expect("spoken lock").as_slice(),
            ["First.", "Second.", "Third."],
            "sentences should reach tts in stream order"
        );
        let sentences: Vec<&str> = seen
            .iter()
            .chain(rest.iter())
            .filter_map(|event| event.strip_prefix("sentence:"))
            .collect();
        assert_eq!(sentences, ["First.", "Second.", "Third."]);
        let last_audio = rest
            .iter()
            .rposition(|event| event == "audio")
            .expect("last sentence audio should arrive");
        assert!(
            last_audio < rest.len() - 1,
            "finished should arrive only after the last sentence audio: {rest:?}"
        );
    }

    #[tokio::test]
    async fn long_reply_streams_every_delta_while_first_sentence_is_blocked_in_tts() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let tts = Arc::new(FakeTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            block_first: AtomicBool::new(true),
        });
        let (_voice_tx, voice_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(ManySentenceModel),
            tts,
            voice_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            AckSettings {
                enabled: false,
                cache: new_ack_cache(),
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("tts should start within timeout");
        let mut deltas = 0;
        let mut model_finished = false;
        while !model_finished {
            let description = next_description(&mut events_rx).await;
            if description.starts_with("delta:") {
                deltas += 1;
            }
            model_finished = description == "model_finished";
        }
        assert_eq!(
            deltas, 12,
            "every delta should arrive while the first sentence is blocked in tts"
        );
        assert_eq!(
            spoken.lock().expect("spoken lock").as_slice(),
            ["Sentence 1."],
            "model_finished should be stamped before tts works through the queue"
        );
        release.notify_one();
        loop {
            if next_description(&mut events_rx).await == "finished" {
                break;
            }
        }
        handle.await.expect("reply task should finish");
        assert_eq!(spoken.lock().expect("spoken lock").len(), 12);
    }

    #[tokio::test]
    async fn voice_off_streams_deltas_without_synthesis_and_finishes() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let tts = Arc::new(FakeTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            block_first: AtomicBool::new(false),
        });
        let (_voice_tx, voice_rx) = watch::channel(false);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(FakeModel),
            tts,
            voice_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            AckSettings {
                enabled: false,
                cache: new_ack_cache(),
            },
        ));
        let mut seen = Vec::new();
        loop {
            let description = next_description(&mut events_rx).await;
            let is_finished = description == "finished";
            seen.push(description);
            if is_finished {
                break;
            }
        }
        handle.await.expect("reply task should finish");
        assert!(seen.contains(&"delta:Second. ".to_string()), "{seen:?}");
        assert!(
            !seen
                .iter()
                .any(|event| event.starts_with("sentence:") || event == "audio"),
            "voice off should not synthesize: {seen:?}"
        );
        assert!(spoken.lock().expect("spoken lock").is_empty());
    }

    async fn collect_until_finished(events_rx: &mut mpsc::Receiver<ReplyEvent>) -> Vec<String> {
        let mut seen = Vec::new();
        loop {
            let description = next_description(events_rx).await;
            let is_finished = description == "finished";
            seen.push(description);
            if is_finished {
                break;
            }
        }
        seen
    }

    #[tokio::test]
    async fn slow_reply_speaks_ack_before_first_sentence() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let tts = Arc::new(FakeTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            block_first: AtomicBool::new(false),
        });
        let release = Arc::new(Notify::new());
        let (_voice_tx, voice_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(GatedModel {
                release: Arc::clone(&release),
            }),
            tts,
            voice_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            AckSettings {
                enabled: true,
                cache: new_ack_cache(),
            },
        ));
        let first = next_description(&mut events_rx).await;
        assert_eq!(first, "ack_audio", "ack should arrive before any sentence");
        release.notify_one();
        let rest = collect_until_finished(&mut events_rx).await;
        handle.await.expect("reply task should finish");
        assert!(
            rest.iter().any(|event| event == "sentence:First answer."),
            "real sentence should follow the ack: {rest:?}"
        );
        assert!(
            !rest.iter().any(|event| event.starts_with("failed")),
            "reply should not fail: {rest:?}"
        );
        assert!(
            !rest.iter().any(|event| event
                .strip_prefix("sentence:")
                .is_some_and(|s| ACK_LINES.contains(&s))),
            "ack text must not appear as a sentence: {rest:?}"
        );
        let spoken = spoken.lock().expect("spoken lock").clone();
        assert_eq!(spoken.first().map(String::as_str), Some(ACK_LINES[0]));
        assert!(spoken.contains(&"First answer.".to_string()), "{spoken:?}");
    }

    #[tokio::test]
    async fn fast_reply_speaks_no_ack() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let tts = Arc::new(FakeTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            block_first: AtomicBool::new(false),
        });
        let (_voice_tx, voice_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(FakeModel),
            tts,
            voice_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            AckSettings {
                enabled: true,
                cache: new_ack_cache(),
            },
        ));
        let seen = collect_until_finished(&mut events_rx).await;
        handle.await.expect("reply task should finish");
        assert!(
            !seen.iter().any(|event| event == "ack_audio"),
            "fast reply should not speak an ack: {seen:?}"
        );
        assert!(
            !spoken
                .lock()
                .expect("spoken lock")
                .iter()
                .any(|text| ACK_LINES.contains(&text.as_str())),
            "fast reply should not synthesize an ack"
        );
    }

    #[tokio::test]
    async fn second_slow_turn_replays_cached_ack() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let tts = Arc::new(FakeTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            block_first: AtomicBool::new(false),
        });
        let cache = new_ack_cache();
        for turn in [1_u64, 4_u64] {
            let release = Arc::new(Notify::new());
            let (_voice_tx, voice_rx) = watch::channel(true);
            let (events_tx, mut events_rx) = mpsc::channel(64);
            let recorder =
                start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
            let handle = tokio::spawn(run_reply(
                Arc::new(GatedModel {
                    release: Arc::clone(&release),
                }),
                Arc::clone(&tts),
                voice_rx,
                Vec::new(),
                None,
                events_tx,
                turn,
                recorder,
                AckSettings {
                    enabled: true,
                    cache: Arc::clone(&cache),
                },
            ));
            let first = next_description(&mut events_rx).await;
            assert_eq!(first, "ack_audio", "turn {turn} should speak an ack");
            release.notify_one();
            let rest = collect_until_finished(&mut events_rx).await;
            handle.await.expect("reply task should finish");
            assert!(
                rest.iter().any(|event| event == "sentence:First answer."),
                "turn {turn} real sentence should follow: {rest:?}"
            );
        }
        let ack_calls = spoken
            .lock()
            .expect("spoken lock")
            .iter()
            .filter(|text| text.as_str() == ACK_LINES[0])
            .count();
        assert_eq!(ack_calls, 1, "cached ack should be synthesized only once");
    }

    #[tokio::test]
    async fn ack_tts_failure_still_delivers_reply() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(Notify::new());
        let tts = Arc::new(AckFailTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::clone(&entered),
        });
        let release = Arc::new(Notify::new());
        let (_voice_tx, voice_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(GatedModel {
                release: Arc::clone(&release),
            }),
            tts,
            voice_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            AckSettings {
                enabled: true,
                cache: new_ack_cache(),
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("ack synthesis should be attempted within timeout");
        release.notify_one();
        let seen = collect_until_finished(&mut events_rx).await;
        handle.await.expect("reply task should finish");
        assert!(
            !seen.iter().any(|event| event == "ack_audio"),
            "failed ack should produce no ack audio: {seen:?}"
        );
        assert!(
            seen.iter().any(|event| event == "sentence:First answer."),
            "real sentence should still arrive: {seen:?}"
        );
        assert!(
            seen.iter().any(|event| event == "audio"),
            "real audio should still arrive: {seen:?}"
        );
        assert!(
            !seen.iter().any(|event| event.starts_with("failed")),
            "ack failure must not fail the reply: {seen:?}"
        );
    }

    #[tokio::test]
    async fn cut_off_after_ack_audio_records_no_unspoken_reply_text() {
        let make_active = |speaking_sent: bool| ActiveReply {
            handle: tokio::spawn(async {}),
            turn: 1,
            started_at: Instant::now(),
            sent_sentences: Vec::new(),
            text: "One. Two. Three.".to_string(),
            spoken_throughout: true,
            speaking_sent,
            first_delta_at: None,
            model_finished_at: None,
            first_sentence_at: None,
            first_audio_at: None,
            ack_first_audio_at: None,
            audio_bytes: 0,
            input_audio_ms: None,
        };
        let mut history = Vec::new();
        record_assistant_history(&mut history, &make_active(true), false);
        assert!(
            history.is_empty(),
            "only the ack was heard, so no reply text belongs in history: {history:?}"
        );
        let mut history = Vec::new();
        record_assistant_history(&mut history, &make_active(false), false);
        assert_eq!(
            history,
            vec![ChatMessage::assistant("One. Two. Three.".to_string())],
            "text shown before any audio is kept as before"
        );
    }

    #[test]
    fn model_ms_ends_at_model_finish_when_known_and_at_record_time_otherwise() {
        let started = Instant::now();
        let model_finished = started + Duration::from_millis(120);
        let recorded = started + Duration::from_millis(5_000);
        assert_eq!(model_ms(started, Some(model_finished), recorded), 120);
        assert_eq!(model_ms(started, None, recorded), 5_000);
    }

    #[test]
    fn forward_clock_maps_processed_time_to_when_it_was_forwarded() {
        let base = Instant::now();
        let mut clock = ForwardClock::default();
        clock.push(3840, base);
        clock.push(3840, base + Duration::from_millis(150));
        assert_eq!(clock.instant_for(0), Some(base));
        assert_eq!(clock.instant_for(80), Some(base));
        assert_eq!(
            clock.instant_for(100),
            Some(base + Duration::from_millis(150))
        );
        assert_eq!(clock.instant_for(10_000), None);
        clock.reset();
        assert_eq!(clock.instant_for(0), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_cannot_receive_does_not_hold_up_the_handoff() {
        let dir = std::env::temp_dir().join(format!("thalvi-close-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(crate::store::SessionStore::open(&dir).unwrap());
        let (persistence, _) =
            super::Persistence::<FakeModel>::attach(Arc::clone(&store), None, None).await;
        let history = vec![
            ChatMessage::user("question".to_string()),
            ChatMessage::assistant("Partial words.".to_string()),
        ];
        let mut stalled = Box::pin(futures_util::sink::unfold((), |(), _message| {
            std::future::pending::<Result<(), std::convert::Infallible>>()
        }));
        let recorder = start_recording(RecordingConfig::disabled(), "s".to_string()).recorder;
        let closing = tokio::spawn(async move {
            super::close_session(
                persistence,
                &history,
                1,
                &recorder,
                Some(crate::store::Detach::Moved),
                &mut stalled,
            )
            .await;
        });

        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let saved = store.list().len();
        closing.abort();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            saved, 1,
            "the history was not saved while the send was stalled"
        );
    }
}

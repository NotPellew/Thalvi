//! Owns one voice session: recognizer, reply pipeline, and the client socket.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::Instrument;

use crate::model::{ChatMessage, ChatModel};
use crate::recognizer::{Recognizer, RecognizerEvent, RecognizerFactory};
use crate::recorder::{AudioBuffer, Recorder, RecordingConfig, start_recording};
use crate::sentence::SentenceSplitter;
use crate::store::SessionStore;
use crate::tts::Tts;

const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

enum ReplyEvent {
    Delta(String),
    Sentence(String),
    Audio(Bytes),
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
    first_sentence_at: Option<Instant>,
    first_audio_at: Option<Instant>,
    audio_bytes: usize,
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
    fn new(recorder: Recorder, next_turn: u64) -> Self {
        Self {
            recorder,
            pending: std::collections::HashMap::new(),
            audio: AudioBuffer::default(),
            forward_clock: ForwardClock::default(),
            next_turn,
            last_reply_turn: 0,
            last_reply_audio_bytes: 0,
        }
    }
}

/// Per-session recording state: pending voice turns and forwarded audio.
struct TurnLog {
    recorder: Recorder,
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

const MAX_TEXT_CHARS: usize = 20_000;

/// Runs one client session until its socket closes.
pub async fn handle_session<F, M, T>(
    socket: WebSocket,
    factory: F,
    model: Arc<M>,
    tts: Arc<T>,
    store: Arc<SessionStore>,
    resume: Option<String>,
    recording_config: RecordingConfig,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let factory = Arc::new(factory);
    let session_started = Instant::now();
    let resumed = resume.as_deref().is_some_and(|id| store.contains(id));
    let (session_id, mut history, restored_next_turn) = resume
        .as_deref()
        .and_then(|id| {
            store
                .take(id)
                .map(|(history, next_turn)| (id.to_string(), history, next_turn))
        })
        .unwrap_or_else(|| (store.new_id(), Vec::new(), 0));
    let recording = start_recording(recording_config, session_id.clone());
    let mut turn_log = TurnLog::new(recording.recorder.clone(), restored_next_turn);
    turn_log
        .recorder
        .emit("session_open", json!({"resumed": resumed}));
    send_session_opening(&mut sink, &session_id, &history).await;
    send_state(&mut sink, "idle").await;
    tracing::info!("session opened");

    let voice = watch::Sender::new(false);
    let mut voice_on = false;
    let mut recognizer: Option<RecognizerHandle> = None;
    let mut rec_rx: Option<mpsc::Receiver<RecognizerMessage>> = None;
    let mut reply: Option<ActiveReply> = None;
    let mut reply_rx: Option<mpsc::Receiver<ReplyEvent>> = None;
    let mut audio_sent = false;

    loop {
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
            client_message = stream.next() => {
                match client_message {
                    Some(Ok(Message::Binary(audio))) => {
                        let forwarded = recognizer.as_ref().is_some_and(|handle| handle.ready)
                            && recognizer.as_ref().is_some_and(|handle| handle.audio_tx.try_send(audio.to_vec()).is_ok());
                        if forwarded {
                            turn_log.forward_clock.push(audio.len(), Instant::now());
                            if turn_log.recorder.audio_enabled() {
                                turn_log.audio.push(&audio);
                            }
                        } else {
                            tracing::debug!("mic frame dropped");
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        handle_client_text(&text, &mut sink, &mut recognizer, &mut rec_rx, &factory, &voice, &mut voice_on, &mut history, &mut reply, &mut reply_rx, &mut audio_sent, &model, &tts, &mut turn_log).await;
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
                handle_recognizer_message(recognizer_message, &mut sink, &mut recognizer, &mut rec_rx, &mut history, &mut reply, &mut reply_rx, &mut audio_sent, &model, &tts, &voice, &mut voice_on, &mut turn_log).await;
            }
            event = reply_event => {
                let Some(event) = event else {
                    reply_rx = None;
                    continue;
                };
                handle_reply_event(event, &mut sink, &mut history, &mut reply, &mut reply_rx, &mut audio_sent, Voice::new(voice_on, recognizer.as_ref()), &mut turn_log).await;
            }
        }
    }

    cut_off_reply(&mut history, &mut reply, &mut reply_rx, &turn_log);
    if let Some(handle) = recognizer.take() {
        handle.task.abort();
    }
    tracing::info!("session closed");
    let next_turn = turn_log.next_turn;
    store.save(session_id, history, next_turn);
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
    audio_sent: &mut bool,
    model: &Arc<M>,
    tts: &Arc<T>,
    turn_log: &mut TurnLog,
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
                audio_sent,
                Voice::new(*voice_on, recognizer.as_ref()),
                "cancel",
                turn_log,
            )
            .await;
        }
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
                audio_sent,
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
                    audio_sent,
                    model,
                    tts,
                    voice,
                    Voice::new(*voice_on, recognizer.as_ref()),
                    "text",
                    text.clone(),
                    turn_log,
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
    audio_sent: &mut bool,
    model: &Arc<M>,
    tts: &Arc<T>,
    voice: &watch::Sender<bool>,
    voice_on: &mut bool,
    turn_log: &mut TurnLog,
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
                audio_sent,
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
            let pending = turn_log.pending.entry(turn_id).or_default();
            pending.end_ms = audio_processed_ms;
        }
        Some(RecognizerMessage::Event(RecognizerEvent::Partial { text })) => {
            send_json(sink, &json!({"type": "partial", "text": text})).await;
        }
        Some(RecognizerMessage::Event(RecognizerEvent::SpeechComplete { turn_id, text })) => {
            if !text.trim().is_empty() {
                let pending = turn_log.pending.remove(&turn_id).unwrap_or_default();
                let turn = begin_turn(
                    sink,
                    history,
                    reply,
                    reply_rx,
                    audio_sent,
                    model,
                    tts,
                    voice,
                    Voice::new(*voice_on, recognizer.as_ref()),
                    "voice",
                    text.clone(),
                    turn_log,
                )
                .await;
                record_voice_turn(turn_log, turn, &pending, &text);
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
                stop_voice(sink, recognizer, rec_rx, voice, reply, audio_sent).await;
            } else if *voice_on && reply.is_none() {
                send_state(sink, "starting").await;
            }
        }
        Some(RecognizerMessage::Ready) => {
            turn_log.audio.reset();
            turn_log.forward_clock.reset();
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

fn record_voice_turn(turn_log: &mut TurnLog, turn: u64, pending: &PendingVoice, text: &str) {
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
    if recorder.audio_enabled() {
        let start = pending.start_ms.unwrap_or(0);
        let end = pending.end_ms.unwrap_or_else(|| turn_log.audio.end_ms());
        let pcm = turn_log.audio.cut(start, end, 300);
        if !pcm.is_empty() {
            recorder.emit_wav(turn, pcm);
            if recorder.metrics_enabled() || recorder.text_enabled() {
                fields["audio"] = json!(format!("{turn}.wav"));
            }
        }
        turn_log.audio.prune(end.saturating_sub(300));
    }
    recorder.emit("turn", fields);
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
    audio_sent: &mut bool,
    recorder: Recorder,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
{
    if !on {
        stop_voice(sink, recognizer, rec_rx, voice, reply, audio_sent).await;
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
    audio_sent: &mut bool,
) {
    voice.send_replace(false);
    if let Some(handle) = recognizer.take() {
        handle.task.abort();
    }
    *rec_rx = None;
    *audio_sent = false;
    if let Some(active) = reply {
        active.spoken_throughout = false;
        send_state(sink, "thinking").await;
    } else {
        send_state(sink, "idle").await;
    }
}

enum ClientText {
    Cancel,
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
    audio_sent: &mut bool,
    model: &Arc<M>,
    tts: &Arc<T>,
    voice: &watch::Sender<bool>,
    state: Voice,
    source: &'static str,
    text: String,
    turn_log: &mut TurnLog,
) -> u64
where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    tracing::info!(source, chars = text.chars().count(), "user turn");
    interrupt(
        sink, history, reply, reply_rx, audio_sent, state, "new turn", turn_log,
    )
    .await;
    turn_log.next_turn += 1;
    let turn = turn_log.next_turn;
    history.push(ChatMessage::user(text.clone()));
    send_json(sink, &json!({"type": "user", "text": text})).await;
    send_state(sink, "thinking").await;
    let (active, events_rx) = start_reply(
        Arc::clone(model),
        Arc::clone(tts),
        voice.subscribe(),
        history.clone(),
        turn,
        turn_log.recorder.clone(),
    );
    *reply = Some(active);
    *reply_rx = Some(events_rx);
    turn
}

fn start_reply<M, T>(
    model: Arc<M>,
    tts: Arc<T>,
    voice: watch::Receiver<bool>,
    history: Vec<ChatMessage>,
    turn: u64,
    recorder: Recorder,
) -> (ActiveReply, mpsc::Receiver<ReplyEvent>)
where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (events_tx, events_rx) = mpsc::channel::<ReplyEvent>(64);
    let spoken_throughout = *voice.borrow();
    let active = ActiveReply {
        handle: tokio::spawn(
            run_reply(model, tts, voice, history, events_tx, turn, recorder).in_current_span(),
        ),
        turn,
        started_at: Instant::now(),
        sent_sentences: Vec::new(),
        text: String::new(),
        spoken_throughout,
        speaking_sent: false,
        first_delta_at: None,
        first_sentence_at: None,
        first_audio_at: None,
        audio_bytes: 0,
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
    audio_sent: &mut bool,
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
        ReplyEvent::Sentence(sentence) => {
            if let Some(active) = reply {
                if active.first_sentence_at.is_none() {
                    active.first_sentence_at = Some(Instant::now());
                }
                active.sent_sentences.push(sentence);
            }
        }
        ReplyEvent::Audio(_) if !state.on => {}
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
            *audio_sent = true;
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
        "model_ms": u64::try_from(active.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        "tts_model": recorder.tts_model(),
        "voice": recorder.tts_voice(),
        "first_audio_ms": ms_since(active.first_audio_at),
        "sentences": active.sent_sentences.len(),
        "audio_ms": u64::try_from(active.audio_bytes).unwrap_or(u64::MAX) * 1000 / 48_000,
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
    let text = if finished || !active.spoken_throughout || spoken.trim().is_empty() {
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
    audio_sent: &mut bool,
    state: Voice,
    reason: &'static str,
    turn_log: &mut TurnLog,
) {
    if reply.is_none() && !*audio_sent {
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
    *audio_sent = false;
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
                    ReplyEvent::Sentence(sentence) => {
                        if active.first_sentence_at.is_none() {
                            active.first_sentence_at = Some(Instant::now());
                        }
                        active.sent_sentences.push(sentence);
                    }
                    ReplyEvent::Delta(text) => active.text.push_str(&text),
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

async fn run_reply<M, T>(
    model: Arc<M>,
    tts: Arc<T>,
    mut voice: watch::Receiver<bool>,
    history: Vec<ChatMessage>,
    events: mpsc::Sender<ReplyEvent>,
    turn: u64,
    recorder: Recorder,
) where
    M: ChatModel,
    T: Tts,
{
    let mut deltas = match model.stream_reply(&history).await {
        Ok(deltas) => deltas,
        Err(error) => {
            let _ = events
                .send(ReplyEvent::Failed(format!(
                    "The language model could not reply: {error}"
                )))
                .await;
            return;
        }
    };
    let mut splitter = SentenceSplitter::new();
    while let Some(delta) = deltas.next().await {
        let delta = match delta {
            Ok(delta) => delta,
            Err(error) => {
                let _ = events
                    .send(ReplyEvent::Failed(format!(
                        "The language model could not reply: {error}"
                    )))
                    .await;
                return;
            }
        };
        if events.send(ReplyEvent::Delta(delta.clone())).await.is_err() {
            return;
        }
        for sentence in splitter.push(&delta) {
            if *voice.borrow()
                && !speak_sentence(&tts, &mut voice, &events, &sentence, turn, &recorder).await
            {
                return;
            }
        }
    }
    if let Some(tail) = splitter.finish()
        && *voice.borrow()
        && !speak_sentence(&tts, &mut voice, &events, &tail, turn, &recorder).await
    {
        return;
    }
    let _ = events.send(ReplyEvent::Finished).await;
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
) -> bool {
    tokio::select! {
        biased;
        () = voice_turned_off(voice) => true,
        keep_going = synthesize_sentence(tts, events, sentence, turn, recorder) => keep_going,
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
            let _ = events
                .send(ReplyEvent::Failed(format!(
                    "Speech synthesis could not speak: {error}"
                )))
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
                let _ = events
                    .send(ReplyEvent::Failed(format!(
                        "Speech synthesis could not speak: {error}"
                    )))
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
    use super::ForwardClock;
    use std::time::Duration;
    use tokio::time::Instant;

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
}

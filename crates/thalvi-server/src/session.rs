//! Owns one voice session: recognizer, reply pipeline, and the client socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::Instrument;

use crate::model::{ChatMessage, ChatModel};
use crate::recognizer::{Recognizer, RecognizerEvent, RecognizerFactory};
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
    started_at: Instant,
    sent_sentences: Vec<String>,
    text: String,
    speaking_sent: bool,
    first_audio_at: Option<Instant>,
    audio_bytes: usize,
}

struct RecognizerHandle {
    audio_tx: mpsc::Sender<Vec<u8>>,
    task: JoinHandle<()>,
    ready: bool,
    dead: bool,
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
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let factory = Arc::new(factory);
    let (session_id, mut history) = resume
        .as_deref()
        .and_then(|id| store.take(id).map(|history| (id.to_string(), history)))
        .unwrap_or_else(|| (store.new_id(), Vec::new()));
    let turns: Vec<serde_json::Value> = history
        .iter()
        .map(|message| json!({"role": message.role, "text": message.content}))
        .collect();
    send_json(
        &mut sink,
        &json!({"type": "session", "id": session_id.clone(), "turns": turns}),
    )
    .await;
    send_state(&mut sink, "idle").await;
    tracing::info!("session opened");

    let voice = Arc::new(AtomicBool::new(false));
    let mut voice_on = false;
    let mut recognizer: Option<RecognizerHandle> = None;
    let mut rec_rx: Option<mpsc::Receiver<RecognizerMessage>> = None;
    let mut reply: Option<ActiveReply> = None;
    let mut reply_rx: Option<mpsc::Receiver<ReplyEvent>> = None;
    let mut playing_until: Option<Instant> = None;

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
                        if !forwarded {
                            tracing::debug!("mic frame dropped");
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match parse_client_text(&text) {
                            ClientText::Cancel => {
                                interrupt(&mut sink, &mut history, &mut reply, &mut reply_rx, &mut playing_until, voice_on, "cancel").await;
                            }
                            ClientText::Voice(on) => {
                                voice_on = on;
                                set_voice(on, &mut sink, &mut recognizer, &mut rec_rx, &factory, &voice, reply.is_some(), &mut playing_until).await;
                            }
                            ClientText::Text(raw) => {
                                match validate_text(&raw) {
                                    Ok(text) => {
                                        begin_turn(&mut sink, &mut history, &mut reply, &mut reply_rx, &mut playing_until, &model, &tts, &voice, voice_on, text).await;
                                    }
                                    Err(message) => {
                                        send_json(&mut sink, &json!({"type": "error", "message": message})).await;
                                    }
                                }
                            }
                            ClientText::Other => {}
                        }
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
                handle_recognizer_message(recognizer_message, &mut sink, &mut recognizer, &mut history, &mut reply, &mut reply_rx, &mut playing_until, &model, &tts, &voice, voice_on).await;
            }
            event = reply_event => {
                let Some(event) = event else {
                    reply_rx = None;
                    continue;
                };
                handle_reply_event(event, &mut sink, &mut history, &mut reply, &mut reply_rx, &mut playing_until, voice_on).await;
            }
        }
    }

    if let Some(active) = reply.take() {
        active.handle.abort();
    }
    if let Some(handle) = recognizer.take() {
        handle.task.abort();
    }
    tracing::info!("session closed");
    store.save(session_id, history);
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn handle_recognizer_message<M, T>(
    recognizer_message: Option<RecognizerMessage>,
    sink: &mut (impl SinkExt<Message> + Unpin),
    recognizer: &mut Option<RecognizerHandle>,
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    playing_until: &mut Option<Instant>,
    model: &Arc<M>,
    tts: &Arc<T>,
    voice: &Arc<AtomicBool>,
    voice_on: bool,
) where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    if let Some(RecognizerMessage::Event(event)) = &recognizer_message {
        tracing::debug!(?event, "recognizer event");
    }
    match recognizer_message {
        Some(RecognizerMessage::Event(RecognizerEvent::SpeechStart)) => {
            interrupt(
                sink,
                history,
                reply,
                reply_rx,
                playing_until,
                voice_on,
                "speech start",
            )
            .await;
        }
        Some(RecognizerMessage::Event(RecognizerEvent::Partial { text })) => {
            send_json(sink, &json!({"type": "partial", "text": text})).await;
        }
        Some(RecognizerMessage::Event(RecognizerEvent::SpeechComplete { text })) => {
            if !text.trim().is_empty() {
                begin_turn(
                    sink,
                    history,
                    reply,
                    reply_rx,
                    playing_until,
                    model,
                    tts,
                    voice,
                    voice_on,
                    text,
                )
                .await;
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
                if fatal {
                    handle.dead = true;
                }
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
            if !fatal && voice_on && reply.is_none() {
                send_state(sink, "starting").await;
            }
        }
        Some(RecognizerMessage::Ready) => {
            if let Some(handle) = recognizer {
                handle.ready = true;
            }
            if voice_on && reply.is_none() {
                send_state(sink, "listening").await;
            }
        }
        None => {}
    }
}

fn spawn_recognizer<F>(factory: &Arc<F>) -> (RecognizerHandle, mpsc::Receiver<RecognizerMessage>)
where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
{
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(32);
    let (rec_tx, rec_rx) = mpsc::channel::<RecognizerMessage>(32);
    let task =
        tokio::spawn(run_recognizer(Arc::clone(factory), audio_rx, rec_tx).in_current_span());
    (
        RecognizerHandle {
            audio_tx,
            task,
            ready: false,
            dead: false,
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
    voice: &Arc<AtomicBool>,
    reply_active: bool,
    playing_until: &mut Option<Instant>,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
{
    voice.store(on, Ordering::Relaxed);
    if on {
        let dead = recognizer.as_ref().is_some_and(|handle| handle.dead);
        if recognizer.is_none() || dead {
            if let Some(old) = recognizer.take() {
                old.task.abort();
            }
            let (handle, receiver) = spawn_recognizer(factory);
            *recognizer = Some(handle);
            *rec_rx = Some(receiver);
            send_state(sink, "starting").await;
        }
    } else {
        if let Some(handle) = recognizer.take() {
            handle.task.abort();
        }
        *rec_rx = None;
        if reply_active {
            send_state(sink, "thinking").await;
        } else {
            *playing_until = None;
            send_state(sink, "idle").await;
        }
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
    playing_until: &mut Option<Instant>,
    model: &Arc<M>,
    tts: &Arc<T>,
    voice: &Arc<AtomicBool>,
    voice_on: bool,
    text: String,
) where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    tracing::info!(%text, "user turn");
    if reply.is_some() {
        interrupt(
            sink,
            history,
            reply,
            reply_rx,
            playing_until,
            voice_on,
            "new turn",
        )
        .await;
    }
    *playing_until = None;
    history.push(ChatMessage::user(text.clone()));
    send_json(sink, &json!({"type": "user", "text": text})).await;
    send_state(sink, "thinking").await;
    let (active, events_rx) = start_reply(
        Arc::clone(model),
        Arc::clone(tts),
        Arc::clone(voice),
        history.clone(),
    );
    *reply = Some(active);
    *reply_rx = Some(events_rx);
}

fn start_reply<M, T>(
    model: Arc<M>,
    tts: Arc<T>,
    voice: Arc<AtomicBool>,
    history: Vec<ChatMessage>,
) -> (ActiveReply, mpsc::Receiver<ReplyEvent>)
where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (events_tx, events_rx) = mpsc::channel::<ReplyEvent>(64);
    let active = ActiveReply {
        handle: tokio::spawn(run_reply(model, tts, voice, history, events_tx).in_current_span()),
        started_at: Instant::now(),
        sent_sentences: Vec::new(),
        text: String::new(),
        speaking_sent: false,
        first_audio_at: None,
        audio_bytes: 0,
    };
    (active, events_rx)
}

async fn handle_reply_event(
    event: ReplyEvent,
    sink: &mut (impl SinkExt<Message> + Unpin),
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    playing_until: &mut Option<Instant>,
    voice_on: bool,
) {
    match event {
        ReplyEvent::Delta(text) => {
            if let Some(active) = reply {
                active.text.push_str(&text);
            }
            send_json(sink, &json!({"type": "reply", "text": text})).await;
        }
        ReplyEvent::Sentence(sentence) => {
            if let Some(active) = reply {
                active.sent_sentences.push(sentence);
            }
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
                record_assistant_history(history, &active);
                *playing_until = playback_deadline(&active);
            }
            *reply_rx = None;
            send_state(sink, if voice_on { "listening" } else { "idle" }).await;
        }
        ReplyEvent::Failed(message) => {
            tracing::warn!(%message, "reply failed");
            if let Some(active) = reply.take() {
                record_assistant_history(history, &active);
            }
            *reply_rx = None;
            *playing_until = None;
            send_json(sink, &json!({"type": "error", "message": message})).await;
            send_state(sink, if voice_on { "listening" } else { "idle" }).await;
        }
    }
}

fn playback_deadline(active: &ActiveReply) -> Option<Instant> {
    let first = active.first_audio_at?;
    let total = Duration::from_millis(
        u64::try_from(active.audio_bytes).unwrap_or(u64::MAX) * 1000 / 48_000,
    );
    let deadline = first + total;
    if deadline > Instant::now() {
        Some(deadline)
    } else {
        None
    }
}

fn record_assistant_history(history: &mut Vec<ChatMessage>, active: &ActiveReply) {
    let spoken = active.sent_sentences.join(" ");
    let text = if spoken.trim().is_empty() {
        active.text.trim().to_string()
    } else {
        spoken
    };
    if !text.is_empty() {
        history.push(ChatMessage::assistant(text));
    }
}

async fn interrupt(
    sink: &mut (impl SinkExt<Message> + Unpin),
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    playing_until: &mut Option<Instant>,
    voice_on: bool,
    reason: &'static str,
) {
    let reply_active = reply.is_some();
    let audio_playing = playing_until.is_some_and(|until| until > Instant::now());
    if !reply_active && !audio_playing {
        *playing_until = None;
        return;
    }
    if let Some(mut active) = reply.take() {
        active.handle.abort();
        if let Some(receiver) = reply_rx.as_mut() {
            while let Ok(event) = receiver.try_recv() {
                if let ReplyEvent::Sentence(sentence) = event {
                    active.sent_sentences.push(sentence);
                }
            }
        }
        record_assistant_history(history, &active);
    }
    *reply_rx = None;
    *playing_until = None;
    tracing::info!(reason, "reply interrupted");
    send_json(sink, &json!({"type": "interrupted"})).await;
    send_state(sink, if voice_on { "listening" } else { "idle" }).await;
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
    voice: Arc<AtomicBool>,
    history: Vec<ChatMessage>,
    events: mpsc::Sender<ReplyEvent>,
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
            if voice.load(Ordering::Relaxed) && !speak_sentence(&tts, &events, &sentence).await {
                return;
            }
        }
    }
    if let Some(tail) = splitter.finish()
        && voice.load(Ordering::Relaxed)
        && !speak_sentence(&tts, &events, &tail).await
    {
        return;
    }
    let _ = events.send(ReplyEvent::Finished).await;
}

/// Returns false when the reply should stop (failure reported or session gone).
async fn speak_sentence<T: Tts>(
    tts: &Arc<T>,
    events: &mpsc::Sender<ReplyEvent>,
    sentence: &str,
) -> bool {
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
    true
}

async fn note_disconnect(
    events: &mpsc::Sender<RecognizerMessage>,
    message: String,
    is_fatal: bool,
    backoff: &mut Duration,
    reconnect_at: &mut Option<Instant>,
    fatal: &mut bool,
) {
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

async fn attempt_reconnect<F>(
    factory: &F,
    recognizer: &mut Option<F::Rec>,
    backoff: &mut Duration,
    reconnect_at: &mut Option<Instant>,
    events: &mpsc::Sender<RecognizerMessage>,
    fatal: &mut bool,
) where
    F: RecognizerFactory,
{
    match factory.connect().await {
        Ok(rec) => {
            *recognizer = Some(rec);
            *backoff = INITIAL_BACKOFF;
            let _ = events.send(RecognizerMessage::Ready).await;
        }
        Err(error) => {
            if error.is_fatal() {
                *fatal = true;
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

async fn initial_connect<F>(
    factory: &F,
    events: &mpsc::Sender<RecognizerMessage>,
    backoff: &mut Duration,
    reconnect_at: &mut Option<Instant>,
    fatal: &mut bool,
) -> Option<F::Rec>
where
    F: RecognizerFactory,
{
    match factory.connect().await {
        Ok(rec) => {
            let _ = events.send(RecognizerMessage::Ready).await;
            Some(rec)
        }
        Err(error) => {
            let is_fatal = error.is_fatal();
            note_disconnect(
                events,
                error.to_string(),
                is_fatal,
                backoff,
                reconnect_at,
                fatal,
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
) where
    F: RecognizerFactory,
    F::Rec: Send,
{
    let mut backoff = INITIAL_BACKOFF;
    let mut reconnect_at: Option<Instant> = None;
    let mut fatal = false;

    let mut recognizer = initial_connect(
        &*factory,
        &events,
        &mut backoff,
        &mut reconnect_at,
        &mut fatal,
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
                    Some(Err(error)) => {
                        recognizer = None;
                        let is_fatal = error.is_fatal();
                        note_disconnect(
                            &events,
                            error.to_string(),
                            is_fatal,
                            &mut backoff,
                            &mut reconnect_at,
                            &mut fatal,
                        )
                        .await;
                    }
                    None => {
                        recognizer = None;
                        note_disconnect(
                            &events,
                            "recognizer connection closed".to_string(),
                            false,
                            &mut backoff,
                            &mut reconnect_at,
                            &mut fatal,
                        )
                        .await;
                    }
                }
            }
            () = reconnect_sleep => {
                reconnect_at = None;
                attempt_reconnect(&*factory, &mut recognizer, &mut backoff, &mut reconnect_at, &events, &mut fatal).await;
            }
        }
    }
}

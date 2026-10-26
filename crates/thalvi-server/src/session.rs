//! Owns one voice session: recognizer, reply pipeline, and the client socket.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::model::{ChatMessage, ChatModel};
use crate::recognizer::{Recognizer, RecognizerEvent, RecognizerFactory};
use crate::sentence::SentenceSplitter;
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
    Reconnected,
}

struct ActiveReply {
    handle: JoinHandle<()>,
    sent_sentences: Vec<String>,
    speaking_sent: bool,
    first_audio_at: Option<Instant>,
    audio_bytes: usize,
}

/// Runs one client session until its socket closes.
pub async fn handle_session<F, M, T>(socket: WebSocket, factory: F, model: Arc<M>, tts: Arc<T>)
where
    F: RecognizerFactory + Send + 'static,
    F::Rec: Send + 'static,
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(32);
    let (rec_tx, mut rec_rx) = mpsc::channel::<RecognizerMessage>(32);
    let recognizer_task = tokio::spawn(run_recognizer(factory, audio_rx, rec_tx));

    let mut history: Vec<ChatMessage> = Vec::new();
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
        tokio::select! {
            client_message = stream.next() => {
                match client_message {
                    Some(Ok(Message::Binary(audio))) => {
                        let _ = audio_tx.try_send(audio.to_vec());
                    }
                    Some(Ok(Message::Text(text))) => {
                        if is_cancel(&text) {
                            interrupt(&mut sink, &mut history, &mut reply, &mut reply_rx, &mut playing_until).await;
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
            recognizer_message = rec_rx.recv() => {
                match recognizer_message {
                    Some(RecognizerMessage::Event(RecognizerEvent::SpeechStart)) => {
                        interrupt(&mut sink, &mut history, &mut reply, &mut reply_rx, &mut playing_until).await;
                    }
                    Some(RecognizerMessage::Event(RecognizerEvent::Partial { text })) => {
                        send_json(&mut sink, &json!({"type": "partial", "text": text})).await;
                    }
                    Some(RecognizerMessage::Event(RecognizerEvent::SpeechComplete { text })) => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        if reply.is_some() {
                            interrupt(&mut sink, &mut history, &mut reply, &mut reply_rx, &mut playing_until).await;
                        }
                        playing_until = None;
                        history.push(ChatMessage::user(text.clone()));
                        send_json(&mut sink, &json!({"type": "user", "text": text})).await;
                        send_state(&mut sink, "thinking").await;
                        let (events_tx, events_rx) = mpsc::channel::<ReplyEvent>(64);
                        let handle = tokio::spawn(run_reply(
                            Arc::clone(&model),
                            Arc::clone(&tts),
                            history.clone(),
                            events_tx,
                        ));
                        reply = Some(ActiveReply {
                            handle,
                            sent_sentences: Vec::new(),
                            speaking_sent: false,
                            first_audio_at: None,
                            audio_bytes: 0,
                        });
                        reply_rx = Some(events_rx);
                    }
                    Some(RecognizerMessage::Event(RecognizerEvent::Error { message })) => {
                        send_json(&mut sink, &json!({"type": "error", "message": plain_recognizer_error(&message)})).await;
                    }
                    Some(RecognizerMessage::Disconnected { message, fatal }) => {
                        let message = if fatal {
                            format!("Speech recognition stopped and won't retry: {message}. Check the speech settings, then tap Start again.")
                        } else {
                            plain_recognizer_error(&message)
                        };
                        send_json(&mut sink, &json!({"type": "error", "message": message})).await;
                    }
                    Some(RecognizerMessage::Reconnected) | None => {}
                }
            }
            event = reply_event => {
                let Some(event) = event else {
                    reply_rx = None;
                    continue;
                };
                handle_reply_event(event, &mut sink, &mut history, &mut reply, &mut reply_rx, &mut playing_until).await;
            }
        }
    }

    if let Some(active) = reply.take() {
        active.handle.abort();
    }
    drop(audio_tx);
    recognizer_task.abort();
}

async fn handle_reply_event(
    event: ReplyEvent,
    sink: &mut (impl SinkExt<Message> + Unpin),
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    playing_until: &mut Option<Instant>,
) {
    match event {
        ReplyEvent::Delta(text) => {
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
                push_assistant_history(history, &active.sent_sentences);
                *playing_until = playback_deadline(&active);
            }
            *reply_rx = None;
            send_state(sink, "listening").await;
        }
        ReplyEvent::Failed(message) => {
            if let Some(active) = reply.take() {
                push_assistant_history(history, &active.sent_sentences);
            }
            *reply_rx = None;
            *playing_until = None;
            send_json(sink, &json!({"type": "error", "message": message})).await;
            send_state(sink, "listening").await;
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

fn push_assistant_history(history: &mut Vec<ChatMessage>, sentences: &[String]) {
    let text = sentences.join(" ");
    if !text.trim().is_empty() {
        history.push(ChatMessage::assistant(text));
    }
}

async fn interrupt(
    sink: &mut (impl SinkExt<Message> + Unpin),
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    playing_until: &mut Option<Instant>,
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
        push_assistant_history(history, &active.sent_sentences);
    }
    *reply_rx = None;
    *playing_until = None;
    send_json(sink, &json!({"type": "interrupted"})).await;
    send_state(sink, "listening").await;
}

fn is_cancel(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|kind| kind.as_str())
                .map(str::to_string)
        })
        .is_some_and(|kind| kind == "cancel")
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
            if !speak_sentence(&tts, &events, &sentence).await {
                return;
            }
        }
    }
    if let Some(tail) = splitter.finish()
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
            let _ = events.send(RecognizerMessage::Reconnected).await;
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
        Ok(rec) => Some(rec),
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
    factory: F,
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
        &factory,
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
                attempt_reconnect(&factory, &mut recognizer, &mut backoff, &mut reconnect_at, &events, &mut fatal).await;
            }
        }
    }
}

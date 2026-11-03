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

use crate::history::{self, HistoryMatch};
use crate::model::{ChatEvent, ChatMessage, ChatModel, Role, ToolCall, ToolDefinition, ToolResult};
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

const MISHEARD_LINE: &str = "Sorry, I didn't catch that.";
/// Most rounds of tool calls one reply may run before the model must answer in text.
const MAX_TOOL_ROUNDS: usize = 3;
const SEARCH_LABEL: &str = "Searching conversations";
const UNKNOWN_TOOL_LABEL: &str = "Running a tool";

type AckCache = Arc<std::sync::Mutex<std::collections::HashMap<(String, &'static str), Bytes>>>;

#[derive(Clone)]
struct AckSettings {
    enabled: bool,
    cache: AckCache,
}

/// Everything one reply needs besides the model and the speech engine.
#[derive(Clone)]
struct ReplyContext {
    ack: AckSettings,
    /// The voice every speech call of this reply uses, fixed when the reply starts.
    voice_name: String,
    /// Saved conversations the history tool may search; none for a private conversation.
    past: Option<Arc<SessionStore>>,
    session_id: String,
}

/// The voices a connection may pick from and the one it starts with.
#[derive(Clone)]
pub struct VoiceChoices {
    /// Voice used until the client picks another.
    pub default: String,
    /// Every voice the client may pick.
    pub offered: Arc<[String]>,
}

/// A connection's reply settings: the context new replies start with and the voices on offer.
struct ReplySettings {
    context: ReplyContext,
    offered: Arc<[String]>,
}

impl ReplySettings {
    fn new(
        ack_enabled: bool,
        voices: VoiceChoices,
        past: Option<Arc<SessionStore>>,
        session_id: String,
    ) -> Self {
        Self {
            context: ReplyContext {
                ack: AckSettings {
                    enabled: ack_enabled,
                    cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                },
                voice_name: voices.default,
                past,
                session_id,
            },
            offered: voices.offered,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
    Running,
    Done,
    Failed,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// One visible step of the work behind a reply.
struct TaskStep {
    id: String,
    label: &'static str,
    status: TaskStatus,
    detail: Option<String>,
}

enum ReplyEvent {
    /// The model's echo disagreed with the caption; the reply is the fallback line instead.
    Misheard,
    Delta(String),
    Sentence(String),
    Audio(Bytes),
    AckAudio(Bytes),
    FirstSentence(Instant),
    ReplyLoopFinished(Instant),
    Finished,
    Failed(String),
    Task(TaskStep),
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
    reply_loop_finished_at: Option<Instant>,
    first_sentence_at: Option<Instant>,
    first_audio_at: Option<Instant>,
    ack_first_audio_at: Option<Instant>,
    audio_bytes: usize,
    /// Length of the user's audio sent to the model, when it heard the turn.
    input_audio_ms: Option<u64>,
    /// The audio turn failed its echo check, so it stays out of history.
    misheard: bool,
    voice_name: String,
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

/// Where the user's turns come from; only `Mic` has a recognizer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputSource {
    Text,
    Mic,
}

impl InputSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Mic => "mic",
        }
    }
}

/// The connection's two independent choices: how the user is heard, and whether replies are spoken.
struct Choices {
    input: InputSource,
    reply_audio: watch::Sender<bool>,
}

impl Choices {
    fn new() -> Self {
        Self {
            input: InputSource::Text,
            reply_audio: watch::Sender::new(false),
        }
    }

    fn snapshot(&self, recognizer: Option<&RecognizerHandle>) -> Mode {
        Mode {
            input: self.input,
            ready: recognizer.is_some_and(|handle| handle.ready),
            reply_audio: *self.reply_audio.borrow(),
        }
    }
}

/// `Choices` at one moment, with whether the recognizer can hear the user.
#[derive(Clone, Copy)]
struct Mode {
    input: InputSource,
    ready: bool,
    reply_audio: bool,
}

impl Mode {
    /// The state to report when no reply is running.
    fn resting_state(self) -> &'static str {
        match (self.input, self.ready) {
            (InputSource::Text, _) => "idle",
            (InputSource::Mic, true) => "listening",
            (InputSource::Mic, false) => "starting",
        }
    }
}

/// What a connection may keep, chosen when it connects and never changed.
pub enum Retention<M> {
    /// A saved conversation: `resume` when the store has it, else a new one.
    Saved {
        /// Where conversations are kept.
        store: Arc<SessionStore>,
        /// The conversation to rejoin, if any.
        resume: Option<String>,
        /// Writes a title after the first reply, when configured.
        titler: Option<Arc<Titler<M>>>,
    },
    /// Context held in memory only, under an id no store knows.
    Private {
        /// The id this connection reports; it is never written anywhere but metrics.
        id: String,
    },
}

/// Every private conversation's id starts with this, so a resume of one is recognised and refused.
pub const PRIVATE_ID_PREFIX: &str = "private-";

fn is_private_id(id: &str) -> bool {
    id.starts_with(PRIVATE_ID_PREFIX)
}

const PRIVATE_RESUME_REFUSAL: &str =
    "A private conversation ends when it closes and can't be resumed. Start a new conversation.";

/// Tells a client that asked to resume a private conversation why not, then closes its socket,
/// so it never lands in a saved conversation by accident.
pub async fn refuse_private_resume(socket: WebSocket) {
    let (mut sink, _) = socket.split();
    send_json(
        &mut sink,
        &json!({"type": "ended", "reason": "private", "message": PRIVATE_RESUME_REFUSAL}),
    )
    .await;
    let _ = sink.send(Message::Close(None)).await;
}

impl<M> Retention<M> {
    /// Reads a connection's request. `None` when it asks to resume a private conversation,
    /// which no one can, so the caller refuses it rather than start a saved one instead.
    pub fn from_request(
        private: bool,
        resume: Option<String>,
        store: Arc<SessionStore>,
        titler: Option<Arc<Titler<M>>>,
        new_private_id: impl FnOnce() -> String,
    ) -> Option<Self> {
        match (private, resume) {
            (true, None) => Some(Self::Private {
                id: new_private_id(),
            }),
            (false, resume) if !resume.as_deref().is_some_and(is_private_id) => Some(Self::Saved {
                store,
                resume,
                titler,
            }),
            _ => None,
        }
    }
}

/// A conversation as it opens: what it already holds and how it will be kept.
struct Opened<M> {
    persistence: Persistence<M>,
    id: String,
    history: Vec<ChatMessage>,
    next_turn: u64,
    past: Option<Arc<SessionStore>>,
}

impl<M: ChatModel + 'static> Retention<M> {
    async fn open(self) -> Opened<M> {
        match self {
            Self::Saved {
                store,
                resume,
                titler,
            } => {
                let mut attachment = store.attach(resume.as_deref()).await;
                let history = std::mem::take(&mut attachment.history);
                let (id, next_turn) = (attachment.id.clone(), attachment.next_turn);
                let past = Some(Arc::clone(&store));
                let saved = Saved {
                    store,
                    attachment,
                    titler,
                    synced_len: history.len(),
                    title_tried: false,
                };
                Opened {
                    persistence: Persistence::Saved(saved),
                    id,
                    history,
                    next_turn,
                    past,
                }
            }
            Self::Private { id } => Opened {
                persistence: Persistence::Private,
                id,
                history: Vec::new(),
                next_turn: 0,
                past: None,
            },
        }
    }
}

/// Keeps the store in step with the history; a private conversation holds no store at all.
enum Persistence<M> {
    Saved(Saved<M>),
    Private,
}

/// A saved conversation's claim on the store, with its title bookkeeping.
struct Saved<M> {
    store: Arc<SessionStore>,
    attachment: Attachment,
    titler: Option<Arc<Titler<M>>>,
    synced_len: usize,
    title_tried: bool,
}

impl<M: ChatModel + 'static> Persistence<M> {
    fn is_private(&self) -> bool {
        matches!(self, Self::Private)
    }

    /// Resolves when a saved conversation must be let go; never for a private one.
    async fn detached(&mut self) -> Detach {
        match self {
            Self::Saved(saved) => saved.attachment.detached().await,
            Self::Private => std::future::pending().await,
        }
    }

    /// A private conversation never saves, so its receiver never signals.
    fn save_error_receiver(&self) -> watch::Receiver<bool> {
        match self {
            Self::Saved(saved) => saved.attachment.save_error_receiver(),
            Self::Private => watch::channel(false).1,
        }
    }

    fn finish(self, history: &[ChatMessage], next_turn: u64, recorder: &Recorder) {
        if let Self::Saved(mut saved) = self {
            saved.sync(history, next_turn, recorder);
            saved.store.release(saved.attachment);
        }
    }

    fn sync(&mut self, history: &[ChatMessage], next_turn: u64, recorder: &Recorder) {
        if let Self::Saved(saved) = self {
            saved.sync(history, next_turn, recorder);
        }
    }
}

impl<M: ChatModel + 'static> Saved<M> {
    fn sync(&mut self, history: &[ChatMessage], next_turn: u64, recorder: &Recorder) {
        if history.len() == self.synced_len {
            return;
        }
        self.synced_len = history.len();
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

async fn send_save_notice(sink: &mut (impl SinkExt<Message> + Unpin), sent: &mut bool) {
    if *sent {
        return;
    }
    *sent = true;
    send_json(
        sink,
        &json!({"type": "error", "message": "This conversation couldn't be saved"}),
    )
    .await;
}

async fn wait_save_failed(save_error: &mut watch::Receiver<bool>) {
    let signalled = save_error.wait_for(|failed| *failed).await.is_ok();
    if !signalled {
        std::future::pending::<()>().await;
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
    retention: Retention<M>,
    recording_config: RecordingConfig,
    ack_enabled: bool,
    audio_input: bool,
    voices: VoiceChoices,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let factory = Arc::new(factory);
    let session_started = Instant::now();
    let Opened {
        mut persistence,
        id: session_id,
        mut history,
        next_turn,
        past,
    } = retention.open().await;
    let private = persistence.is_private();
    let resumed = !history.is_empty();
    let recording_config = if private {
        recording_config.metrics_only()
    } else {
        recording_config
    };
    let recording = start_recording(recording_config, session_id.clone());
    let mut turn_log = TurnLog::new(recording.recorder.clone(), next_turn, audio_input);
    turn_log
        .recorder
        .emit("session_open", json!({"resumed": resumed}));
    send_session_opening(&mut sink, &session_id, &history, private).await;
    send_state(&mut sink, "idle").await;
    tracing::info!("session opened");

    let mut settings = ReplySettings::new(ack_enabled, voices, past, session_id.clone());
    let mut choices = Choices::new();
    let mut recognizer: Option<RecognizerHandle> = None;
    let mut rec_rx: Option<mpsc::Receiver<RecognizerMessage>> = None;
    let mut reply: Option<ActiveReply> = None;
    let mut reply_rx: Option<mpsc::Receiver<ReplyEvent>> = None;
    let mut client_audio = ClientAudio::default();
    let mut detached = None;
    let (mut save_error, mut save_notice_sent) = (persistence.save_error_receiver(), false);

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
            reason = persistence.detached() => {
                detached = Some(reason);
                break;
            }
            () = wait_save_failed(&mut save_error), if !save_notice_sent => send_save_notice(&mut sink, &mut save_notice_sent).await,
            client_message = stream.next() => {
                match client_message {
                    Some(Ok(Message::Binary(audio))) => {
                        forward_mic(&audio, recognizer.as_ref(), &mut turn_log);
                    }
                    Some(Ok(Message::Text(text))) => {
                        handle_client_text(&text, &mut sink, &mut recognizer, &mut rec_rx, &factory, &mut choices, &mut history, &mut reply, &mut reply_rx, &mut client_audio, &model, &tts, &mut turn_log, &mut settings).await;
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
                handle_recognizer_message(recognizer_message, &mut sink, &mut recognizer, &mut rec_rx, &mut history, &mut reply, &mut reply_rx, &mut client_audio, &model, &tts, &mut choices, &mut turn_log, &settings).await;
            }
            event = reply_event => {
                let Some(event) = event else {
                    reply_rx = None;
                    continue;
                };
                handle_reply_event(event, &mut sink, &mut history, &mut reply, &mut reply_rx, &mut client_audio, choices.snapshot(recognizer.as_ref()), &mut turn_log).await;
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
    choices: &mut Choices,
    history: &mut Vec<ChatMessage>,
    reply: &mut Option<ActiveReply>,
    reply_rx: &mut Option<mpsc::Receiver<ReplyEvent>>,
    client_audio: &mut ClientAudio,
    model: &Arc<M>,
    tts: &Arc<T>,
    turn_log: &mut TurnLog,
    settings: &mut ReplySettings,
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
                choices.snapshot(recognizer.as_ref()),
                "cancel",
                turn_log,
            )
            .await;
        }
        ClientText::Played(bytes) => client_audio.played(bytes),
        ClientText::Input(source) => {
            set_input(
                source,
                sink,
                recognizer,
                rec_rx,
                factory,
                choices,
                reply.is_some(),
                turn_log,
            )
            .await;
        }
        ClientText::ReplyAudio(on) => {
            set_reply_audio(on, sink, choices, reply, client_audio).await;
        }
        ClientText::VoiceName(name) => {
            if settings.offered.contains(&name) {
                settings.context.voice_name = name;
            } else {
                send_json(
                    sink,
                    &json!({"type": "error", "message": "That voice isn't offered, so the voice was not changed."}),
                )
                .await;
            }
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
                    &choices.reply_audio,
                    choices.snapshot(recognizer.as_ref()),
                    "text",
                    text.clone(),
                    &[],
                    turn_log,
                    settings,
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
    choices: &mut Choices,
    turn_log: &mut TurnLog,
    settings: &ReplySettings,
) where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
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
                choices.snapshot(recognizer.as_ref()),
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
                    &choices.reply_audio,
                    choices.snapshot(recognizer.as_ref()),
                    "voice",
                    text.clone(),
                    &clip,
                    turn_log,
                    settings,
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
            let replying = reply.is_some();
            recognizer_disconnected(
                &message, fatal, sink, recognizer, rec_rx, choices, replying, turn_log,
            )
            .await;
        }
        Some(RecognizerMessage::Ready) => {
            turn_log.reset_mic();
            if let Some(handle) = recognizer {
                handle.ready = true;
            }
            send_input(sink, InputSource::Mic, true).await;
            if reply.is_none() {
                send_state(sink, "listening").await;
            }
        }
        None => {}
    }
}

/// Reports a lost recognizer; a fatal loss falls back to text input.
#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn recognizer_disconnected(
    message: &str,
    fatal: bool,
    sink: &mut (impl SinkExt<Message> + Unpin),
    recognizer: &mut Option<RecognizerHandle>,
    rec_rx: &mut Option<mpsc::Receiver<RecognizerMessage>>,
    choices: &mut Choices,
    replying: bool,
    turn_log: &mut TurnLog,
) {
    if let Some(handle) = recognizer {
        handle.ready = false;
    }
    tracing::warn!(%message, fatal, "recognizer disconnected");
    let message = if fatal {
        format!(
            "Speech recognition stopped and won't retry: {message}. Check the speech settings, then tap Start again."
        )
    } else {
        plain_recognizer_error(message)
    };
    send_json(sink, &json!({"type": "error", "message": message})).await;
    if fatal {
        send_input(sink, InputSource::Text, true).await;
        stop_input(sink, choices, recognizer, rec_rx, replying, turn_log).await;
    } else {
        send_input(sink, InputSource::Mic, false).await;
        if !replying {
            send_state(sink, "starting").await;
        }
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
        recorder.emit_wav(turn.to_string(), clip);
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
    private: bool,
) {
    let turns: Vec<serde_json::Value> = history
        .iter()
        .map(|message| json!({"role": message.role, "text": message.content}))
        .collect();
    let mut opening = json!({"type": "session", "id": session_id, "turns": turns});
    if private {
        opening["private"] = json!(true);
    }
    send_json(sink, &opening).await;
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
async fn set_input<F>(
    source: InputSource,
    sink: &mut (impl SinkExt<Message> + Unpin),
    recognizer: &mut Option<RecognizerHandle>,
    rec_rx: &mut Option<mpsc::Receiver<RecognizerMessage>>,
    factory: &Arc<F>,
    choices: &mut Choices,
    replying: bool,
    turn_log: &mut TurnLog,
) where
    F: RecognizerFactory + Send + Sync + 'static,
    F::Rec: Send + 'static,
{
    match source {
        InputSource::Text => {
            stop_input(sink, choices, recognizer, rec_rx, replying, turn_log).await;
        }
        InputSource::Mic => {
            choices.input = InputSource::Mic;
            if recognizer.is_none() {
                let (handle, receiver) = spawn_recognizer(factory, turn_log.recorder.clone());
                *recognizer = Some(handle);
                *rec_rx = Some(receiver);
                send_input(sink, InputSource::Mic, false).await;
                if !replying {
                    send_state(sink, "starting").await;
                }
            }
        }
    }
}

/// Switches to text input: releases the recognizer and leaves the running reply and its audio alone.
async fn stop_input(
    sink: &mut (impl SinkExt<Message> + Unpin),
    choices: &mut Choices,
    recognizer: &mut Option<RecognizerHandle>,
    rec_rx: &mut Option<mpsc::Receiver<RecognizerMessage>>,
    replying: bool,
    turn_log: &mut TurnLog,
) {
    choices.input = InputSource::Text;
    let Some(handle) = recognizer.take() else {
        return;
    };
    handle.task.abort();
    *rec_rx = None;
    turn_log.pending.clear();
    turn_log.reset_mic();
    if !replying {
        send_state(sink, "idle").await;
    }
}

/// Turns spoken replies on or off; off drops queued audio but the reply's text carries on.
async fn set_reply_audio(
    on: bool,
    sink: &mut (impl SinkExt<Message> + Unpin),
    choices: &Choices,
    reply: &mut Option<ActiveReply>,
    client_audio: &mut ClientAudio,
) {
    choices.reply_audio.send_replace(on);
    if !on {
        client_audio.clear();
        if let Some(active) = reply {
            active.spoken_throughout = false;
        }
    }
    send_json(sink, &json!({"type": "reply_audio", "on": on})).await;
}

async fn send_input(sink: &mut (impl SinkExt<Message> + Unpin), source: InputSource, ready: bool) {
    send_json(
        sink,
        &json!({"type": "input", "source": source.as_str(), "ready": ready}),
    )
    .await;
}

enum ClientText {
    Cancel,
    Played(u64),
    Input(InputSource),
    ReplyAudio(bool),
    VoiceName(String),
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
        Some("input") => match value.get("source").and_then(serde_json::Value::as_str) {
            Some("text") => ClientText::Input(InputSource::Text),
            Some("mic") => ClientText::Input(InputSource::Mic),
            _ => ClientText::Other,
        },
        Some("reply_audio") => value
            .get("on")
            .and_then(serde_json::Value::as_bool)
            .map_or(ClientText::Other, ClientText::ReplyAudio),
        Some("voice_name") => value
            .get("voice")
            .and_then(serde_json::Value::as_str)
            .map_or(ClientText::Other, |name| {
                ClientText::VoiceName(name.to_string())
            }),
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
    reply_audio: &watch::Sender<bool>,
    state: Mode,
    source: &'static str,
    text: String,
    clip: &[u8],
    turn_log: &mut TurnLog,
    settings: &ReplySettings,
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
    let caption = audio.is_some().then(|| text.clone());
    history.push(ChatMessage::user(text.clone()));
    send_json(sink, &json!({"type": "user", "text": text})).await;
    send_state(sink, "thinking").await;
    let (active, events_rx) = start_reply(
        Arc::clone(model),
        Arc::clone(tts),
        reply_audio.subscribe(),
        history.clone(),
        audio.zip(caption),
        turn,
        turn_log.recorder.clone(),
        settings.context.clone(),
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
    reply_audio: watch::Receiver<bool>,
    history: Vec<ChatMessage>,
    audio: Option<(Vec<u8>, String)>,
    turn: u64,
    recorder: Recorder,
    context: ReplyContext,
) -> (ActiveReply, mpsc::Receiver<ReplyEvent>)
where
    M: ChatModel + 'static,
    T: Tts + 'static,
{
    let (events_tx, events_rx) = mpsc::channel::<ReplyEvent>(64);
    let spoken_throughout = *reply_audio.borrow();
    let input_audio_ms = audio
        .as_ref()
        .map(|(pcm, _)| u64::try_from(pcm.len()).unwrap_or(u64::MAX) / 48);
    let voice_name = context.voice_name.clone();
    let active = ActiveReply {
        handle: tokio::spawn(
            run_reply(
                model,
                tts,
                reply_audio,
                history,
                audio,
                events_tx,
                turn,
                recorder,
                context,
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
        reply_loop_finished_at: None,
        first_sentence_at: None,
        first_audio_at: None,
        ack_first_audio_at: None,
        audio_bytes: 0,
        input_audio_ms,
        misheard: false,
        voice_name,
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
    state: Mode,
    turn_log: &mut TurnLog,
) {
    match event {
        ReplyEvent::Misheard => {
            if let Some(active) = reply {
                active.misheard = true;
            }
        }
        ReplyEvent::Delta(text) => {
            if let Some(active) = reply {
                if active.first_delta_at.is_none() {
                    active.first_delta_at = Some(Instant::now());
                }
                active.text.push_str(&text);
            }
            send_json(sink, &json!({"type": "reply", "text": text})).await;
        }
        ReplyEvent::ReplyLoopFinished(at) => {
            if let Some(active) = reply {
                active.reply_loop_finished_at = Some(at);
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
        ReplyEvent::AckAudio(_) | ReplyEvent::Audio(_) if !state.reply_audio => {}
        ReplyEvent::AckAudio(audio) => {
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
        ReplyEvent::Task(step) => {
            send_task_step(sink, &step).await;
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

/// Shows a task step, then the state the reply is in around it.
async fn send_task_step(sink: &mut (impl SinkExt<Message> + Unpin), step: &TaskStep) {
    let mut fields = json!({"id": step.id, "label": step.label, "status": step.status.as_str()});
    if let Some(detail) = &step.detail {
        fields["detail"] = json!(detail);
    }
    send_json(sink, &json!({"type": "task", "step": fields})).await;
    let state = if step.status == TaskStatus::Running {
        "working"
    } else {
        "thinking"
    };
    send_state(sink, state).await;
}

fn reply_loop_ms(
    started_at: Instant,
    reply_loop_finished_at: Option<Instant>,
    recorded_at: Instant,
) -> u64 {
    let end = reply_loop_finished_at.unwrap_or(recorded_at);
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
        "reply_loop_ms": reply_loop_ms(active.started_at, active.reply_loop_finished_at, Instant::now()),
        "tts_model": recorder.tts_model(),
        "voice": active.voice_name,
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
    if active.misheard {
        history.pop();
        return;
    }
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
    state: Mode,
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
                    ReplyEvent::Misheard => active.misheard = true,
                    ReplyEvent::Sentence(sentence) => active.sent_sentences.push(sentence),
                    ReplyEvent::Delta(text) => active.text.push_str(&text),
                    ReplyEvent::FirstSentence(at) => {
                        if active.first_sentence_at.is_none() {
                            active.first_sentence_at = Some(at);
                        }
                    }
                    ReplyEvent::ReplyLoopFinished(at) => active.reply_loop_finished_at = Some(at),
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
    reply_audio: watch::Receiver<bool>,
    history: Vec<ChatMessage>,
    audio: Option<(Vec<u8>, String)>,
    events: mpsc::Sender<ReplyEvent>,
    turn: u64,
    recorder: Recorder,
    context: ReplyContext,
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
            ToolRun {
                past: context.past.clone(),
                session_id: context.session_id.clone(),
                recorder: recorder.clone(),
                turn,
            },
        )
        .in_current_span(),
    );
    let mut consumer = tokio::spawn(
        consume_sentences(
            sentence_rx,
            tts,
            reply_audio,
            events.clone(),
            turn,
            recorder,
            failure_sent,
            context.ack,
            context.voice_name,
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

/// What running a tool call needs besides the call.
struct ToolRun {
    past: Option<Arc<SessionStore>>,
    session_id: String,
    recorder: Recorder,
    turn: u64,
}

struct ModelCall<'a> {
    run: &'a ToolRun,
    round: usize,
    started: Instant,
    recorded: bool,
}

impl<'a> ModelCall<'a> {
    fn start(run: &'a ToolRun, round: usize) -> Self {
        Self {
            run,
            round,
            started: Instant::now(),
            recorded: false,
        }
    }

    fn finish(&mut self, outcome: &str, error: Option<&str>) {
        if self.recorded {
            return;
        }
        self.run.recorder.emit(
            "model_call",
            json!({
                "turn": self.run.turn,
                "round": self.round,
                "model": self.run.recorder.llm_model(),
                "reasoning_effort": self.run.recorder.reasoning_effort(),
                "total_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "outcome": outcome,
                "error": error,
            }),
        );
        self.recorded = true;
    }

    async fn fail(
        &mut self,
        error: &crate::model::ModelError,
        events: &mpsc::Sender<ReplyEvent>,
        failure_sent: &AtomicBool,
    ) -> bool {
        self.finish("failed", Some(&error.to_string()));
        send_failed(
            events,
            failure_sent,
            format!("The language model could not reply: {error}"),
        )
        .await;
        false
    }
}

impl Drop for ModelCall<'_> {
    fn drop(&mut self) {
        self.finish("interrupted", None);
    }
}

async fn produce_sentences<M>(
    model: Arc<M>,
    history: Vec<ChatMessage>,
    audio: Option<(Vec<u8>, String)>,
    events: mpsc::Sender<ReplyEvent>,
    sentences: mpsc::UnboundedSender<String>,
    failure_sent: Arc<AtomicBool>,
    tools: ToolRun,
) -> bool
where
    M: ChatModel,
{
    let (clip, caption) = audio.unzip();
    let definitions = [history::tool_definition()];
    let mut results: Vec<ToolResult> = Vec::new();
    let mut splitter = SentenceSplitter::new();
    let mut first_sentence_sent = false;
    let mut ends_in_whitespace = true;
    for round in 0..=MAX_TOOL_ROUNDS {
        let mut round_start = true;
        let mut heard = String::new();
        let mut echo_checked = caption.is_none();
        let offered: &[ToolDefinition] = if round < MAX_TOOL_ROUNDS {
            &definitions
        } else {
            &[]
        };
        let mut model_call = ModelCall::start(&tools, round);
        let mut stream = match model
            .stream_reply_with_tools(&history, clip.as_deref(), offered, &results)
            .await
        {
            Ok(stream) => stream,
            Err(error) => return model_call.fail(&error, &events, &failure_sent).await,
        };
        let mut calls: Vec<ToolCall> = Vec::new();
        while let Some(event) = stream.next().await {
            let mut delta = match event {
                Ok(ChatEvent::Heard(fragment)) => {
                    heard.push_str(&fragment);
                    continue;
                }
                Ok(ChatEvent::Text(delta)) => delta,
                Ok(ChatEvent::ToolCall(call)) => {
                    calls.push(call);
                    continue;
                }
                Err(error) => return model_call.fail(&error, &events, &failure_sent).await,
            };
            if delta.is_empty() {
                continue;
            }
            if let Some(caption) = caption.as_deref().filter(|_| !echo_checked) {
                echo_checked = true;
                if echo_disagrees(&heard, caption) {
                    model_call.finish("misheard", None);
                    return reply_misheard(&events, &sentences).await;
                }
            }
            if round_start && !ends_in_whitespace && !delta.starts_with(char::is_whitespace) {
                delta.insert(0, ' ');
            }
            round_start = false;
            ends_in_whitespace = delta.ends_with(char::is_whitespace);
            if events.send(ReplyEvent::Delta(delta.clone())).await.is_err() {
                return false;
            }
            for sentence in splitter.push(&delta) {
                if !send_sentence(&events, &sentences, sentence, &mut first_sentence_sent).await {
                    return false;
                }
            }
        }
        if let Some(caption) = caption.as_deref().filter(|_| !echo_checked)
            && echo_disagrees(&heard, caption)
        {
            model_call.finish("misheard", None);
            return reply_misheard(&events, &sentences).await;
        }
        model_call.finish("ok", None);
        if let Some(tail) = splitter.finish()
            && !send_sentence(&events, &sentences, tail, &mut first_sentence_sent).await
        {
            return false;
        }
        if calls.is_empty() {
            break;
        }
        if round == MAX_TOOL_ROUNDS {
            send_failed(
                &events,
                &failure_sent,
                "The language model kept searching instead of answering.".to_string(),
            )
            .await;
            return false;
        }
        for call in calls {
            let Some(result) = run_tool_call(call, &history, &tools, &events).await else {
                return false;
            };
            results.push(result);
        }
    }
    events
        .send(ReplyEvent::ReplyLoopFinished(Instant::now()))
        .await
        .is_ok()
}

/// Hands one finished sentence to speech, marking the first one. False when the reply is gone.
async fn send_sentence(
    events: &mpsc::Sender<ReplyEvent>,
    sentences: &mpsc::UnboundedSender<String>,
    sentence: String,
    first_sentence_sent: &mut bool,
) -> bool {
    if !*first_sentence_sent {
        *first_sentence_sent = true;
        if events
            .send(ReplyEvent::FirstSentence(Instant::now()))
            .await
            .is_err()
        {
            return false;
        }
    }
    sentences.send(sentence).is_ok()
}

/// Runs one model tool call against the live history and any saved conversations, showing it
/// as a task step. A call that cannot run becomes a failed step and an error result for the
/// model. None when the reply is gone.
async fn run_tool_call(
    call: ToolCall,
    history: &[ChatMessage],
    run: &ToolRun,
    events: &mpsc::Sender<ReplyEvent>,
) -> Option<ToolResult> {
    let known = call.name == history::tool_definition().name;
    let label = if known {
        SEARCH_LABEL
    } else {
        UNKNOWN_TOOL_LABEL
    };
    let step = |status, detail| {
        ReplyEvent::Task(TaskStep {
            id: call.id.clone(),
            label,
            status,
            detail,
        })
    };
    events.send(step(TaskStatus::Running, None)).await.ok()?;
    let started = Instant::now();
    let outcome: Result<Vec<HistoryMatch>, String> = if known {
        history::parse_arguments(&call.arguments).map(|query| match &run.past {
            Some(store) => store.search_history(&run.session_id, history, &query),
            None => history::search_live(&run.session_id, history, &query.text),
        })
    } else {
        Err("That tool doesn't exist.".to_string())
    };
    let total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let tool = if known { call.name.as_str() } else { "unknown" };
    let (content, event, fields) = match &outcome {
        Ok(matches) => {
            let detail = match matches.len() {
                1 => "1 match".to_string(),
                count => format!("{count} matches"),
            };
            (
                history::result_content(matches),
                step(TaskStatus::Done, Some(detail)),
                json!({"turn": run.turn, "tool": tool, "total_ms": total_ms, "outcome": "ok", "matches": matches.len()}),
            )
        }
        Err(message) => (
            json!({"error": message}).to_string(),
            step(TaskStatus::Failed, Some(message.clone())),
            json!({"turn": run.turn, "tool": tool, "total_ms": total_ms, "outcome": "failed", "error": message}),
        ),
    };
    run.recorder.emit("tool", fields);
    events.send(event).await.ok()?;
    Some(ToolResult { call, content })
}

/// True when the guard should discard the answer.
/// An endpoint that sends no echo is trusted.
fn echo_disagrees(heard: &str, caption: &str) -> bool {
    let echo = (!heard.trim().is_empty()).then_some(heard.trim());
    echo.is_some_and(|echo| crate::heard::disagrees(echo, caption))
}

async fn reply_misheard(
    events: &mpsc::Sender<ReplyEvent>,
    sentences: &mpsc::UnboundedSender<String>,
) -> bool {
    let now = Instant::now();
    for event in [
        ReplyEvent::Misheard,
        ReplyEvent::Delta(MISHEARD_LINE.to_string()),
        ReplyEvent::FirstSentence(now),
        ReplyEvent::ReplyLoopFinished(now),
    ] {
        if events.send(event).await.is_err() {
            return false;
        }
    }
    sentences.send(MISHEARD_LINE.to_string()).is_ok()
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn consume_sentences<T>(
    mut sentences: mpsc::UnboundedReceiver<String>,
    tts: Arc<T>,
    mut reply_audio: watch::Receiver<bool>,
    events: mpsc::Sender<ReplyEvent>,
    turn: u64,
    recorder: Recorder,
    failure_sent: Arc<AtomicBool>,
    ack: AckSettings,
    voice_name: String,
) -> bool
where
    T: Tts,
{
    let mut pending_first: Option<String> = None;
    if ack.enabled && *reply_audio.borrow() {
        match tokio::time::timeout(ACK_DELAY, sentences.recv()).await {
            Ok(Some(sentence)) => pending_first = Some(sentence),
            Ok(None) => return true,
            Err(_) => {
                let line = ACK_LINES[usize::try_from(turn.saturating_sub(1) % 3).unwrap_or(0)];
                if !speak_ack(
                    &tts,
                    &mut reply_audio,
                    &events,
                    line,
                    turn,
                    &recorder,
                    &ack.cache,
                    &voice_name,
                )
                .await
                {
                    return false;
                }
            }
        }
    }
    if let Some(sentence) = pending_first
        && *reply_audio.borrow()
        && !speak_sentence(
            &tts,
            &mut reply_audio,
            &events,
            &sentence,
            turn,
            &recorder,
            &failure_sent,
            &voice_name,
        )
        .await
    {
        return false;
    }
    while let Some(sentence) = sentences.recv().await {
        if *reply_audio.borrow()
            && !speak_sentence(
                &tts,
                &mut reply_audio,
                &events,
                &sentence,
                turn,
                &recorder,
                &failure_sent,
                &voice_name,
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
) -> std::sync::MutexGuard<'_, std::collections::HashMap<(String, &'static str), Bytes>> {
    ack_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn speak_ack<T: Tts>(
    tts: &Arc<T>,
    reply_audio: &mut watch::Receiver<bool>,
    events: &mpsc::Sender<ReplyEvent>,
    line: &'static str,
    turn: u64,
    recorder: &Recorder,
    ack_cache: &AckCache,
    voice_name: &str,
) -> bool {
    tokio::select! {
        biased;
        () = reply_audio_muted(reply_audio) => true,
        keep_going = synthesize_ack(tts, events, line, turn, recorder, ack_cache, voice_name) => keep_going,
    }
}

async fn synthesize_ack<T: Tts>(
    tts: &Arc<T>,
    events: &mpsc::Sender<ReplyEvent>,
    line: &'static str,
    turn: u64,
    recorder: &Recorder,
    ack_cache: &AckCache,
    voice_name: &str,
) -> bool {
    let started = Instant::now();
    let total_ms =
        |started: Instant| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let key = (voice_name.to_string(), line);
    let cached = lock_ack_cache(ack_cache).get(&key).cloned();
    if let Some(pcm) = cached {
        if events.send(ReplyEvent::AckAudio(pcm)).await.is_err() {
            return false;
        }
        recorder.emit(
            "ack",
            json!({"turn": turn, "cached": true, "total_ms": total_ms(started), "voice": voice_name}),
        );
        return true;
    }
    let mut audio = match tts.synthesize_with_voice(line, voice_name).await {
        Ok(audio) => audio,
        Err(error) => {
            tracing::warn!(%error, "acknowledgement synthesis failed");
            recorder.emit(
                "ack",
                json!({"turn": turn, "cached": false, "total_ms": total_ms(started), "outcome": "failed", "voice": voice_name}),
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
                    json!({"turn": turn, "cached": false, "total_ms": total_ms(started), "outcome": "failed", "voice": voice_name}),
                );
                return true;
            }
        }
    }
    if pcm.is_empty() {
        recorder.emit(
            "ack",
            json!({"turn": turn, "cached": false, "total_ms": total_ms(started), "outcome": "failed", "voice": voice_name}),
        );
        return true;
    }
    lock_ack_cache(ack_cache).insert(key, Bytes::from(pcm));
    recorder.emit(
        "ack",
        json!({"turn": turn, "cached": false, "total_ms": total_ms(started), "voice": voice_name}),
    );
    true
}

/// Speaks one sentence, giving it up as soon as reply audio is muted.
/// Returns false when the reply should stop (failure reported or session gone).
#[expect(
    clippy::too_many_arguments,
    reason = "session state is passed explicitly rather than bundled into a context type"
)]
async fn speak_sentence<T: Tts>(
    tts: &Arc<T>,
    reply_audio: &mut watch::Receiver<bool>,
    events: &mpsc::Sender<ReplyEvent>,
    sentence: &str,
    turn: u64,
    recorder: &Recorder,
    failure_sent: &AtomicBool,
    voice_name: &str,
) -> bool {
    tokio::select! {
        biased;
        () = reply_audio_muted(reply_audio) => true,
        keep_going = synthesize_sentence(tts, events, sentence, turn, recorder, failure_sent, voice_name) => keep_going,
    }
}

async fn reply_audio_muted(reply_audio: &mut watch::Receiver<bool>) {
    if reply_audio.wait_for(|on| !*on).await.is_err() {
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
    voice_name: &str,
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
    let mut audio = match tts.synthesize_with_voice(sentence, voice_name).await {
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
            "voice": voice_name,
            "chars": sentence.chars().count(),
            "first_byte_ms": ms_since(first_byte_at),
            "total_ms": u64::try_from(tts_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "audio_ms": u64::try_from(audio_bytes).unwrap_or(u64::MAX) * 1000 / 48_000,
        }),
    );
    true
}

/// The metrics fields for a lost recognizer: a fixed category and whether reconnecting is
/// allowed, never the provider's message, which may echo request or transcript details.
fn stt_disconnect_fields(fatal: bool) -> serde_json::Value {
    json!({"category": "recognizer_disconnect", "fatal": fatal})
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
    recorder.emit("stt_disconnect", stt_disconnect_fields(is_fatal));
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
                recorder.emit("stt_disconnect", stt_disconnect_fields(true));
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
        ACK_LINES, AckCache, AckSettings, ActiveReply, ForwardClock, RecognizerMessage,
        ReplyContext, ReplyEvent, record_assistant_history, reply_loop_ms, run_recognizer,
        run_reply,
    };
    use crate::model::{ChatEvent, ChatMessage, ChatModel, DeltaStream, ModelError};
    use crate::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerFactory};
    use crate::recorder::{RecordKinds, RecordingConfig, start_recording};
    use crate::store::SessionStore;
    use crate::tts::{AudioStream, Tts, TtsError};
    use bytes::Bytes;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{Notify, mpsc, watch};
    use tokio::time::Instant;

    static UNIT_STORES: AtomicUsize = AtomicUsize::new(0);

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

    struct EchoToolModel {
        echo: &'static str,
    }

    impl ChatModel for EchoToolModel {
        fn stream_reply(
            &self,
            history: &[ChatMessage],
            audio: Option<&[u8]>,
        ) -> impl std::future::Future<Output = Result<DeltaStream, ModelError>> + Send {
            FakeModel.stream_reply(history, audio)
        }

        fn stream_reply_with_tools(
            &self,
            _history: &[ChatMessage],
            _audio: Option<&[u8]>,
            _tools: &[crate::model::ToolDefinition],
            results: &[crate::model::ToolResult],
        ) -> impl std::future::Future<Output = Result<crate::model::EventStream, ModelError>> + Send
        {
            let events = if results.is_empty() {
                vec![
                    Ok(ChatEvent::Heard(self.echo.to_string())),
                    Ok(ChatEvent::ToolCall(crate::model::ToolCall {
                        id: "search".to_string(),
                        name: "search_history".to_string(),
                        arguments: r#"{"query":"demo"}"#.to_string(),
                    })),
                ]
            } else {
                vec![Ok(ChatEvent::Text("Answer.".to_string()))]
            };
            let stream: crate::model::EventStream = Box::pin(futures_util::stream::iter(events));
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

    struct ToolTimingModel {
        first_finished: Arc<Notify>,
        next_started: Arc<Notify>,
        outcome: &'static str,
    }

    impl ChatModel for ToolTimingModel {
        fn stream_reply(
            &self,
            history: &[ChatMessage],
            audio: Option<&[u8]>,
        ) -> impl std::future::Future<Output = Result<DeltaStream, ModelError>> + Send {
            FakeModel.stream_reply(history, audio)
        }

        fn stream_reply_with_tools(
            &self,
            _history: &[ChatMessage],
            _audio: Option<&[u8]>,
            _tools: &[crate::model::ToolDefinition],
            results: &[crate::model::ToolResult],
        ) -> impl std::future::Future<Output = Result<crate::model::EventStream, ModelError>> + Send
        {
            use crate::model::{ChatEvent, ToolCall};
            if !results.is_empty() {
                self.next_started.notify_one();
                let result: Result<crate::model::EventStream, ModelError> = match self.outcome {
                    "failed" => Err(ModelError("model failed".to_string())),
                    "interrupted" => Ok(Box::pin(futures_util::stream::pending())),
                    _ => Ok(Box::pin(futures_util::stream::iter([Ok(ChatEvent::Text(
                        "Answer.".to_string(),
                    ))]))),
                };
                return std::future::ready(result);
            }
            let mut event = Some(Ok(ChatEvent::ToolCall(ToolCall {
                id: "search".to_string(),
                name: "search_history".to_string(),
                arguments: r#"{"query":"demo"}"#.to_string(),
            })));
            let finished = Arc::clone(&self.first_finished);
            let stream: crate::model::EventStream =
                Box::pin(futures_util::stream::poll_fn(move |_| {
                    let next = event.take();
                    if next.is_none() {
                        finished.notify_one();
                    }
                    std::task::Poll::Ready(next)
                }));
            std::future::ready(Ok(stream))
        }
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

    fn context(ack: AckSettings) -> ReplyContext {
        let dir = std::env::temp_dir().join(format!(
            "thalvi-session-unit-{}-{}",
            std::process::id(),
            UNIT_STORES.fetch_add(1, Ordering::SeqCst)
        ));
        let store = SessionStore::open(&dir).expect("scratch store");
        // Nothing here saves, so the directory is only needed to open the store.
        let _ = std::fs::remove_dir_all(&dir);
        ReplyContext {
            ack,
            voice_name: "af_heart".to_string(),
            past: Some(Arc::new(store)),
            session_id: "unit".to_string(),
        }
    }

    fn describe(event: &ReplyEvent) -> String {
        match event {
            ReplyEvent::Misheard => "misheard".to_string(),
            ReplyEvent::Delta(text) => format!("delta:{text}"),
            ReplyEvent::Sentence(sentence) => format!("sentence:{sentence}"),
            ReplyEvent::Audio(_) => "audio".to_string(),
            ReplyEvent::AckAudio(_) => "ack_audio".to_string(),
            ReplyEvent::Finished => "finished".to_string(),
            ReplyEvent::Failed(message) => format!("failed:{message}"),
            ReplyEvent::FirstSentence(_) => "first_sentence".to_string(),
            ReplyEvent::ReplyLoopFinished(_) => "reply_loop_finished".to_string(),
            ReplyEvent::Task(step) => format!("task:{}", step.status.as_str()),
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
    async fn audio_echo_guards_tool_calls_and_accepts_matching_short_reply() {
        for (echo, rejected) in [("no", true), ("yes", false)] {
            let tts = Arc::new(FakeTts {
                spoken: Arc::new(Mutex::new(Vec::new())),
                entered: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
                block_first: AtomicBool::new(false),
            });
            let (_audio_tx, audio_rx) = watch::channel(false);
            let (events_tx, mut events_rx) = mpsc::channel(64);
            let recorder =
                start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
            let handle = tokio::spawn(run_reply(
                Arc::new(EchoToolModel { echo }),
                tts,
                audio_rx,
                vec![ChatMessage::user("yes".to_string())],
                Some((vec![0, 0], "yes".to_string())),
                events_tx,
                1,
                recorder,
                context(AckSettings {
                    enabled: false,
                    cache: new_ack_cache(),
                }),
            ));
            let seen = collect_until_finished(&mut events_rx).await;
            handle.await.expect("reply task should finish");
            assert_eq!(seen.contains(&"misheard".to_string()), rejected, "{seen:?}");
            assert_eq!(
                seen.iter().any(|event| event.starts_with("task:")),
                !rejected,
                "{seen:?}"
            );
            assert_eq!(
                seen.iter().any(|event| event == "delta:Answer."),
                !rejected,
                "{seen:?}"
            );
        }
    }

    #[test]
    fn checking_an_audio_echo_keeps_speech_text_out_of_normal_logs() {
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || CapturedLog(Arc::clone(&output)))
            .finish();
        let heard = "invented speech one";
        let caption = "unrelated caption two";
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("capture control");
            assert!(super::echo_disagrees(heard, caption));
        });
        let log = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(log.contains("capture control"));
        assert!(!log.contains(heard));
        assert!(!log.contains(caption));
    }

    struct FailingRecognizer {
        error: Option<RecognizerError>,
    }

    impl Recognizer for FailingRecognizer {
        fn send_audio(
            &mut self,
            _: &[u8],
        ) -> impl std::future::Future<Output = Result<(), RecognizerError>> + Send {
            std::future::ready(Ok(()))
        }

        fn next_event(
            &mut self,
        ) -> impl std::future::Future<Output = Option<Result<RecognizerEvent, RecognizerError>>> + Send
        {
            std::future::ready(self.error.take().map(Err))
        }

        async fn close(&mut self) {}
    }

    struct FailingFactory {
        calls: AtomicUsize,
        during_connect: bool,
        marker: &'static str,
    }

    impl RecognizerFactory for FailingFactory {
        type Rec = FailingRecognizer;

        fn connect(
            &self,
        ) -> impl std::future::Future<Output = Result<FailingRecognizer, RecognizerError>> + Send
        {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            let fatal = RecognizerError::Fatal(self.marker.to_owned());
            std::future::ready(if self.during_connect {
                Err(if attempt == 0 {
                    RecognizerError::Retryable(self.marker.to_owned())
                } else {
                    fatal
                })
            } else {
                Ok(FailingRecognizer { error: Some(fatal) })
            })
        }
    }

    fn recorded_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
        let mut pending = vec![root.to_path_buf()];
        let mut files = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let text = String::from_utf8(std::fs::read(&path).unwrap())
                        .expect("private metrics must contain only text artifacts");
                    files.push((path, text));
                }
            }
        }
        files
    }

    #[tokio::test(start_paused = true)]
    async fn private_disconnect_and_reconnect_keep_provider_text_out_of_every_recording() {
        for (during_connect, expected_fatal) in [(false, vec![true]), (true, vec![false, true])] {
            let marker = "PROVIDER_MARKER_7f3a_transcript_echo";
            let data_dir = std::env::temp_dir().join(format!(
                "thalvi-session-stt-private-{}-{during_connect}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&data_dir);
            let recording = start_recording(
                RecordingConfig {
                    kinds: RecordKinds {
                        metrics: true,
                        text: true,
                        audio: true,
                    },
                    data_dir: data_dir.clone(),
                    stt_model: "test-stt".to_owned(),
                    llm_model: String::new(),
                    reasoning_effort: None,
                    tts_model: String::new(),
                    tts_voice: String::new(),
                }
                .metrics_only(),
                "private-stt-test".to_owned(),
            );
            let (audio_tx, audio_rx) = mpsc::channel(32);
            let (events_tx, mut events_rx) = mpsc::channel(32);
            let task = tokio::spawn(run_recognizer(
                Arc::new(FailingFactory {
                    calls: AtomicUsize::new(0),
                    during_connect,
                    marker,
                }),
                audio_rx,
                events_tx,
                recording.recorder.clone(),
            ));
            for expected in &expected_fatal {
                loop {
                    let event = tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    if let RecognizerMessage::Disconnected { message, fatal } = event {
                        assert_eq!((message.as_str(), fatal), (marker, *expected));
                        break;
                    }
                }
            }
            drop(audio_tx);
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            recording.finish(0).await;

            let files = recorded_files(&data_dir);
            std::fs::remove_dir_all(&data_dir).unwrap();
            assert!(!files.is_empty(), "the metrics recording must exist");
            let mut actual_fatal = Vec::new();
            for (path, text) in files {
                assert!(
                    !text.contains(marker),
                    "provider text reached {}",
                    path.display()
                );
                for line in text.lines() {
                    let event: serde_json::Value = serde_json::from_str(line).unwrap();
                    if event["event"] == "stt_disconnect" {
                        assert_eq!(event["category"], "recognizer_disconnect");
                        assert!(event.get("reason").is_none());
                        actual_fatal.push(event["fatal"].as_bool().unwrap());
                    }
                }
            }
            assert_eq!(
                actual_fatal, expected_fatal,
                "during_connect={during_connect}"
            );
        }
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
        let (_audio_tx, audio_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(FakeModel),
            tts,
            audio_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            context(AckSettings {
                enabled: false,
                cache: new_ack_cache(),
            }),
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
        let (_audio_tx, audio_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(ManySentenceModel),
            tts,
            audio_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            context(AckSettings {
                enabled: false,
                cache: new_ack_cache(),
            }),
        ));
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("tts should start within timeout");
        let mut deltas = 0;
        let mut reply_loop_finished = false;
        while !reply_loop_finished {
            let description = next_description(&mut events_rx).await;
            if description.starts_with("delta:") {
                deltas += 1;
            }
            reply_loop_finished = description == "reply_loop_finished";
        }
        assert_eq!(
            deltas, 12,
            "every delta should arrive while the first sentence is blocked in tts"
        );
        assert_eq!(
            spoken.lock().expect("spoken lock").as_slice(),
            ["Sentence 1."],
            "reply_loop_finished should be stamped before tts works through the queue"
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
    async fn reply_audio_off_streams_deltas_without_synthesis_and_finishes() {
        let spoken = Arc::new(Mutex::new(Vec::new()));
        let tts = Arc::new(FakeTts {
            spoken: Arc::clone(&spoken),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            block_first: AtomicBool::new(false),
        });
        let (_audio_tx, audio_rx) = watch::channel(false);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(FakeModel),
            tts,
            audio_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            context(AckSettings {
                enabled: false,
                cache: new_ack_cache(),
            }),
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
            "reply audio off should not synthesize: {seen:?}"
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
        let (_audio_tx, audio_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(GatedModel {
                release: Arc::clone(&release),
            }),
            tts,
            audio_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            context(AckSettings {
                enabled: true,
                cache: new_ack_cache(),
            }),
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
        let (_audio_tx, audio_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(FakeModel),
            tts,
            audio_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            context(AckSettings {
                enabled: true,
                cache: new_ack_cache(),
            }),
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
            let (_audio_tx, audio_rx) = watch::channel(true);
            let (events_tx, mut events_rx) = mpsc::channel(64);
            let recorder =
                start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
            let handle = tokio::spawn(run_reply(
                Arc::new(GatedModel {
                    release: Arc::clone(&release),
                }),
                Arc::clone(&tts),
                audio_rx,
                Vec::new(),
                None,
                events_tx,
                turn,
                recorder,
                context(AckSettings {
                    enabled: true,
                    cache: Arc::clone(&cache),
                }),
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

    struct VoiceTts {
        requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Tts for VoiceTts {
        fn synthesize(
            &self,
            _text: &str,
        ) -> impl std::future::Future<Output = Result<AudioStream, TtsError>> + Send {
            std::future::ready(Err(TtsError("the reply must pick its voice".to_string())))
        }

        fn synthesize_with_voice(
            &self,
            text: &str,
            voice: &str,
        ) -> impl std::future::Future<Output = Result<AudioStream, TtsError>> + Send {
            self.requests
                .lock()
                .expect("requests lock")
                .push((text.to_string(), voice.to_string()));
            let audio: AudioStream = Box::pin(futures_util::stream::iter(vec![Ok(
                Bytes::from_static(b"audio"),
            )]));
            std::future::ready(Ok(audio))
        }
    }

    #[tokio::test]
    async fn acknowledgements_and_sentences_use_the_reply_voice_and_are_cached_per_voice() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let tts = Arc::new(VoiceTts {
            requests: Arc::clone(&requests),
        });
        let cache = new_ack_cache();
        // Turns 1, 4 and 7 draw the same acknowledgement line.
        for (turn, voice) in [(1_u64, "af_heart"), (4, "sohee"), (7, "sohee")] {
            let release = Arc::new(Notify::new());
            let (_audio_tx, audio_rx) = watch::channel(true);
            let (events_tx, mut events_rx) = mpsc::channel(64);
            let recorder =
                start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
            let handle = tokio::spawn(run_reply(
                Arc::new(GatedModel {
                    release: Arc::clone(&release),
                }),
                Arc::clone(&tts),
                audio_rx,
                Vec::new(),
                None,
                events_tx,
                turn,
                recorder,
                ReplyContext {
                    voice_name: voice.to_string(),
                    ..context(AckSettings {
                        enabled: true,
                        cache: Arc::clone(&cache),
                    })
                },
            ));
            assert_eq!(next_description(&mut events_rx).await, "ack_audio");
            release.notify_one();
            collect_until_finished(&mut events_rx).await;
            handle.await.expect("reply task should finish");
        }
        let ack = ACK_LINES[0].to_string();
        let answer = "First answer.".to_string();
        assert_eq!(
            *requests.lock().expect("requests lock"),
            [
                (ack.clone(), "af_heart".to_string()),
                (answer.clone(), "af_heart".to_string()),
                (ack, "sohee".to_string()),
                (answer.clone(), "sohee".to_string()),
                (answer, "sohee".to_string()),
            ],
            "each voice synthesizes the acknowledgement once; the second sohee reply replays it"
        );
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
        let (_audio_tx, audio_rx) = watch::channel(true);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let recorder = start_recording(RecordingConfig::disabled(), "test".to_string()).recorder;
        let handle = tokio::spawn(run_reply(
            Arc::new(GatedModel {
                release: Arc::clone(&release),
            }),
            tts,
            audio_rx,
            Vec::new(),
            None,
            events_tx,
            1,
            recorder,
            context(AckSettings {
                enabled: true,
                cache: new_ack_cache(),
            }),
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
            reply_loop_finished_at: None,
            first_sentence_at: None,
            first_audio_at: None,
            ack_first_audio_at: None,
            audio_bytes: 0,
            input_audio_ms: None,
            misheard: false,
            voice_name: "af_heart".to_string(),
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
    fn reply_loop_ms_ends_at_model_finish_when_known_and_at_record_time_otherwise() {
        let started = Instant::now();
        let reply_loop_finished = started + Duration::from_millis(120);
        let recorded = started + Duration::from_millis(5_000);
        assert_eq!(
            reply_loop_ms(started, Some(reply_loop_finished), recorded),
            120
        );
        assert_eq!(reply_loop_ms(started, None, recorded), 5_000);
    }

    #[tokio::test(start_paused = true)]
    async fn each_model_call_records_its_outcome_without_the_delay_between_tool_rounds() {
        for outcome in ["ok", "failed", "interrupted"] {
            let dir = std::env::temp_dir().join(format!(
                "thalvi-model-timing-{}-{outcome}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let mut config = RecordingConfig::disabled();
            config.kinds.metrics = true;
            config.data_dir = dir.clone();
            let recording = start_recording(config, "timing".to_string());
            let first_finished = Arc::new(Notify::new());
            let next_started = Arc::new(Notify::new());
            let model = Arc::new(ToolTimingModel {
                first_finished: Arc::clone(&first_finished),
                next_started: Arc::clone(&next_started),
                outcome,
            });
            let store = context(AckSettings {
                enabled: false,
                cache: new_ack_cache(),
            })
            .past;
            let (events, mut received) = mpsc::channel(1);
            let reservation = events.clone();
            let permit = reservation.reserve().await.expect("reserve event capacity");
            let (sentences, _sentence_rx) = mpsc::unbounded_channel();
            let started = Instant::now();
            let producer = tokio::spawn(super::produce_sentences(
                model,
                Vec::new(),
                None,
                events,
                sentences,
                Arc::new(AtomicBool::new(false)),
                super::ToolRun {
                    past: store,
                    session_id: "unit".to_string(),
                    recorder: recording.recorder.clone(),
                    turn: 1,
                },
            ));
            tokio::time::timeout(Duration::from_secs(5), first_finished.notified())
                .await
                .expect("first model call finished");
            tokio::time::advance(Duration::from_secs(5)).await;
            drop(permit);
            drop(reservation);
            loop {
                let event = tokio::time::timeout(Duration::from_secs(5), received.recv())
                    .await
                    .expect("tool event arrived")
                    .expect("tool events");
                if matches!(event, ReplyEvent::Task(step) if step.status == super::TaskStatus::Done)
                {
                    break;
                }
            }
            tokio::time::timeout(Duration::from_secs(5), next_started.notified())
                .await
                .expect("next model call started");
            if outcome == "interrupted" {
                producer.abort();
            }
            let mut loop_finished = None;
            while let Some(event) = tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .expect("model event arrived")
            {
                if let ReplyEvent::ReplyLoopFinished(at) = event {
                    loop_finished = Some(at);
                }
            }
            let _ = producer.await;
            if outcome == "ok" {
                assert_eq!(
                    loop_finished.expect("loop complete") - started,
                    Duration::from_secs(5)
                );
            }
            recording.finish(5_000).await;
            let date = std::fs::read_dir(dir.join("recordings"))
                .expect("recording directory")
                .next()
                .expect("dated recordings")
                .expect("date entry")
                .path();
            let lines =
                std::fs::read_to_string(date.join("timing.jsonl")).expect("recorded metrics");
            let calls: Vec<serde_json::Value> = lines
                .lines()
                .map(|line| serde_json::from_str(line).expect("metric JSON"))
                .filter(|line: &serde_json::Value| line["event"] == "model_call")
                .collect();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["outcome"], "ok");
            assert_eq!(calls[1]["outcome"], outcome);
            assert_eq!(calls[0]["total_ms"], 0);
            assert_eq!(calls[1]["total_ms"], 0);
            let _ = std::fs::remove_dir_all(&dir);
        }
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
        let super::Opened { persistence, .. } = super::Retention::<FakeModel>::Saved {
            store: Arc::clone(&store),
            resume: None,
            titler: None,
        }
        .open()
        .await;
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

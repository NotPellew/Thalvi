//! Runs the streaming recognizers switched on as extras beside the live one: each hears the capture's frames in a task of its own and gives its final text to the merge or the recording.

use std::collections::VecDeque;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::{Answer, Extra, READY_TIMEOUT, Unheard, until};
use crate::capture::{CaptureRoute, has_words};
use crate::entries::LiveEntry;
use crate::frame::Frame;
use crate::merge;
use crate::metrics::Dictation;
use crate::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerSession};

/// A live entry other than the active one, switched on as an extra, with its recognizer.
pub struct StreamingExtra<R> {
    pub entry: LiveEntry,
    pub recognizer: Arc<R>,
}

/// What the live capture's end tells an extra.
#[derive(Clone, Copy)]
struct Finish {
    at: Instant,
    route: CaptureRoute,
    has_endings: bool,
}

enum Order {
    Frame(Box<Frame>),
    Finish(Finish),
}

struct Line {
    orders: UnboundedSender<Order>,
    cancel: Arc<Notify>,
}

/// The extras of one capture while they run.
#[derive(Default)]
pub(super) struct Streams {
    lines: Vec<Line>,
    /// One answer per extra, once it has its text, failed, or was cancelled.
    pub(super) tasks: JoinSet<Answer>,
}

impl Streams {
    /// Starts every extra connecting at once. Their slots in the merge begin at `first`.
    pub(super) fn start<R>(
        extras: &[StreamingExtra<R>],
        first: usize,
        dictation: &Dictation,
    ) -> Self
    where
        R: Recognizer + 'static,
        R::Session: 'static,
    {
        let mut streams = Self::default();
        for (at, extra) in extras.iter().enumerate() {
            let (orders, inbox) = unbounded_channel();
            let cancel = Arc::new(Notify::new());
            streams.lines.push(Line {
                orders,
                cancel: Arc::clone(&cancel),
            });
            streams.tasks.spawn(run(
                Arc::clone(&extra.recognizer),
                inbox,
                cancel,
                (
                    first + at,
                    Extra::streaming(&extra.entry),
                    dictation.clone(),
                ),
            ));
        }
        streams
    }

    /// Gives every extra the frames the live recognizer is sent.
    pub(super) fn send(&self, frames: &[Frame]) {
        for line in &self.lines {
            for frame in frames {
                let _ = line.orders.send(Order::Frame(Box::new(*frame)));
            }
        }
    }

    /// Tells every extra the live capture was accepted; they flush while the delivery goes on.
    pub(super) fn finish(&self, route: CaptureRoute, has_endings: bool) {
        let at = Instant::now();
        for line in &self.lines {
            let _ = line.orders.send(Order::Finish(Finish {
                at,
                route,
                has_endings,
            }));
        }
    }

    /// Has every extra drop its session; the tasks end with a cancelled answer.
    pub(super) fn cancel(self) -> JoinSet<Answer> {
        for line in &self.lines {
            line.cancel.notify_one();
        }
        self.tasks
    }
}

/// What a recognizer said, as far as the merge needs it.
#[derive(Default)]
struct Text {
    finals: Vec<String>,
    /// A partial with words came after the last final.
    unfinished: bool,
}

impl Text {
    fn on_event(&mut self, event: RecognizerEvent, buffers: bool) {
        match event {
            RecognizerEvent::Partial(text) => self.unfinished |= has_words(&text),
            RecognizerEvent::Final(text) => {
                self.unfinished = false;
                if buffers || has_words(&text) {
                    self.finals.push(text);
                }
            }
            RecognizerEvent::Ended => {}
        }
    }

    /// Whether all the capture's words are in: a buffered upload answers once, a stream when
    /// nothing it heard is left unfinalized.
    fn complete(&self, buffers: bool) -> bool {
        !self.finals.is_empty() && (buffers || !self.unfinished)
    }
}

enum Step {
    Cancel,
    Order(Option<Order>),
    Event(Result<RecognizerEvent, RecognizerError>),
    TooLate,
}

/// Sends `frame` unless the capture is cancelled meanwhile.
async fn send_frame<S: RecognizerSession>(
    session: &mut S,
    cancel: &Notify,
    frame: &Frame,
) -> Result<(), Unheard> {
    tokio::select! {
        biased;
        () = cancel.notified() => Err(Unheard::Cancelled),
        sent = session.send_frame(frame) => sent.map_err(|_| Unheard::Failed),
    }
}

/// Connects, forwards the frames and waits for the text; also when it ended.
async fn hear<R: Recognizer>(
    recognizer: &R,
    orders: &mut UnboundedReceiver<Order>,
    cancel: &Notify,
    finish: &mut Option<Finish>,
) -> (Result<String, Unheard>, Instant) {
    let mut connect = pin!(tokio::time::timeout(READY_TIMEOUT, recognizer.connect()));
    let mut backlog = VecDeque::new();
    let mut open = true;
    let mut session = loop {
        tokio::select! {
            biased;
            () = cancel.notified() => return (Err(Unheard::Cancelled), Instant::now()),
            connected = &mut connect => match connected {
                Ok(Ok(session)) => break session,
                Ok(Err(_)) => return (Err(Unheard::Failed), Instant::now()),
                Err(_) => return (Err(Unheard::Timeout), Instant::now()),
            },
            order = orders.recv(), if open => match order {
                Some(order) => {
                    if let Order::Finish(ending) = &order {
                        *finish = Some(*ending);
                    }
                    backlog.push_back(order);
                }
                None => open = false,
            },
        }
    };
    let buffers = session.buffers_audio();
    let mut text = Text::default();
    let ended = loop {
        let step = if let Some(order) = backlog.pop_front() {
            Step::Order(Some(order))
        } else {
            tokio::select! {
                biased;
                () = cancel.notified() => Step::Cancel,
                order = orders.recv(), if finish.is_none() => Step::Order(order),
                event = session.next_event() => Step::Event(event),
                () = until(finish.as_ref().map(|finish| finish.at + merge::LIMIT)) => Step::TooLate,
            }
        };
        let stepped = match step {
            // The engine let go of the capture without ending it, or cancelled it.
            Step::Cancel | Step::Order(None) => Err(Unheard::Cancelled),
            Step::TooLate => Err(Unheard::Timeout),
            Step::Order(Some(Order::Frame(frame))) => {
                send_frame(&mut session, cancel, &frame).await
            }
            Step::Order(Some(Order::Finish(ending))) => {
                *finish = Some(ending);
                if buffers {
                    tokio::select! {
                        biased;
                        () = cancel.notified() => Err(Unheard::Cancelled),
                        done = session.finish_audio() => done.map_err(|_| Unheard::Failed),
                    }
                } else {
                    Ok(())
                }
            }
            Step::Event(Ok(event)) => {
                text.on_event(event, buffers);
                Ok(())
            }
            Step::Event(Err(_)) => Err(Unheard::Failed),
        };
        match stepped {
            Err(why) => break (Err(why), Instant::now()),
            Ok(()) if finish.is_some() && text.complete(buffers) => {
                break (Ok(text.finals.join(" ")), Instant::now());
            }
            Ok(()) => {}
        }
    };
    match ended.0 {
        Ok(_) => session.close().await,
        Err(_) => session.cancel().await,
    }
    ended
}

/// Waits for the end of the capture after a failure, so the answer can say how it ended.
async fn park(orders: &mut UnboundedReceiver<Order>, cancel: &Notify) -> Option<Finish> {
    loop {
        tokio::select! {
            biased;
            () = cancel.notified() => return None,
            order = orders.recv() => match order {
                Some(Order::Finish(finish)) => return Some(finish),
                Some(Order::Frame(_)) => {}
                None => return None,
            },
        }
    }
}

/// One extra's capture: its answer arrives only once the capture has ended or been cancelled.
async fn run<R: Recognizer>(
    recognizer: Arc<R>,
    mut orders: UnboundedReceiver<Order>,
    cancel: Arc<Notify>,
    (index, extra, dictation): (usize, Extra, Dictation),
) -> Answer {
    let mut finish = None;
    let (mut heard, ended) = hear(&*recognizer, &mut orders, &cancel, &mut finish).await;
    if matches!(heard, Err(Unheard::Failed | Unheard::Timeout)) && finish.is_none() {
        finish = park(&mut orders, &cancel).await;
        if finish.is_none() {
            heard = Err(Unheard::Cancelled);
        }
    }
    let (route, has_endings, took) =
        finish.map_or((CaptureRoute::Paste, true, Duration::ZERO), |finish| {
            (
                finish.route,
                finish.has_endings,
                ended.saturating_duration_since(finish.at),
            )
        });
    Answer {
        index,
        extra,
        dictation,
        route,
        has_endings,
        heard,
        took,
    }
}

/// Replays a finished capture through a recognizer that normally takes live audio.
pub(super) async fn finished<R: Recognizer>(
    recognizer: &R,
    frames: &[Frame],
) -> Result<String, super::final_pass::Outcome> {
    use super::{RELEASE_SILENCE, final_pass::Outcome};
    use crate::recognizer::ConnectFailure;

    let replay = async {
        let mut session = recognizer.connect().await?;
        let buffers = session.buffers_audio();
        let mut text = Text::default();
        if buffers {
            for frame in frames {
                session.send_frame(frame).await?;
            }
            session.finish_audio().await?;
            loop {
                text.on_event(session.next_event().await?, buffers);
                if text.complete(buffers) {
                    break;
                }
            }
        } else {
            if session.needs_release_silence() {
                for frame in frames.iter().chain(&RELEASE_SILENCE) {
                    session.send_frame(frame).await?;
                }
            } else {
                for frame in frames {
                    session.send_frame(frame).await?;
                }
                session.input_ended();
            }
            loop {
                let settled = text
                    .complete(buffers)
                    .then(|| Instant::now() + session.release_limit());
                tokio::select! {
                    biased;
                    event = session.next_event() => {
                        let event = event?;
                        let ended = matches!(event, RecognizerEvent::Ended);
                        text.on_event(event, buffers);
                        if ended {
                            break;
                        }
                    }
                    () = until(settled) => break,
                }
            }
        }
        Ok::<_, RecognizerError>(text.finals.join(" "))
    };
    match tokio::time::timeout(merge::LIMIT, replay).await {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(RecognizerError::Unreachable {
            reason: ConnectFailure::Rejected(code),
            ..
        })) => Err(Outcome::HttpStatus(code)),
        Ok(Err(
            RecognizerError::Certificate { .. }
            | RecognizerError::Unreachable { .. }
            | RecognizerError::Dropped { .. }
            | RecognizerError::Model { .. },
        )) => Err(Outcome::Unreachable),
        Ok(Err(_)) => Err(Outcome::BadReply),
        Err(_) => Err(Outcome::Timeout),
    }
}

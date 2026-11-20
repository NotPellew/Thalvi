//! Selects a single recognizer for finished audio and bounds its influence on delivery.

use std::{sync::Arc, time::Duration};

use serde_json::json;
use tokio::{sync::oneshot, task::AbortHandle, time::Instant};

use super::{Engine, Extra, millis, streaming};
use crate::{
    capture::{CaptureRequest, CaptureRoute, has_words},
    config::DesktopConfig,
    entries::{FinalEntry, LiveKind},
    frame::Frame,
    metrics::{Dictation, Event, Recorder},
    recognizer::Recognizer,
    secondary::{self, OpenAiLive, SecondaryApi, SecondaryError, SecondaryStt},
};

const LIMIT: Duration = Duration::from_secs(2);

enum Source<R> {
    OpenAi(OpenAiLive),
    Meta(SecondaryStt),
    Stream(Arc<R>),
}

pub(super) struct FinalPass<R> {
    label: Extra,
    source: Source<R>,
    prompt: String,
}

impl<R> FinalPass<R> {
    pub(super) const fn label(&self) -> &Extra {
        &self.label
    }

    pub(super) fn of(
        config: &DesktopConfig,
        streaming: &[streaming::StreamingExtra<R>],
    ) -> Option<Self> {
        let (label, source) = match config.stt.entries.final_pass()? {
            FinalEntry::Upload(entry) => (
                Extra::upload(entry),
                match entry.settings.api() {
                    SecondaryApi::OpenAi => Source::OpenAi(OpenAiLive {
                        settings: entry.settings.clone(),
                        request: secondary::LiveRequest::Transcriptions,
                        language: None,
                    }),
                    SecondaryApi::Meta => Source::Meta(entry.settings.clone()),
                },
            ),
            FinalEntry::Live(entry) => (
                Extra::streaming(entry),
                match &entry.kind {
                    LiveKind::OpenAi(open_ai) => Source::OpenAi(open_ai.clone()),
                    _ => Source::Stream(Arc::clone(
                        &streaming
                            .iter()
                            .find(|one| one.entry.id == entry.id)?
                            .recognizer,
                    )),
                },
            ),
        };
        Some(Self {
            label,
            source,
            prompt: config.stt.phrases.phrases().join(", "),
        })
    }
}

#[derive(Clone, Copy)]
pub(super) enum Outcome {
    Replaced,
    Timeout,
    Unreachable,
    HttpStatus(u16),
    BadReply,
    Disagreed,
    SkippedScratch,
    SkippedTask,
    SkippedEmpty,
    SkippedFailed,
    Cancelled,
}

impl Outcome {
    fn key(self) -> &'static str {
        match self {
            Self::Replaced => "replaced",
            Self::Timeout => "timeout",
            Self::Unreachable => "unreachable",
            Self::HttpStatus(_) => "http_status",
            Self::BadReply => "bad_reply",
            Self::Disagreed => "disagreed",
            Self::SkippedScratch => "skipped_scratch",
            Self::SkippedTask => "skipped_task",
            Self::SkippedEmpty => "skipped_empty",
            Self::SkippedFailed => "skipped_failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn reason(self) -> Option<String> {
        Some(match self {
            Self::Timeout => "took too long".to_owned(),
            Self::Unreachable => "could not be reached".to_owned(),
            Self::HttpStatus(code) => format!("returned HTTP {code}"),
            Self::BadReply => "returned an unreadable transcription".to_owned(),
            Self::Disagreed => "heard a different ending".to_owned(),
            _ => return None,
        })
    }
}

impl From<SecondaryError> for Outcome {
    fn from(error: SecondaryError) -> Self {
        match error {
            SecondaryError::Timeout => Self::Timeout,
            SecondaryError::Unreachable | SecondaryError::Certificate(_) => Self::Unreachable,
            SecondaryError::Status(code) => Self::HttpStatus(code),
            SecondaryError::BadResponse => Self::BadReply,
        }
    }
}

pub(super) struct Record {
    pub(super) outcome: Outcome,
    pub(super) ended: Instant,
    requested: Option<Duration>,
}

impl Record {
    pub(super) fn skipped(outcome: Outcome, ended: Instant) -> Self {
        Self {
            outcome,
            ended,
            requested: None,
        }
    }

    pub(super) fn emit(
        &self,
        recorder: &Recorder,
        entry: &Extra,
        dictation: &Dictation,
        delivered: Option<CaptureRoute>,
        cancelled: bool,
        live: &str,
    ) -> Option<String> {
        let outcome = if cancelled {
            Outcome::Cancelled
        } else {
            self.outcome
        };
        recorder.metric(
            Event::FinalPass,
            outcome.key(),
            self.requested,
            json!({
                "dictation": dictation.to_string(),
                "entry": entry.name,
                "model": entry.model,
                "api": entry.api,
                "request_ms": self.requested.map(millis),
                "release_to_paste_ms": delivered
                    .filter(|route| matches!(route, CaptureRoute::Paste | CaptureRoute::Submit | CaptureRoute::Write))
                    .map(|_| millis(self.ended.elapsed())),
            }),
        );
        let reason = outcome.reason()?;
        let verb = match delivered {
            Some(CaptureRoute::Paste | CaptureRoute::Submit) => "pasted",
            Some(_) => "used",
            None => return Some(format!("{} {reason}.", entry.name)),
        };
        Some(format!(
            "{} {reason}; {verb} what {live} heard.",
            entry.name
        ))
    }
}

struct Reply {
    text: Result<String, Outcome>,
    at: Instant,
}

pub(super) struct Request {
    ended: Instant,
    started: Instant,
    response: oneshot::Receiver<Reply>,
    abort: AbortHandle,
    keep_late: bool,
}

impl Request {
    pub(super) fn cancel(self) -> Record {
        self.abort.abort();
        self.record(Outcome::Cancelled, Instant::now())
    }

    fn record(&self, outcome: Outcome, at: Instant) -> Record {
        Record {
            outcome,
            ended: self.ended,
            requested: Some(at.saturating_duration_since(self.started)),
        }
    }
}

impl<R, W, C> Engine<R, W, C>
where
    R: Recognizer + 'static,
    R::Session: 'static,
    W: super::WakeDetector,
    C: super::CuePlayer + 'static,
{
    pub(super) fn skip_unfinished_final_pass(&mut self, outcome: &super::CaptureOutcome) {
        if self.final_dictation.is_none() || self.final_record.is_some() {
            return;
        }
        let skipped = match outcome {
            super::CaptureOutcome::Cancelled { .. } => Outcome::Cancelled,
            super::CaptureOutcome::Failed {
                error: super::CaptureError::NothingHeard | super::CaptureError::NoWords(_),
                ..
            } => Outcome::SkippedEmpty,
            _ => Outcome::SkippedFailed,
        };
        self.final_record = Some(Record::skipped(skipped, Instant::now()));
    }

    pub(super) fn start_final_pass(
        &mut self,
        request: &CaptureRequest,
        frames: &mut Vec<Frame>,
        dictation: &Dictation,
        ended: Instant,
        scratched: bool,
    ) -> Option<Request> {
        let pass = Arc::clone(self.final_pass.as_ref()?);
        let skipped = match request.route {
            _ if scratched => Some(Outcome::SkippedScratch),
            CaptureRoute::Task => Some(Outcome::SkippedTask),
            _ if !has_words(&request.text) => Some(Outcome::SkippedEmpty),
            _ => None,
        };
        self.recorder.text(
            dictation,
            format!("live ({}): {}", self.speech.name(), request.text),
        );
        if let Some(skipped) = skipped {
            self.final_record = Some(Record::skipped(skipped, ended));
            return None;
        }
        let frames = std::mem::take(frames);
        let (send, response) = oneshot::channel();
        let client = self.http.clone();
        let recording = self.recorder.capture_text(dictation.clone());
        let keep_late = recording.is_some();
        while self.final_tasks.try_join_next().is_some() {}
        let started = Instant::now();
        let abort = self.final_tasks.spawn(async move {
            let text = match &pass.source {
                Source::OpenAi(entry) => {
                    secondary::transcribe_finished(&client, entry, &frames, &pass.prompt)
                        .await
                        .map_err(Outcome::from)
                }
                Source::Meta(entry) => secondary::transcribe(&client, entry, &frames)
                    .await
                    .map_err(Outcome::from),
                Source::Stream(recognizer) => streaming::finished(&**recognizer, &frames).await,
            };
            let at = Instant::now();
            if let (Some(record), Ok(raw)) = (recording, &text) {
                record(format!("final pass ({}): {raw}", pass.label.name));
            }
            let _ = send.send(Reply { text, at });
        });
        self.set_state(super::EngineState::FinalPass {
            route: request.route,
        });
        Some(Request {
            ended,
            started,
            response,
            abort,
            keep_late,
        })
    }

    pub(super) async fn wait_final_pass(
        &mut self,
        mut request: CaptureRequest,
        mut pending: Request,
        has_endings: bool,
    ) -> super::CaptureOutcome {
        let deadline = pending.ended + LIMIT;
        let reply = tokio::select! {
            biased;
            _ = self.cancel.changed() => {
                self.final_record = Some(pending.cancel());
                return super::CaptureOutcome::cancelled_with(request.text);
            }
            result = &mut pending.response => result.ok(),
            () = tokio::time::sleep_until(deadline) => None,
        };
        let (outcome, at) = match reply {
            Some(Reply { text, at }) if at < deadline => {
                let outcome = match text {
                    Ok(raw) if !has_words(&raw) => Outcome::BadReply,
                    Ok(raw) => {
                        if let Some(text) = self.agreed(&raw, request.route, has_endings, true) {
                            request.text = text;
                            Outcome::Replaced
                        } else {
                            Outcome::Disagreed
                        }
                    }
                    Err(outcome) => outcome,
                };
                (outcome, at)
            }
            _ => {
                if !pending.keep_late {
                    pending.abort.abort();
                }
                (Outcome::Timeout, deadline)
            }
        };
        self.final_record = Some(pending.record(outcome, at));
        super::CaptureOutcome::Accepted(request)
    }
}

//! Owns buffered capture uploads to an OpenAI-compatible transcription endpoint.

use std::time::Instant;
use tokio::sync::{Notify, mpsc};

use crate::{
    frame::Frame,
    metrics::{Event, Metrics},
    recognizer::{
        Recognizer, RecognizerError, RecognizerEvent, RecognizerSession, SpeechSource,
        TranscriptionFailure,
    },
    secondary::{SecondaryStt, transcribe_events},
};

pub struct OpenAiRecognizer {
    speech: SpeechSource,
    settings: SecondaryStt,
    client: reqwest::Client,
    metrics: Metrics,
}

impl OpenAiRecognizer {
    #[must_use]
    pub fn new(
        name: &str,
        settings: SecondaryStt,
        client: reqwest::Client,
        metrics: Metrics,
    ) -> Self {
        let speech = SpeechSource::direct(name, settings.address().to_owned());
        Self {
            speech,
            settings,
            client,
            metrics,
        }
    }
}

pub struct OpenAiSession {
    speech: SpeechSource,
    settings: SecondaryStt,
    frames: Vec<Frame>,
    finished: bool,
    client: reqwest::Client,
    events: mpsc::UnboundedReceiver<Result<RecognizerEvent, RecognizerError>>,
    event_sender: Option<mpsc::UnboundedSender<Result<RecognizerEvent, RecognizerError>>>,
    started: bool,
    released_at: Option<Instant>,
    metrics: Metrics,
    task: Option<tokio::task::JoinHandle<()>>,
    finished_signal: Notify,
}

impl Recognizer for OpenAiRecognizer {
    type Session = OpenAiSession;

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Connecting only creates the in-memory session state."
    )]
    async fn connect(&self) -> Result<Self::Session, RecognizerError> {
        let (sender, events) = mpsc::unbounded_channel();
        Ok(OpenAiSession {
            speech: self.speech.clone(),
            settings: self.settings.clone(),
            frames: Vec::new(),
            finished: false,
            client: self.client.clone(),
            events,
            event_sender: Some(sender),
            started: false,
            released_at: None,
            metrics: self.metrics.clone(),
            task: None,
            finished_signal: Notify::new(),
        })
    }
}

impl RecognizerSession for OpenAiSession {
    fn buffers_audio(&self) -> bool {
        true
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Finishing only updates buffered capture state."
    )]
    async fn finish_audio(&mut self) -> Result<(), RecognizerError> {
        self.finished = true;
        self.released_at = Some(Instant::now());
        self.finished_signal.notify_one();
        Ok(())
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Frames are buffered locally without I/O."
    )]
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), RecognizerError> {
        self.frames.push(*frame);
        Ok(())
    }

    async fn next_event(&mut self) -> Result<RecognizerEvent, RecognizerError> {
        if !self.finished {
            self.finished_signal.notified().await;
        }
        if !self.started {
            self.started = true;
            let client = self.client.clone();
            let speech = self.speech.clone();
            let settings = self.settings.clone();
            let frames = self.frames.clone();
            let metrics = self.metrics.clone();
            let released_at = self.released_at.unwrap_or_else(Instant::now);
            let Some(sender) = self.event_sender.take() else {
                return Err(RecognizerError::said(
                    "Speech transcription stopped unexpectedly.",
                ));
            };
            self.task = Some(tokio::spawn(async move {
                let started = Instant::now();
                let mut first_text = true;
                let mut deltas = 0_u32;
                let result = transcribe_events(&client, &settings, &frames, |event| {
                    if matches!(event, RecognizerEvent::Partial(_)) {
                        deltas = deltas.saturating_add(1);
                        metrics.metric(Event::LiveSttDelta, "ok", None, serde_json::json!({}));
                    }
                    if first_text
                        && matches!(
                            event,
                            RecognizerEvent::Partial(_) | RecognizerEvent::Final(_)
                        )
                    {
                        first_text = false;
                        metrics.metric(
                            Event::LiveSttFirstText,
                            "ok",
                            Some(released_at.elapsed()),
                            serde_json::json!({}),
                        );
                    }
                    let _ = sender.send(Ok(event));
                })
                .await;
                let outcome = if result.is_ok() { "ok" } else { "error" };
                metrics.metric(
                    Event::LiveStt,
                    outcome,
                    Some(started.elapsed()),
                    serde_json::json!({"model": settings.model(), "deltas": deltas}),
                );
                match result {
                    Ok(text) => {
                        if first_text {
                            metrics.metric(
                                Event::LiveSttFirstText,
                                "ok",
                                Some(released_at.elapsed()),
                                serde_json::json!({}),
                            );
                        }
                        let _ = sender.send(Ok(RecognizerEvent::Final(text)));
                    }
                    Err(error) => {
                        let (failure, message) = match error {
                            crate::secondary::SecondaryError::Timeout => (
                                TranscriptionFailure::Timeout,
                                format!(
                                    "{} took too long to transcribe this capture. Nothing was delivered. Check the address and API key under Settings → Speech.",
                                    speech.subject()
                                ),
                            ),
                            crate::secondary::SecondaryError::Unreachable => (
                                TranscriptionFailure::Unreachable,
                                format!(
                                    "{} could not be reached. Nothing was delivered. Check the address and API key under Settings → Speech.",
                                    speech.subject()
                                ),
                            ),
                            crate::secondary::SecondaryError::Status(code) => (
                                TranscriptionFailure::HttpStatus,
                                format!(
                                    "{} returned HTTP {code}. Nothing was delivered. Check the address and API key under Settings → Speech.",
                                    speech.subject()
                                ),
                            ),
                            crate::secondary::SecondaryError::BadResponse => (
                                TranscriptionFailure::BadReply,
                                format!(
                                    "{} returned an unreadable transcription. Nothing was delivered. Check the address and API key under Settings → Speech.",
                                    speech.subject()
                                ),
                            ),
                        };
                        let _ =
                            sender.send(Err(RecognizerError::Transcription { failure, message }));
                    }
                }
            }));
        }
        match self.events.recv().await {
            Some(Ok(RecognizerEvent::Final(text))) => Ok(RecognizerEvent::Final(text)),
            Some(event) => event,
            None => Err(RecognizerError::said(
                "Speech transcription stopped unexpectedly. Nothing was delivered.",
            )),
        }
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Cancellation only aborts the local upload task."
    )]
    async fn cancel(self) {
        if let Some(task) = self.task {
            task.abort();
        }
    }
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "Closing only aborts the local upload task."
    )]
    async fn close(self) {
        if let Some(task) = self.task {
            task.abort();
        }
    }
}

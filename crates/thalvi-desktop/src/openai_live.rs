//! Owns buffered capture uploads to an OpenAI-compatible transcription endpoint.

use std::time::Instant;
use tokio::sync::{Notify, mpsc};

use crate::{
    frame::Frame,
    metrics::{Event, Metrics},
    recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerSession},
    secondary::{SecondaryStt, transcribe_events},
};

pub struct OpenAiRecognizer {
    settings: SecondaryStt,
    client: reqwest::Client,
    metrics: Metrics,
}

impl OpenAiRecognizer {
    pub fn new(
        _name: &str,
        settings: SecondaryStt,
        client: reqwest::Client,
        metrics: Metrics,
    ) -> Self {
        Self {
            settings,
            client,
            metrics,
        }
    }
}

pub struct OpenAiSession {
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

    async fn connect(&self) -> Result<Self::Session, RecognizerError> {
        let (sender, events) = mpsc::unbounded_channel();
        Ok(OpenAiSession {
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

    async fn finish_audio(&mut self) -> Result<(), RecognizerError> {
        self.finished = true;
        self.released_at = Some(Instant::now());
        self.finished_signal.notify_one();
        Ok(())
    }

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
                        let message = match error {
                        crate::secondary::SecondaryError::Timeout => "The speech server took too long to transcribe this capture. Nothing was delivered.".to_owned(),
                        crate::secondary::SecondaryError::Unreachable => "Thalvi could not reach the speech server. Nothing was delivered.".to_owned(),
                        crate::secondary::SecondaryError::Status(code) => format!("The speech server returned HTTP {code}. Nothing was delivered."),
                        crate::secondary::SecondaryError::BadResponse => "The speech server returned an unreadable transcription. Nothing was delivered.".to_owned(),
                        };
                        let _ = sender.send(Err(RecognizerError::said(format!(
                            "{message} Check the address and API key under Settings → Speech."
                        ))));
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

    async fn cancel(self) {
        if let Some(task) = self.task {
            task.abort();
        }
    }
    async fn close(self) {
        if let Some(task) = self.task {
            task.abort();
        }
    }
}

//! Owns the desktop's transcription-only socket; captured speech never calls the chat model.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{FutureExt, SinkExt};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use crate::recognizer::{Recognizer, RecognizerEvent, RecognizerFactory};
use crate::recorder::{RecordingConfig, start_recording};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_AUDIO_BYTES: usize = 48_000;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Cancel,
}

/// Relays 24 kHz mono s16le PCM to the recognizer and returns partial and final text.
pub async fn handle_transcription<F: RecognizerFactory>(
    mut socket: WebSocket,
    factory: F,
    config: RecordingConfig,
    id: String,
) {
    let recording = start_recording(config, id);
    let started = Instant::now();
    let connection = tokio::select! {
        result = tokio::time::timeout(IO_TIMEOUT, factory.connect()) => match result {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(_) => Err("Speech recognition took too long to connect.".to_string()),
        },
        _ = socket.recv() => Err("Capture closed before speech recognition was ready.".to_string()),
    };
    let connected_ms = elapsed_ms(started);
    recording.recorder.emit(
        "transcription_connect",
        json!({
            "model": recording.recorder.stt_model(), "total_ms": connected_ms,
            "outcome": if connection.is_ok() { "ok" } else { "failed" },
        }),
    );
    let (recognizer, result) = match connection {
        Ok(mut recognizer) => {
            let result = relay(&mut socket, &mut recognizer).await;
            (Some(recognizer), result)
        }
        Err(error) => (None, Err(error)),
    };
    if let Some(mut recognizer) = recognizer {
        let _ = recognizer.close().now_or_never();
    }
    if let Err(message) = &result {
        let _ = send(&mut socket, json!({"type":"error", "message":message})).await;
    }
    recording.recorder.emit(
        "transcription",
        json!({
            "model": recording.recorder.stt_model(), "total_ms": elapsed_ms(started),
            "outcome": if result.is_ok() { "ok" } else { "failed" },
            "error": result.err(),
        }),
    );
    let _ = tokio::time::timeout(IO_TIMEOUT, socket.close()).await;
    recording.finish(elapsed_ms(started)).await;
}

async fn send(socket: &mut WebSocket, value: serde_json::Value) -> Result<(), String> {
    tokio::time::timeout(
        IO_TIMEOUT,
        socket.send(Message::Text(value.to_string().into())),
    )
    .await
    .map_err(|_| "The desktop stopped receiving capture updates.".to_string())?
    .map_err(|_| "The capture connection closed.".to_string())
}

async fn relay<R: Recognizer>(socket: &mut WebSocket, recognizer: &mut R) -> Result<(), String> {
    send(socket, json!({"type":"ready"})).await?;
    let mut buffered = None;
    loop {
        let message = async {
            match buffered.take() {
                Some(audio) => Some(Ok(Message::Binary(audio))),
                None => socket.recv().await,
            }
        };
        tokio::select! {
            message = message => match message {
                Some(Ok(Message::Binary(audio))) => {
                    if audio.is_empty() || audio.len() > MAX_AUDIO_BYTES || audio.len() % 2 != 0 {
                        return Err("Send at most one second of 24 kHz mono s16le audio at a time.".to_string());
                    }
                    let write = tokio::time::timeout(IO_TIMEOUT, recognizer.send_audio(&audio));
                    tokio::pin!(write);
                    loop {
                        tokio::select! {
                            biased;
                            result = &mut write => {
                                result.map_err(|_| "Speech recognition stopped accepting audio.".to_string())?
                                    .map_err(|error| error.to_string())?;
                                break;
                            }
                            next = socket.recv() => match next {
                                Some(Ok(Message::Binary(next))) => {
                                    if buffered.replace(next).is_some() {
                                        return Err("Speech recognition stopped accepting audio. Capture stopped without delivery.".to_string());
                                    }
                                }
                                Some(Ok(Message::Text(text))) => {
                                    return serde_json::from_str::<Command>(&text)
                                        .map(|Command::Cancel| ())
                                        .map_err(|_| "Capture accepts audio or a cancel message.".to_string());
                                }
                                Some(Ok(Message::Close(_))) | None => return Ok(()),
                                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                                Some(Err(_)) => return Err("The capture connection closed.".to_string()),
                            }
                        }
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str::<Command>(&text)
                        .map(|Command::Cancel| ())
                        .map_err(|_| "Capture accepts audio or a cancel message.".to_string());
                }
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                Some(Err(_)) => return Err("The capture connection closed.".to_string()),
            },
            event = recognizer.next_event() => match event {
                Some(Ok(RecognizerEvent::Partial {text})) => send(socket,json!({"type":"partial","text":text})).await?,
                Some(Ok(RecognizerEvent::SpeechComplete {turn_id,text})) => send(socket,json!({"type":"final","turn_id":turn_id,"text":text})).await?,
                Some(Ok(RecognizerEvent::SpeechStart {..} | RecognizerEvent::SpeechEnd {..})) => {},
                Some(Ok(RecognizerEvent::Error {message})) => return Err(message),
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("Speech recognition closed. Your capture was not delivered.".to_string()),
            }
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::{Router, extract::ws::WebSocketUpgrade, routing::get};
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::Notify;
    use tokio_tungstenite::tungstenite::Message;

    use super::handle_transcription;
    use crate::recognizer::{Recognizer, RecognizerError, RecognizerEvent, RecognizerFactory};
    use crate::recorder::RecordingConfig;

    #[derive(Clone)]
    struct Factory {
        entered: Arc<Notify>,
        dropped: Arc<Notify>,
    }

    struct Stalled(Factory);

    impl Drop for Stalled {
        fn drop(&mut self) {
            self.0.dropped.notify_one();
        }
    }

    impl RecognizerFactory for Factory {
        type Rec = Stalled;

        fn connect(
            &self,
        ) -> impl std::future::Future<Output = Result<Stalled, RecognizerError>> + Send {
            std::future::ready(Ok(Stalled(self.clone())))
        }
    }

    impl Recognizer for Stalled {
        async fn send_audio(&mut self, _: &[u8]) -> Result<(), RecognizerError> {
            self.0.entered.notify_one();
            std::future::pending().await
        }

        async fn next_event(&mut self) -> Option<Result<RecognizerEvent, RecognizerError>> {
            std::future::pending().await
        }

        async fn close(&mut self) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn cancel_and_disconnect_drop_a_stalled_recognizer() {
        for cancel in [true, false] {
            let factory = Factory {
                entered: Arc::new(Notify::new()),
                dropped: Arc::new(Notify::new()),
            };
            let handler_factory = factory.clone();
            let app = Router::new().route(
                "/transcription",
                get(move |upgrade: WebSocketUpgrade| {
                    let factory = handler_factory.clone();
                    async move {
                        upgrade.on_upgrade(move |socket| {
                            handle_transcription(
                                socket,
                                factory,
                                RecordingConfig::disabled(),
                                "test".into(),
                            )
                        })
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let (mut client, _) =
                tokio_tungstenite::connect_async(format!("ws://{address}/transcription"))
                    .await
                    .unwrap();
            client.next().await.unwrap().unwrap();
            client
                .send(Message::Binary(vec![0, 0].into()))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), factory.entered.notified())
                .await
                .unwrap();
            if cancel {
                client
                    .send(Message::Text(r#"{"type":"cancel"}"#.into()))
                    .await
                    .unwrap();
            } else {
                drop(client);
            }
            tokio::time::timeout(Duration::from_secs(2), factory.dropped.notified())
                .await
                .unwrap();
            server.abort();
        }
    }
}

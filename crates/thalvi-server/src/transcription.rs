//! Owns the desktop's transcription-only socket; captured speech never calls the chat model.

use std::{collections::VecDeque, time::Duration};

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use futures_util::{FutureExt, SinkExt};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use crate::recognizer::{Recognizer, RecognizerEvent, RecognizerFactory};
use crate::recorder::{RecordingConfig, start_recording};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_AUDIO_BYTES: usize = 48_000;
const MAX_BUFFERED_AUDIO_BYTES: usize = MAX_AUDIO_BYTES * 8;

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

fn validate_audio(audio: &[u8]) -> Result<(), String> {
    if audio.is_empty() || audio.len() > MAX_AUDIO_BYTES || !audio.len().is_multiple_of(2) {
        return Err("Send at most one second of 24 kHz mono s16le audio at a time.".to_string());
    }
    Ok(())
}

async fn relay<R: Recognizer>(socket: &mut WebSocket, recognizer: &mut R) -> Result<(), String> {
    send(socket, json!({"type":"ready"})).await?;
    let mut buffered: VecDeque<Bytes> = VecDeque::new();
    let mut buffered_bytes = 0;
    loop {
        let message = async {
            if let Some(audio) = buffered.pop_front() {
                buffered_bytes -= audio.len();
                Ok(Some(Ok(Message::Binary(audio))))
            } else {
                let message = socket.recv().await;
                if let Some(Ok(Message::Binary(audio))) = &message {
                    validate_audio(audio)?;
                }
                Ok::<_, String>(message)
            }
        };
        tokio::select! {
            message = message => match message? {
                Some(Ok(Message::Binary(audio))) => {
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
                                    validate_audio(&next)?;
                                    if buffered_bytes + next.len() > MAX_BUFFERED_AUDIO_BYTES {
                                        return Err("Speech recognition stopped accepting audio. Capture stopped without delivery.".to_string());
                                    }
                                    buffered_bytes += next.len();
                                    buffered.push_back(next);
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
                Some(Ok(RecognizerEvent::SpeechStart {turn_id, audio_processed_ms})) => send(socket,json!({"type":"speech_start","turn_id":turn_id,"audio_processed_ms":audio_processed_ms})).await?,
                Some(Ok(RecognizerEvent::SpeechEnd {turn_id, audio_processed_ms})) => send(socket,json!({"type":"speech_end","turn_id":turn_id,"audio_processed_ms":audio_processed_ms})).await?,
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

    #[derive(Clone)]
    struct Buffered {
        release: Arc<Notify>,
        delivered: Arc<Notify>,
        received: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
    }

    impl RecognizerFactory for Buffered {
        type Rec = Self;

        fn connect(
            &self,
        ) -> impl std::future::Future<Output = Result<Self, RecognizerError>> + Send {
            std::future::ready(Ok(self.clone()))
        }
    }

    impl Recognizer for Buffered {
        async fn send_audio(&mut self, audio: &[u8]) -> Result<(), RecognizerError> {
            if self.received.lock().await.len().is_multiple_of(60) {
                self.release.notified().await;
            }
            self.received.lock().await.push(audio.to_vec());
            self.delivered.notify_one();
            Ok(())
        }

        async fn next_event(&mut self) -> Option<Result<RecognizerEvent, RecognizerError>> {
            std::future::pending().await
        }

        async fn close(&mut self) {}
    }

    #[tokio::test]
    async fn startup_audio_burst_is_buffered_in_order_and_bounded() {
        for frames in [60_u8, 102] {
            let factory = Buffered {
                release: Arc::new(Notify::new()),
                delivered: Arc::new(Notify::new()),
                received: Arc::default(),
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
            let bursts = if frames == 60 { 2 } else { 1 };
            for burst in 0..bursts {
                let end = (burst + 1) * frames;
                for index in burst * frames..end {
                    client
                        .send(Message::Binary([index, 0].repeat(1920).into()))
                        .await
                        .unwrap();
                }
                client
                    .send(Message::Ping(vec![burst].into()))
                    .await
                    .unwrap();
                let response = tokio::time::timeout(Duration::from_secs(2), client.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                if frames == 60 {
                    assert!(matches!(response, Message::Pong(_)));
                    factory.release.notify_one();
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while factory.received.lock().await.len() < usize::from(end) {
                            factory.delivered.notified().await;
                        }
                    })
                    .await
                    .unwrap();
                    assert_eq!(
                        *factory.received.lock().await,
                        (0..end)
                            .map(|index| [index, 0].repeat(1920))
                            .collect::<Vec<_>>()
                    );
                } else {
                    let Message::Text(error) = response else {
                        panic!("the oversized backlog must return an error");
                    };
                    assert!(error.contains("Speech recognition stopped accepting audio"));
                    assert!(factory.received.lock().await.is_empty());
                }
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn cancel_disconnect_and_invalid_audio_drop_a_stalled_recognizer() {
        for command in [
            Some(Message::Text(r#"{"type":"cancel"}"#.into())),
            None,
            Some(Message::Binary(Vec::new().into())),
            Some(Message::Binary(vec![0].into())),
            Some(Message::Binary(vec![0; 48_002].into())),
            Some(Message::Binary(vec![0; 400_000].into())),
        ] {
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
            if let Some(command) = command {
                let invalid_audio = matches!(command, Message::Binary(_));
                client.send(command).await.unwrap();
                if invalid_audio {
                    let response = tokio::time::timeout(Duration::from_secs(2), client.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                    let Message::Text(error) = response else {
                        panic!("invalid audio must return an error");
                    };
                    assert!(error.contains("Send at most one second of 24 kHz mono s16le audio"));
                }
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

//! Owns the HTTP router: the web client at `/`, the live socket at `/session`, `/sessions`, and the voice picker's `/voices` and `/voice/preview`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tower_http::services::ServeDir;
use tracing::Instrument;

use crate::api;
use crate::config::{Config, TtsApi};
use crate::learning::LearningStore;
use crate::learning_api;
use crate::model::OpenAiChatModel;
use crate::recognizer::MetaRecognizerFactory;
use crate::recorder::{RecordingConfig, start_recording, wav_bytes};
use crate::retirement::{RETIREMENT_SYSTEM_PROMPT, spawn_retirement};
use crate::session::{VoiceChoices, handle_session};
use crate::store::SessionStore;
use crate::title::{TITLE_SYSTEM_PROMPT, Titler};
use crate::tts::{OpenAiTts, Qwen3Tts, Tts};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

const PREVIEW_PHRASE: &str = "Hello, this is how I sound.";

/// Shared state for the session endpoint.
#[derive(Clone)]
pub struct AppState<T> {
    factory: MetaRecognizerFactory,
    model: Arc<OpenAiChatModel>,
    tts: Arc<T>,
    voices: Arc<[String]>,
    default_voice: String,
    store: Arc<SessionStore>,
    titler: Option<Arc<Titler<OpenAiChatModel>>>,
    recording: RecordingConfig,
    ack_enabled: bool,
    audio_input: bool,
}

/// Builds the router from validated configuration.
///
/// # Errors
///
/// Returns a plain message when the saved conversations cannot be opened.
pub fn router(config: &Config) -> Result<Router, String> {
    let store = Arc::new(SessionStore::open(&config.data_dir.join("sessions"))?);
    let learnings = Arc::new(LearningStore::open(
        &config.data_dir.join("learnings.json"),
    )?);
    let base_url = config.tts_base_url.clone();
    let voice = config.tts_voice.clone();
    Ok(match &config.tts_api {
        TtsApi::OpenAi { model, api_key } => with_tts(
            config,
            store,
            learnings,
            OpenAiTts::new(base_url, model.clone(), voice, api_key.clone()),
            model.clone(),
        ),
        TtsApi::Qwen3 => with_tts(
            config,
            store,
            learnings,
            Qwen3Tts::new(base_url, voice),
            "qwen3-tts".to_string(),
        ),
    })
}

fn with_tts<T: Tts + Clone + 'static>(
    config: &Config,
    store: Arc<SessionStore>,
    learnings: Arc<LearningStore>,
    tts: T,
    tts_model: String,
) -> Router {
    let recording = RecordingConfig {
        kinds: config.record,
        data_dir: config.data_dir.clone(),
        stt_model: config.stt_model.clone(),
        llm_model: config.llm_model.clone(),
        reasoning_effort: config.llm_reasoning_effort.clone(),
        tts_model,
        tts_voice: config.tts_voice.clone(),
    };
    if let Some(days) = config.retire_days {
        let model = OpenAiChatModel::new(
            config.llm_base_url.clone(),
            config.llm_model.clone(),
            config.llm_api_key.clone(),
            config.llm_reasoning_effort.clone(),
        )
        .with_system_prompt(RETIREMENT_SYSTEM_PROMPT);
        let recorder = start_recording(recording.clone(), "retirement".to_string()).recorder;
        spawn_retirement(
            Arc::new(model),
            Arc::clone(&store),
            Arc::clone(&learnings),
            days,
            recorder,
        );
    }
    let state = AppState {
        factory: MetaRecognizerFactory::new(
            config.stt_url.clone(),
            config.stt_model.clone(),
            config.stt_api_key.clone(),
        ),
        model: Arc::new(OpenAiChatModel::new(
            config.llm_base_url.clone(),
            config.llm_model.clone(),
            config.llm_api_key.clone(),
            config.llm_reasoning_effort.clone(),
        )),
        tts: Arc::new(tts),
        voices: config.tts_voices.clone().into(),
        default_voice: config.tts_voice.clone(),
        store: Arc::clone(&store),
        titler: config.bg_llm.as_ref().map(|bg| {
            Arc::new(Titler {
                model: OpenAiChatModel::new(
                    bg.base_url.clone(),
                    bg.model.clone(),
                    bg.api_key.clone(),
                    None,
                )
                .with_system_prompt(TITLE_SYSTEM_PROMPT),
                name: bg.model.clone(),
            })
        }),
        recording,
        ack_enabled: config.ack_enabled,
        audio_input: config.llm_audio_input,
    };
    Router::new()
        .route("/session", get(session_upgrade::<T>))
        .route("/voices", get(list_voices::<T>))
        .route("/voice/preview", post(preview_voice::<T>))
        .with_state(state)
        .merge(api::routes().with_state(store))
        .merge(learning_api::routes().with_state(learnings))
        .fallback_service(ServeDir::new(&config.web_dir))
}

async fn session_upgrade<T: Tts + Clone + 'static>(
    State(state): State<AppState<T>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let resume = query.get("resume").cloned();
    upgrade.on_upgrade(move |socket| {
        handle_session(
            socket,
            state.factory,
            state.model,
            state.tts,
            state.store,
            resume,
            state.titler,
            state.recording,
            state.ack_enabled,
            state.audio_input,
            VoiceChoices {
                default: state.default_voice,
                offered: state.voices,
            },
        )
        .instrument(tracing::info_span!("session", id))
    })
}

async fn list_voices<T>(State(state): State<AppState<T>>) -> Response {
    axum::Json(json!({"voices": &*state.voices, "default": state.default_voice})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewRequest {
    voice: String,
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

struct Preview {
    pcm: Vec<u8>,
    first_audio_ms: Option<u64>,
}

async fn synthesize_preview<T: Tts>(
    tts: &T,
    voice: &str,
    started: std::time::Instant,
) -> Result<Preview, String> {
    let mut stream = tts
        .synthesize_with_voice(PREVIEW_PHRASE, voice)
        .await
        .map_err(|error| error.to_string())?;
    let mut pcm = Vec::new();
    let mut first_audio_ms = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if first_audio_ms.is_none() && !chunk.is_empty() {
            first_audio_ms = Some(elapsed_ms(started));
        }
        pcm.extend_from_slice(&chunk);
    }
    if pcm.is_empty() {
        return Err("speech synthesis returned no audio".to_string());
    }
    Ok(Preview {
        pcm,
        first_audio_ms,
    })
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn preview_voice<T: Tts + 'static>(
    State(state): State<AppState<T>>,
    body: Bytes,
) -> Response {
    let Ok(PreviewRequest { voice }) = serde_json::from_slice(&body) else {
        return plain_error(
            StatusCode::BAD_REQUEST,
            "Send a JSON body like {\"voice\": \"...\"}.",
        );
    };
    if !state.voices.contains(&voice) {
        return plain_error(StatusCode::BAD_REQUEST, "That voice isn't offered.");
    }
    let started = std::time::Instant::now();
    let result = synthesize_preview(&*state.tts, &voice, started).await;
    let total_ms = elapsed_ms(started);

    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let recording = start_recording(
        RecordingConfig {
            tts_voice: voice.clone(),
            ..state.recording.clone()
        },
        format!("preview-{id}"),
    );
    let (outcome, error, first_audio_ms, audio_ms) = match &result {
        Ok(preview) => (
            "ok",
            None,
            preview.first_audio_ms,
            Some(u64::try_from(preview.pcm.len()).unwrap_or(u64::MAX) * 1000 / 48_000),
        ),
        Err(error) => ("error", Some(error.as_str()), None, None),
    };
    recording.recorder.emit(
        "voice_preview",
        json!({
            "tts_model": recording.recorder.tts_model(),
            "voice": voice,
            "first_audio_ms": first_audio_ms,
            "total_ms": total_ms,
            "audio_ms": audio_ms,
            "outcome": outcome,
            "error": error,
        }),
    );
    tokio::spawn(recording.finish(total_ms));

    match result {
        Ok(preview) => (
            [(header::CONTENT_TYPE, "audio/wav")],
            wav_bytes(&preview.pcm),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%voice, %error, "voice preview failed");
            plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't play that voice. Speech isn't responding.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use serde_json::{Value, json};

    use crate::config::Config;

    use super::router;

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    /// A speech server that records each requested voice and answers with 4 bytes of PCM.
    async fn fake_speech(voices: Arc<Mutex<Vec<String>>>) -> String {
        let app = axum::Router::new()
            .route(
                "/v1/audio/speech",
                post(
                    |State(voices): State<Arc<Mutex<Vec<String>>>>,
                     axum::Json(body): axum::Json<Value>| async move {
                        voices
                            .lock()
                            .unwrap_or_else(|e| panic!("{e}"))
                            .push(body["voice"].as_str().unwrap_or_default().to_string());
                        vec![1_u8, 2, 3, 4]
                    },
                ),
            )
            .with_state(voices);
        serve(app).await
    }

    fn config(speech: &str, data_dir: &Path, extra: &[(&str, &str)]) -> Config {
        let mut vars: HashMap<&str, String> = [
            ("THALVI_ADDR", "127.0.0.1:0".to_string()),
            ("THALVI_WEB_DIR", "web/dist".to_string()),
            ("LLM_BASE_URL", "http://127.0.0.1:1/v1".to_string()),
            ("LLM_MODEL", "m".to_string()),
            ("LLM_API_KEY", String::new()),
            ("STT_URL", "ws://127.0.0.1:1".to_string()),
            ("STT_MODEL", "s".to_string()),
            ("STT_API_KEY", String::new()),
            ("TTS_API", "openai".to_string()),
            ("TTS_BASE_URL", speech.to_string()),
            ("TTS_MODEL", "kokoro".to_string()),
            ("TTS_VOICE", "af_heart".to_string()),
            ("TTS_VOICES", "sohee".to_string()),
            ("TTS_API_KEY", String::new()),
            ("THALVI_DATA_DIR", data_dir.display().to_string()),
        ]
        .into_iter()
        .collect();
        vars.extend(extra.iter().map(|(k, v)| (*k, (*v).to_string())));
        Config::from_lookup(|name| vars.get(name).cloned()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("thalvi-voices-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn lists_the_configured_voices_and_previews_only_those() {
        let requested = Arc::new(Mutex::new(Vec::new()));
        let speech = fake_speech(Arc::clone(&requested)).await;
        let dir = scratch("api");
        let app =
            router(&config(&format!("{speech}/v1"), &dir, &[])).unwrap_or_else(|e| panic!("{e}"));
        let base = serve(app).await;
        let client = reqwest::Client::new();

        let listed: Value = client
            .get(format!("{base}/voices"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            listed,
            json!({"voices": ["af_heart", "sohee"], "default": "af_heart"})
        );

        let preview = |body: &'static str| {
            client
                .post(format!("{base}/voice/preview"))
                .header("content-type", "application/json")
                .body(body)
                .send()
        };
        let ok = preview(r#"{"voice":"sohee"}"#)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers()["content-type"], "audio/wav");
        let wav = ok.bytes().await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (&wav[0..4], &wav[44..]),
            (&b"RIFF"[..], &[1_u8, 2, 3, 4][..])
        );

        for body in [
            r#"{"voice":"nobody"}"#,
            r#"{"voice":""}"#,
            "{}",
            "not json",
            r#"{"voice":"sohee","x":1}"#,
        ] {
            let bad = preview(body).await.unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "body={body}");
            let error: Value = bad.json().await.unwrap_or_else(|e| panic!("{e}"));
            assert!(
                error["error"].as_str().is_some_and(|m| !m.is_empty()),
                "body={body}"
            );
        }
        assert_eq!(
            *requested.lock().unwrap_or_else(|e| panic!("{e}")),
            ["sohee"],
            "only the accepted preview reaches speech"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preview_fails_in_plain_words_when_speech_is_unreachable() {
        let dir = scratch("down");
        let app =
            router(&config("http://127.0.0.1:1/v1", &dir, &[])).unwrap_or_else(|e| panic!("{e}"));
        let base = serve(app).await;
        let response = reqwest::Client::new()
            .post(format!("{base}/voice/preview"))
            .header("content-type", "application/json")
            .body(r#"{"voice":"af_heart"}"#)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let error: Value = response.json().await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            error,
            json!({"error": "Couldn't play that voice. Speech isn't responding."})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preview_records_latency_and_outcome_with_the_requested_voice_when_metrics_are_on() {
        let speech = fake_speech(Arc::default()).await;
        let dir = scratch("metrics");
        let app = router(&config(
            &format!("{speech}/v1"),
            &dir,
            &[("THALVI_RECORD", "metrics")],
        ))
        .unwrap_or_else(|e| panic!("{e}"));
        let base = serve(app).await;
        let status = reqwest::Client::new()
            .post(format!("{base}/voice/preview"))
            .header("content-type", "application/json")
            .body(r#"{"voice":"sohee"}"#)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .status();
        assert_eq!(status, StatusCode::OK);

        let line = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(line) = recorded_preview(&dir) {
                    return line;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no voice_preview record was written"));
        assert_eq!(line["voice"], "sohee");
        assert_eq!(line["tts_model"], "kokoro");
        assert_eq!(line["outcome"], "ok");
        assert!(
            line["total_ms"].is_u64() && line["first_audio_ms"].is_u64(),
            "{line}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn recorded_preview(dir: &Path) -> Option<Value> {
        let day = std::fs::read_dir(dir.join("recordings"))
            .ok()?
            .next()?
            .ok()?
            .path();
        std::fs::read_dir(day)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|file| std::fs::read_to_string(file.path()).ok())
            .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
            .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
            .find(|line| line["event"] == "voice_preview")
    }
}

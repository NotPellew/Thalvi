//! Owns the HTTP router: the web client at `/`, the live socket at `/session`, and `/sessions`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::services::ServeDir;
use tracing::Instrument;

use crate::api;
use crate::config::{Config, TtsApi};
use crate::model::OpenAiChatModel;
use crate::recognizer::MetaRecognizerFactory;
use crate::recorder::RecordingConfig;
use crate::session::handle_session;
use crate::store::SessionStore;
use crate::title::{TITLE_SYSTEM_PROMPT, Titler};
use crate::tts::{OpenAiTts, Qwen3Tts, Tts};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Shared state for the session endpoint.
#[derive(Clone)]
pub struct AppState<T> {
    factory: MetaRecognizerFactory,
    model: Arc<OpenAiChatModel>,
    tts: Arc<T>,
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
    let base_url = config.tts_base_url.clone();
    let voice = config.tts_voice.clone();
    Ok(match &config.tts_api {
        TtsApi::OpenAi { model, api_key } => with_tts(
            config,
            store,
            OpenAiTts::new(base_url, model.clone(), voice, api_key.clone()),
            model.clone(),
        ),
        TtsApi::Qwen3 => with_tts(
            config,
            store,
            Qwen3Tts::new(base_url, voice),
            "qwen3-tts".to_string(),
        ),
    })
}

fn with_tts<T: Tts + Clone + 'static>(
    config: &Config,
    store: Arc<SessionStore>,
    tts: T,
    tts_model: String,
) -> Router {
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
        recording: RecordingConfig {
            kinds: config.record,
            data_dir: config.data_dir.clone(),
            stt_model: config.stt_model.clone(),
            llm_model: config.llm_model.clone(),
            reasoning_effort: config.llm_reasoning_effort.clone(),
            tts_model,
            tts_voice: config.tts_voice.clone(),
        },
        ack_enabled: config.ack_enabled,
        audio_input: config.llm_audio_input,
    };
    Router::new()
        .route("/session", get(session_upgrade::<T>))
        .with_state(state)
        .merge(api::routes().with_state(store))
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
        )
        .instrument(tracing::info_span!("session", id))
    })
}

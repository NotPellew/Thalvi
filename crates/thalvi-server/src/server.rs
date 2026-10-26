//! Owns the HTTP router: the web client at `/` and sessions at `/session`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::services::ServeDir;
use tracing::Instrument;

use crate::config::Config;
use crate::model::OpenAiChatModel;
use crate::recognizer::MetaRecognizerFactory;
use crate::session::handle_session;
use crate::store::SessionStore;
use crate::tts::OpenAiTts;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Shared state for the session endpoint.
#[derive(Clone)]
pub struct AppState {
    factory: MetaRecognizerFactory,
    model: Arc<OpenAiChatModel>,
    tts: Arc<OpenAiTts>,
    store: Arc<SessionStore>,
}

/// Builds the router from validated configuration.
pub fn router(config: &Config) -> Router {
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
        tts: Arc::new(OpenAiTts::new(
            config.tts_base_url.clone(),
            config.tts_model.clone(),
            config.tts_voice.clone(),
            config.tts_api_key.clone(),
        )),
        store: Arc::new(SessionStore::new()),
    };
    Router::new()
        .route("/session", get(session_upgrade))
        .fallback_service(ServeDir::new(&config.web_dir))
        .with_state(state)
}

async fn session_upgrade(
    State(state): State<AppState>,
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
        )
        .instrument(tracing::info_span!("session", id))
    })
}

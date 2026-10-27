//! Owns the HTTP endpoints that list, rename and delete saved conversations.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use serde::Deserialize;
use serde_json::json;

use crate::store::{MAX_TITLE_CHARS, SessionStore};

/// Routes for `/sessions`; anyone who can reach the server can use them.
pub fn routes() -> Router<Arc<SessionStore>> {
    Router::new()
        .route("/sessions", get(list))
        .route("/sessions/{id}", patch(rename).delete(delete))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rename {
    title: String,
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

fn not_found() -> Response {
    plain_error(StatusCode::NOT_FOUND, "That conversation doesn't exist.")
}

async fn list(State(store): State<Arc<SessionStore>>) -> Response {
    axum::Json(json!({"sessions": store.list()})).into_response()
}

async fn rename(
    State(store): State<Arc<SessionStore>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(Rename { title }) = serde_json::from_slice(&body) else {
        return plain_error(
            StatusCode::BAD_REQUEST,
            "Send a JSON body like {\"title\": \"...\"}.",
        );
    };
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return plain_error(StatusCode::BAD_REQUEST, "The title can't be empty.");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return plain_error(
            StatusCode::BAD_REQUEST,
            &format!("The title is too long (at most {MAX_TITLE_CHARS} characters)."),
        );
    }
    if store.rename(&id, &title) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

async fn delete(State(store): State<Arc<SessionStore>>, Path(id): Path<String>) -> Response {
    if store.delete(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

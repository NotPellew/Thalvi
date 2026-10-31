//! Owns the HTTP endpoints that list and edit learnings.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use serde::Deserialize;
use serde_json::json;

use crate::learning::{LearningError, LearningStore};

/// Routes for `/learnings`; anyone who can reach the server can use them.
pub fn routes() -> Router<Arc<LearningStore>> {
    Router::new()
        .route("/learnings", get(list))
        .route("/learnings/{id}", patch(edit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    text: String,
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

async fn list(State(store): State<Arc<LearningStore>>) -> Response {
    axum::Json(json!({"learnings": store.list()})).into_response()
}

async fn edit(
    State(store): State<Arc<LearningStore>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(Edit { text }) = serde_json::from_slice(&body) else {
        return plain_error(
            StatusCode::BAD_REQUEST,
            "Send a JSON body like {\"text\": \"...\"}.",
        );
    };
    let result = match tokio::task::spawn_blocking(move || store.edit(&id, &text)).await {
        Ok(result) => result,
        Err(error) => Err(LearningError::Disk(error.to_string())),
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error @ (LearningError::Blank | LearningError::TooLong)) => {
            plain_error(StatusCode::BAD_REQUEST, &error.to_string())
        }
        Err(error @ LearningError::NotFound) => {
            plain_error(StatusCode::NOT_FOUND, &error.to_string())
        }
        Err(error @ LearningError::Disk(_)) => {
            tracing::warn!(%error, "could not save learning");
            plain_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::routes;
    use crate::learning::LearningStore;

    async fn serve(store: Arc<LearningStore>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
        let app = routes().with_state(store);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn lists_rows_and_edits_with_every_failure_answered_in_plain_json() {
        let dir = std::env::temp_dir().join(format!("thalvi-learning-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(
            LearningStore::open(&dir.join("learnings.json")).unwrap_or_else(|e| panic!("{e}")),
        );
        let added = store
            .append("s1", &["Likes tea".to_string()])
            .unwrap_or_else(|e| panic!("{e}"));
        let base = serve(Arc::clone(&store)).await;
        let client = reqwest::Client::new();
        let id = &added[0].id;
        let patch = |path: String, body: String| {
            client
                .patch(format!("{base}{path}"))
                .header("content-type", "application/json")
                .body(body)
                .send()
        };

        let listed: Value = client
            .get(format!("{base}/learnings"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(listed["learnings"][0]["id"], json!(id));
        assert_eq!(listed["learnings"][0]["text"], "Likes tea");
        assert_eq!(listed["learnings"][0]["source_session"], "s1");
        assert!(listed["learnings"][0]["updated_ms"].is_u64());

        for (body, status, message) in [
            (
                "not json",
                400,
                "Send a JSON body like {\"text\": \"...\"}.",
            ),
            ("{\"text\":\"  \"}", 400, "The text can't be empty."),
            (
                &json!({"text": "x".repeat(1001)}).to_string(),
                400,
                "The text is too long (at most 1000 characters).",
            ),
        ] {
            let response = patch(format!("/learnings/{id}"), body.to_string())
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(response.status().as_u16(), status, "{body}");
            let error: Value = response.json().await.unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(error["error"], message);
        }
        let unknown = patch(
            "/learnings/nope".to_string(),
            "{\"text\":\"Fine\"}".to_string(),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(unknown.status().as_u16(), 404);
        assert_eq!(store.list(), added);

        let ok = patch(
            format!("/learnings/{id}"),
            json!({"text": "x".repeat(1000)}).to_string(),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ok.status().as_u16(), 204);
        assert_eq!(store.list()[0].text, "x".repeat(1000));

        std::fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&dir, "in the way").unwrap_or_else(|e| panic!("{e}"));
        let failed = patch(format!("/learnings/{id}"), "{\"text\":\"New\"}".to_string())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(failed.status().as_u16(), 500);
        let error: Value = failed.json().await.unwrap_or_else(|e| panic!("{e}"));
        assert!(
            error["error"]
                .as_str()
                .is_some_and(|m| m.starts_with("Could not save the learning"))
        );
        assert_eq!(store.list()[0].text, "x".repeat(1000));
        let _ = std::fs::remove_file(&dir);
    }
}

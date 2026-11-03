//! Owns the HTTP endpoints that list, read, rename and delete saved conversations.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::json;

use crate::store::{MAX_TITLE_CHARS, SessionStore};

/// Routes for `/sessions`; anyone who can reach the server can use them.
pub fn routes() -> Router<Arc<SessionStore>> {
    Router::new()
        .route("/sessions", get(list))
        .route("/sessions/{id}", get(detail).patch(rename).delete(delete))
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

async fn detail(State(store): State<Arc<SessionStore>>, Path(id): Path<String>) -> Response {
    store
        .snapshot(&id)
        .map_or_else(not_found, |snapshot| axum::Json(snapshot).into_response())
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
    match store.rename(&id, &title).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(error) => {
            tracing::warn!(%error, %id, "could not rename session");
            plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "That conversation couldn't be renamed.",
            )
        }
    }
}

async fn delete(State(store): State<Arc<SessionStore>>, Path(id): Path<String>) -> Response {
    match store.delete(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(error) => {
            tracing::warn!(%error, %id, "could not delete session");
            plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "That conversation couldn't be deleted.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::routes;
    use crate::model::ChatMessage;
    use crate::store::SessionStore;
    use serde_json::Value;
    use std::sync::Arc;

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("thalvi-api-test-{tag}-{}", std::process::id()))
    }

    fn break_disk(dir: &std::path::Path) {
        std::fs::remove_dir_all(dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir, b"not a directory").unwrap_or_else(|e| panic!("{e}"));
    }

    async fn seeded(tag: &str) -> (Arc<SessionStore>, String, std::path::PathBuf, String) {
        let dir = scratch(tag);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        let store = Arc::new(SessionStore::open(&dir).unwrap_or_else(|e| panic!("{e}")));
        let attachment = store.attach(None).await;
        let id = attachment.id.clone();
        store.save(&attachment, &[ChatMessage::user("hi".to_string())], 1);
        store.flush().await;
        store.release(attachment);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let addr = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
        let app = routes().with_state(Arc::clone(&store));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
        });
        (store, id, dir, format!("http://{addr}"))
    }

    #[tokio::test]
    async fn rename_returns_500_when_the_write_fails_and_404_for_unknown_ids() {
        let (store, id, dir, base) = seeded("rename-fails").await;
        break_disk(&dir);
        let client = reqwest::Client::new();
        let unknown = client
            .patch(format!("{base}/sessions/no-such-session"))
            .json(&serde_json::json!({"title": "New"}))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(unknown.status().as_u16(), 404);
        let response = client
            .patch(format!("{base}/sessions/{id}"))
            .json(&serde_json::json!({"title": "New"}))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&dir);
        assert_eq!(status, 500, "rename on a broken disk: {text}");
        let body: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            body["error"].as_str().is_some_and(|m| !m.is_empty()),
            "rename failure must use the plain error shape: {body}"
        );
        assert_eq!(
            store.list()[0].title,
            "hi",
            "a failed rename stays unchanged"
        );
    }

    #[tokio::test]
    async fn delete_returns_500_when_the_removal_fails_and_404_for_unknown_ids() {
        let (store, id, dir, base) = seeded("delete-fails").await;
        break_disk(&dir);
        let client = reqwest::Client::new();
        let unknown = client
            .delete(format!("{base}/sessions/no-such-session"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(unknown.status().as_u16(), 404);
        let response = client
            .delete(format!("{base}/sessions/{id}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&dir);
        assert_eq!(status, 500, "delete on a broken disk: {text}");
        let body: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            body["error"].as_str().is_some_and(|m| !m.is_empty()),
            "delete failure must use the plain error shape: {body}"
        );
        assert_eq!(
            store.list().len(),
            1,
            "a failed delete keeps the conversation listed"
        );
    }

    async fn get_json(client: &reqwest::Client, url: String) -> (u16, Value) {
        let response = client
            .get(url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let status = response.status().as_u16();
        (
            status,
            response.json().await.unwrap_or_else(|e| panic!("{e}")),
        )
    }

    #[tokio::test]
    async fn detail_returns_the_title_and_committed_turns_of_an_idle_conversation() {
        let (_store, id, dir, base) = seeded("detail-idle").await;
        let (status, body) =
            get_json(&reqwest::Client::new(), format!("{base}/sessions/{id}")).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(status, 200);
        assert_eq!(
            body,
            serde_json::json!({
                "id": id,
                "title": "hi",
                "turns": [{"role": "user", "text": "hi"}],
                "live": false,
            })
        );
    }

    #[tokio::test]
    async fn detail_is_not_found_for_unknown_and_deleted_conversations() {
        let (store, id, dir, base) = seeded("detail-missing").await;
        let client = reqwest::Client::new();
        let (unknown, _) = get_json(&client, format!("{base}/sessions/no-such-session")).await;
        assert_eq!(unknown, 404, "unknown id");
        assert_eq!(store.delete(&id).await, Ok(true));
        let (deleted, body) = get_json(&client, format!("{base}/sessions/{id}")).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(deleted, 404, "deleted id");
        assert_eq!(body["error"], "That conversation doesn't exist.");
    }

    #[tokio::test]
    async fn reading_a_live_conversation_leaves_its_owner_in_place() {
        let (store, id, dir, base) = seeded("detail-live").await;
        let mut owner = store.attach(Some(&id)).await;
        let client = reqwest::Client::new();
        for _ in 0..2 {
            let (status, body) = get_json(&client, format!("{base}/sessions/{id}")).await;
            assert_eq!(status, 200);
            assert_eq!(body["live"], true);
            assert_eq!(body["turns"].as_array().map(Vec::len), Some(1));
            let (listed, _) = get_json(&client, format!("{base}/sessions")).await;
            assert_eq!(listed, 200);
        }
        tokio::select! {
            biased;
            reason = owner.detached() => panic!("a read detached the owner: {reason:?}"),
            () = std::future::ready(()) => {}
        }
        assert_eq!(
            store.list().len(),
            1,
            "reads must not create a conversation"
        );
        store.release(owner);
        let (_, body) = get_json(&client, format!("{base}/sessions/{id}")).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(body["live"], false, "released conversations are idle again");
    }
}

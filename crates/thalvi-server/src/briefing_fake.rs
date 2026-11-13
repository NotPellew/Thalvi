//! Test support: a loopback briefing source that plays scripted replies, one script per endpoint.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri, header};
use axum::response::Response;
use bytes::Bytes;
use chrono::{SecondsFormat, TimeDelta, Utc};
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::{Notify, watch};

use crate::config::BriefingConfig;

pub(crate) const TOKEN: &str = "secret-token-1234";
pub(crate) const SEARCH: &str = "/v1/briefings/search";
pub(crate) const ITEMS: &str = "/v1/briefings/items/";

#[derive(Clone)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    pub(crate) hold: Option<Arc<Notify>>,
    pub(crate) redirect: bool,
    pub(crate) stall_body: bool,
    pub(crate) endless_body: bool,
}

impl Reply {
    pub(crate) fn raw(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            hold: None,
            redirect: false,
            stall_body: false,
            endless_body: false,
        }
    }

    pub(crate) fn ok(generated_at: &str, items: &[(&str, &str)]) -> Self {
        Self::raw(200, briefing_json(generated_at, items))
    }

    pub(crate) fn hours_ago(hours: i64, items: &[(&str, &str)]) -> Self {
        Self::ok(&hours_ago(hours), items)
    }

    pub(crate) fn held_by(mut self, gate: &Arc<Notify>) -> Self {
        self.hold = Some(Arc::clone(gate));
        self
    }
}

pub(crate) fn hours_ago(hours: i64) -> String {
    (Utc::now() - TimeDelta::hours(hours)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) fn briefing_json(generated_at: &str, items: &[(&str, &str)]) -> String {
    let items: Vec<_> = items
        .iter()
        .map(|(id, headline)| json!({"id": id, "headline": headline}))
        .collect();
    json!({"generated_at": generated_at, "items": items}).to_string()
}

pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) authorization: Option<String>,
    pub(crate) body: String,
}

pub(crate) struct SourceState {
    latest: Mutex<VecDeque<Reply>>,
    search: Mutex<VecDeque<Reply>>,
    items: Mutex<VecDeque<Reply>>,
    hits: watch::Sender<usize>,
    pub(crate) seen: Mutex<Vec<Seen>>,
}

/// A loopback briefing source. Each endpoint plays its script in order and repeats its last
/// reply; an endpoint with no script answers 404.
pub(crate) struct Source {
    pub(crate) config: BriefingConfig,
    hits: watch::Receiver<usize>,
    pub(crate) state: Arc<SourceState>,
}

impl Source {
    pub(crate) async fn start(latest: Vec<Reply>) -> Self {
        Self::start_with(latest, Vec::new(), Vec::new()).await
    }

    pub(crate) async fn start_with(
        latest: Vec<Reply>,
        search: Vec<Reply>,
        items: Vec<Reply>,
    ) -> Self {
        let (hits, hits_rx) = watch::channel(0);
        let state = Arc::new(SourceState {
            latest: Mutex::new(latest.into()),
            search: Mutex::new(search.into()),
            items: Mutex::new(items.into()),
            hits,
            seen: Mutex::new(Vec::new()),
        });
        let app = Router::new().fallback(serve).with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            config: BriefingConfig {
                base_url: format!("http://{addr}"),
                token: TOKEN.to_string(),
            },
            hits: hits_rx,
            state,
        }
    }

    pub(crate) async fn hits_reach(&mut self, count: usize) {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.hits.wait_for(|hits| *hits >= count),
        )
        .await
        .expect("the source was not asked often enough")
        .unwrap();
    }

    pub(crate) fn hits(&self) -> usize {
        *self.hits.borrow()
    }

    /// How many requests reached a path starting with `prefix`.
    pub(crate) fn requests_to(&self, prefix: &str) -> usize {
        let seen = self.state.seen.lock().unwrap();
        seen.iter()
            .filter(|request| request.uri.starts_with(prefix))
            .count()
    }
}

async fn serve(
    State(state): State<Arc<SourceState>>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: String,
) -> Response {
    let script = if uri.path().starts_with(ITEMS) {
        &state.items
    } else if uri.path() == SEARCH {
        &state.search
    } else {
        &state.latest
    };
    state.seen.lock().unwrap().push(Seen {
        method: method.to_string(),
        uri: uri.to_string(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        body,
    });
    state.hits.send_modify(|hits| *hits += 1);
    let reply = {
        let mut script = script.lock().unwrap();
        if script.len() > 1 {
            script.pop_front()
        } else {
            script.front().cloned()
        }
        .unwrap_or_else(|| Reply::raw(404, "{}"))
    };
    if let Some(gate) = &reply.hold {
        gate.notified().await;
    }
    let builder = Response::builder().status(reply.status);
    let builder = if reply.redirect {
        builder.header(header::LOCATION, "http://127.0.0.1:1/elsewhere")
    } else {
        builder
    };
    let body = if reply.endless_body {
        Body::from_stream(futures_util::stream::repeat_with(|| {
            Ok::<_, Infallible>(Bytes::from(vec![b' '; 8192]))
        }))
    } else if reply.stall_body {
        let first = Ok::<_, Infallible>(Bytes::from(reply.body));
        Body::from_stream(
            futures_util::stream::iter([first]).chain(futures_util::stream::pending()),
        )
    } else {
        Body::from(reply.body)
    };
    builder.body(body).unwrap()
}

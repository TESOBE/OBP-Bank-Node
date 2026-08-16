//! The app's own HTTP surface: static UI files plus a whitelisted JSON proxy
//! to the configured Bank Nodes.
//!
//! The proxy is deliberately dumb — it forwards a fixed set of south-side
//! paths verbatim and relays the node's status code and JSON body unchanged,
//! so every state the UI shows is readable from a node endpoint, never
//! inferred here. The whitelist keeps it from being an open relay.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tracing::warn;

use crate::NodeConfig;

const INDEX_HTML: &str = include_str!("static/index.html");
const PROMISES_HTML: &str = include_str!("static/promises.html");
const CREDITS_HTML: &str = include_str!("static/credits.html");
const SETTLEMENTS_HTML: &str = include_str!("static/settlements.html");
const APP_JS: &str = include_str!("static/app.js");
const STYLE_CSS: &str = include_str!("static/style.css");

/// Prefix every proxied path is mounted under on the node.
const NODE_API_PREFIX: &str = "/obp-bank-node/v5.1.0";

#[derive(Clone)]
pub struct AppState {
    nodes: Arc<HashMap<String, NodeConfig>>,
    /// Insertion order of the configured nodes, for a stable `/api/nodes` list.
    node_order: Arc<Vec<String>>,
    /// Per-instance form prefill (config `ui_defaults`), keyed by form-field
    /// name and served verbatim at `/api/ui-defaults`.
    ui_defaults: Arc<HashMap<String, String>>,
    http: reqwest::Client,
}

impl AppState {
    pub fn new(
        nodes: Vec<NodeConfig>,
        ui_defaults: HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let node_order: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
        let map: HashMap<String, NodeConfig> =
            nodes.into_iter().map(|n| (n.name.clone(), n)).collect();
        if map.len() != node_order.len() {
            anyhow::bail!("node names must be unique");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(AppState {
            nodes: Arc::new(map),
            node_order: Arc::new(node_order),
            ui_defaults: Arc::new(ui_defaults),
            http,
        })
    }
}

pub fn build_router(state: AppState, setup: Option<crate::setup::SetupState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/promises", get(|| async { Html(PROMISES_HTML) }))
        .route("/credits", get(|| async { Html(CREDITS_HTML) }))
        .route("/settlements", get(|| async { Html(SETTLEMENTS_HTML) }))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/nodes", get(list_nodes))
        .route("/api/ui-defaults", get(ui_defaults))
        .route("/api/nodes/:node/*path", get(proxy_get).post(proxy_post))
        .with_state(state)
        .merge(crate::setup::router(setup))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], APP_JS)
}

async fn style_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], STYLE_CSS)
}

/// `GET /api/nodes` — the configured node names, in config order. The UI
/// drives everything else off this list; base URLs and credentials stay
/// server-side.
async fn list_nodes(State(state): State<AppState>) -> Response {
    let body: Vec<_> = state
        .node_order
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    Json(body).into_response()
}

/// `GET /api/ui-defaults` — the instance's form-prefill map, verbatim from
/// config. The UI applies each entry to the matching input by field name.
async fn ui_defaults(State(state): State<AppState>) -> Response {
    Json(state.ui_defaults.as_ref().clone()).into_response()
}

/// South-side paths the proxy will forward. Everything else is refused, so a
/// compromised or curious browser can only reach what the UI actually needs.
fn allowed(method: &Method, path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    matches!(
        (method, segs.as_slice()),
        (&Method::GET, ["health"])
            | (&Method::GET, ["transaction-requests"])
            | (&Method::GET, ["transaction-requests", _])
            | (&Method::GET, ["settlements"])
            | (&Method::GET, ["settlements", _])
            | (&Method::GET, ["settlements", _, "corridor"])
            | (&Method::GET, ["evidence"])
            | (&Method::GET, ["evidence", _])
            | (&Method::POST, ["transaction-requests"])
            | (&Method::POST, ["settlements"])
    )
}

async fn proxy_get(
    State(state): State<AppState>,
    Path((node, path)): Path<(String, String)>,
) -> Response {
    proxy(state, node, path, Method::GET, None).await
}

async fn proxy_post(
    State(state): State<AppState>,
    Path((node, path)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    proxy(state, node, path, Method::POST, Some(body)).await
}

async fn proxy(
    state: AppState,
    node: String,
    path: String,
    method: Method,
    body: Option<Bytes>,
) -> Response {
    let Some(target) = state.nodes.get(&node) else {
        return error(
            StatusCode::NOT_FOUND,
            "OBP-BANK-NODE-APP-UNKNOWN-NODE",
            format!("no configured node named {node}"),
        );
    };
    if !allowed(&method, &path) {
        return error(
            StatusCode::NOT_FOUND,
            "OBP-BANK-NODE-APP-PATH-NOT-ALLOWED",
            format!("{method} {path} is not a proxied Bank Node endpoint"),
        );
    }

    let url = format!(
        "{}{}/{}",
        target.base_url.trim_end_matches('/'),
        NODE_API_PREFIX,
        path
    );
    let mut req = state.http.request(method.as_str().parse().unwrap(), &url);
    if let Some(token) = &target.bearer_token {
        req = req.bearer_auth(token);
    }
    if let Some(bytes) = body {
        req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes);
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.bytes().await {
                Ok(bytes) => {
                    (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
                }
                Err(e) => upstream_error(&node, &url, e),
            }
        }
        Err(e) => upstream_error(&node, &url, e),
    }
}

fn upstream_error(node: &str, url: &str, e: reqwest::Error) -> Response {
    warn!(node, url, error = %e, "proxied node request failed");
    error(
        StatusCode::BAD_GATEWAY,
        "OBP-BANK-NODE-APP-UPSTREAM-001",
        format!("node {node} unreachable: {e}"),
    )
}

fn error(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({ "error_code": code, "message": message })),
    )
        .into_response()
}

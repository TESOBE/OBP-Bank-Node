//! Router tests: static serving, the node list, and the proxy's pass-through,
//! whitelist, and error mapping — against a stub node bound on an ephemeral
//! port (the proxy goes over real HTTP via reqwest, so `oneshot` alone can't
//! stub the upstream).

use axum::body::{to_bytes, Body};
use axum::extract::Request as AxumRequest;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use tower::ServiceExt;

use crate::proxy::{build_router, AppState};
use crate::NodeConfig;

/// Serve `router` on an ephemeral localhost port, returning its base URL.
async fn spawn_stub(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// A stub Bank Node: health, an echo POST, and a 404-with-body endpoint.
fn stub_node() -> Router {
    Router::new()
        .route(
            "/obp-bank-node/v5.1.0/health",
            get(|headers: HeaderMap| async move {
                let auth = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                Json(serde_json::json!({ "status": "healthy", "auth_seen": auth }))
            }),
        )
        .route(
            "/obp-bank-node/v5.1.0/transaction-requests",
            post(|req: AxumRequest| async move {
                let bytes = to_bytes(req.into_body(), 1024 * 1024).await.unwrap();
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                (StatusCode::ACCEPTED, Json(serde_json::json!({ "echo": v }))).into_response()
            }),
        )
        .route(
            "/obp-bank-node/v5.1.0/settlements/:key",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error_code": "OBP-BANK-NODE-NOT-FOUND-001" })),
                )
            }),
        )
}

fn app_with(nodes: Vec<NodeConfig>) -> Router {
    build_router(AppState::new(nodes).unwrap())
}

fn node(name: &str, base_url: &str) -> NodeConfig {
    NodeConfig {
        name: name.into(),
        base_url: base_url.into(),
        bearer_token: None,
    }
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn get_req(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn index_and_assets_are_served() {
    let app = app_with(vec![node("a", "http://127.0.0.1:1")]);
    let resp = app.clone().oneshot(get_req("/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("text/html"));

    let resp = app.clone().oneshot(get_req("/app.js")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app.oneshot(get_req("/style.css")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_nodes_lists_configured_nodes_in_order() {
    let app = app_with(vec![
        node("node-a", "http://127.0.0.1:1"),
        node("node-b", "http://127.0.0.1:1"),
    ]);
    let resp = app.oneshot(get_req("/api/nodes")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v[0]["name"], "node-a");
    assert_eq!(v[1]["name"], "node-b");
}

#[tokio::test]
async fn duplicate_node_names_are_refused() {
    assert!(AppState::new(vec![
        node("same", "http://127.0.0.1:1"),
        node("same", "http://127.0.0.1:2"),
    ])
    .is_err());
}

#[tokio::test]
async fn proxy_passes_get_through_to_the_node() {
    let base = spawn_stub(stub_node()).await;
    let app = app_with(vec![node("node-a", &base)]);
    let resp = app
        .oneshot(get_req("/api/nodes/node-a/health"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["status"], "healthy");
    // No bearer configured — no Authorization header reaches the node.
    assert_eq!(v["auth_seen"], "");
}

#[tokio::test]
async fn proxy_attaches_configured_bearer_token() {
    let base = spawn_stub(stub_node()).await;
    let mut n = node("node-a", &base);
    n.bearer_token = Some("s3cret".into());
    let app = app_with(vec![n]);
    let resp = app
        .oneshot(get_req("/api/nodes/node-a/health"))
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["auth_seen"], "Bearer s3cret");
}

#[tokio::test]
async fn proxy_forwards_post_body_and_relays_status() {
    let base = spawn_stub(stub_node()).await;
    let app = app_with(vec![node("node-a", &base)]);
    let payload = serde_json::json!({ "value": { "currency": "KES", "amount": "500.00" } });
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/nodes/node-a/transaction-requests")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let v = body_json(resp).await;
    assert_eq!(v["echo"], payload);
}

#[tokio::test]
async fn proxy_relays_upstream_error_status_and_body() {
    let base = spawn_stub(stub_node()).await;
    let app = app_with(vec![node("node-a", &base)]);
    let resp = app
        .oneshot(get_req("/api/nodes/node-a/settlements/nope"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    assert_eq!(v["error_code"], "OBP-BANK-NODE-NOT-FOUND-001");
}

#[tokio::test]
async fn proxy_refuses_unknown_node() {
    let app = app_with(vec![node("node-a", "http://127.0.0.1:1")]);
    let resp = app
        .oneshot(get_req("/api/nodes/ghost/health"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    assert_eq!(v["error_code"], "OBP-BANK-NODE-APP-UNKNOWN-NODE");
}

#[tokio::test]
async fn proxy_refuses_paths_outside_the_whitelist() {
    // Never proxied: not a south-side read/trigger endpoint.
    let base = spawn_stub(stub_node()).await;
    let app = app_with(vec![node("node-a", &base)]);
    for uri in [
        "/api/nodes/node-a/outbox",
        "/api/nodes/node-a/transaction-requests/x/y",
        "/api/nodes/node-a/settlements/k/corridor/deeper",
    ] {
        let resp = app.clone().oneshot(get_req(uri)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "GET {uri}");
        let v = body_json(resp).await;
        assert_eq!(
            v["error_code"], "OBP-BANK-NODE-APP-PATH-NOT-ALLOWED",
            "GET {uri}"
        );
    }
    // POST is only allowed on the two trigger endpoints.
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/nodes/node-a/evidence")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn proxy_maps_unreachable_node_to_502() {
    // Port 1 refuses connections.
    let app = app_with(vec![node("node-a", "http://127.0.0.1:1")]);
    let resp = app
        .oneshot(get_req("/api/nodes/node-a/health"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = body_json(resp).await;
    assert_eq!(v["error_code"], "OBP-BANK-NODE-APP-UPSTREAM-001");
}

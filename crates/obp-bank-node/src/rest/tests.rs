// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! Integration tests for the south-side REST router.
//!
//! Uses `tower::ServiceExt::oneshot` to call the router directly without
//! binding a TCP socket.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use super::{build_router, BankNodeState};
use crate::evidence::{EvidenceStore, NewEvidence};
use crate::obp_client::{ObpAuth, ObpClient};
use crate::outbox::OutboxStore;
use crate::settlement_store::{NewClaim, SettlementStore};

/// Finality depth used across REST tests.
const TEST_FINALITY_DEPTH: u32 = 7;

/// A router backed by fresh in-memory stores. Each call gets its own stores,
/// so tests are isolated; use [`router_with_state`] when a test needs the same
/// stores across requests.
async fn router() -> axum::Router {
    let (app, _state) = router_with_state().await;
    app
}

async fn router_with_state() -> (axum::Router, BankNodeState) {
    // Port 1 refuses connections — tests that don't exercise Interface B
    // never touch it, and tests that do use `router_with_obp` instead.
    router_with_obp("http://127.0.0.1:1").await
}

async fn router_with_obp(obp_base_url: &str) -> (axum::Router, BankNodeState) {
    let state = BankNodeState {
        outbox: OutboxStore::connect_in_memory().await.unwrap(),
        settlements: SettlementStore::connect_in_memory().await.unwrap(),
        evidence: EvidenceStore::connect_in_memory().await.unwrap(),
        obp: Arc::new(ObpClient::new(obp_base_url, ObpAuth::None).unwrap()),
        // Unloaded — routing validation is fail-open unless a test loads it.
        routing: crate::routing::RoutingRegistry::default(),
        blockchain_label: "mock",
        bank_id: "test.bank.id".into(),
        account_id: "test-account-id".into(),
        corridor_accounts: Default::default(),
        finality_depth: TEST_FINALITY_DEPTH,
        interface_c: crate::interface_c::consumer::ConsumerStatus::new(),
        obp_api_url: obp_base_url.into(),
        chain_health_url: None,
        instance_id: "test-instance".into(),
        started_at: std::time::Instant::now(),
    };
    (build_router(state.clone()), state)
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

async fn get_with_accept(uri: &str, accept: Option<&str>) -> axum::response::Response {
    let mut req = Request::builder().method(Method::GET).uri(uri);
    if let Some(a) = accept {
        req = req.header("accept", a);
    }
    router().await.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
}

#[tokio::test]
async fn root_alive_and_health_are_minimal_like_obp_api() {
    let resp = get_with_accept("/alive", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"true");

    let resp = get_with_accept("/health", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v, serde_json::json!({"status": "ok"}));
}

#[tokio::test]
async fn root_status_is_503_json_with_each_check_when_degraded() {
    // The test state's OBP-API (port 1) refuses connections and its Interface C
    // never connects, so the node is not ready.
    let resp = get_with_accept("/status", Some("application/json")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["status"], "degraded");
    assert_eq!(v["service"], "OBP-Bank-Node");
    assert_eq!(v["instance_id"], "test-instance");
    assert_eq!(v["checks"]["database"]["status"], "ok");
    assert_eq!(v["checks"]["obp_api"]["status"], "fail");
    assert_eq!(v["checks"]["open_corridor"]["status"], "fail");
    assert_eq!(v["checks"]["blockchain"]["status"], "ok", "the mock has no chain to check");
    assert_eq!(v["checks"]["interface_c"]["status"], "fail");
    assert_eq!(v["outbox"]["detail"], "no transaction requests");
    assert!(v["git_commit"].is_string() && v["uptime_seconds"].is_u64());
}

#[tokio::test]
async fn ready_is_503_when_obp_api_is_unreachable() {
    let resp = get_with_accept("/ready", None).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["status"], "not_ready");
    assert_eq!(v["checks"]["database"]["status"], "ok");
    assert_eq!(v["checks"]["obp_api"]["status"], "fail");
    assert_eq!(v["checks"]["blockchain"]["status"], "ok");
    assert!(v["checks"].get("interface_c").is_none(), "readiness ignores Interface C");
}

#[tokio::test]
async fn ready_is_200_when_stores_obp_api_and_chain_answer() {
    let stub = axum::Router::new().route(
        "/obp/v6.0.0/root",
        axum::routing::get(|| async { axum::Json(serde_json::json!({"version": "v6.0.0"})) }),
    );
    let base = spawn_obp_stub(stub).await;
    let (app, _state) = router_with_obp(&base).await;
    let resp = app
        .oneshot(Request::builder().uri("/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["status"], "ready");
    assert_eq!(v["checks"]["obp_api"]["status"], "ok");
}

async fn status_json_of(app: axum::Router) -> serde_json::Value {
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/status")
                .header("accept", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

#[tokio::test]
async fn root_status_incoming_check_is_ok_when_nothing_open() {
    let (app, _state) = router_with_state().await;
    let v = status_json_of(app).await;
    assert_eq!(v["incoming_settlements"]["status"], "ok");
    assert_eq!(v["incoming_settlements"]["detail"], "no incoming settlements");
}

#[tokio::test]
async fn root_status_warns_on_mismatched_incoming_settlement() {
    use crate::settlement_store::{incoming_status, IncomingUpdate};
    let (app, state) = router_with_state().await;
    state
        .settlements
        .record_incoming("set-bad", Some("rt.bank.a"), Some("KES"), Some("50.00"), &["tr-1".into()])
        .await
        .unwrap();
    state
        .settlements
        .update_incoming("set-bad", IncomingUpdate {
            status: incoming_status::MISMATCH,
            tx_id: Some("tx-9"),
            asset: None,
            asset_amount: None,
            received_amount: None,
            depth: 0,
            detail: "nothing arrived",
        })
        .await
        .unwrap();
    let v = status_json_of(app).await;
    assert_eq!(v["incoming_settlements"]["status"], "warn");
    let detail = v["incoming_settlements"]["detail"].as_str().unwrap();
    assert!(detail.contains("MISMATCH 1") && detail.contains("set-bad"), "{detail}");
    assert_eq!(v["status"], "degraded", "soft: the overall verdict is the hard checks' alone");
}

#[tokio::test]
async fn root_status_is_html_by_default() {
    let resp = get_with_accept("/status", None).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let html = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(html.contains("OBP Bank Node Status:"));
    assert!(html.contains("<th>interface_c</th>"));
}

#[tokio::test]
async fn versioned_health_returns_200_with_blockchain_label() {
    let resp = router()
        .await
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/obp-bank-node/v5.1.0/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_bytes(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["status"], "healthy");
    assert_eq!(v["service"], "OBP-Bank-Node");
    assert_eq!(v["blockchain"], "mock");
    // A node that cannot receive AMQP must be visible: the consumer state is
    // part of health (this test state never connects, so "connecting").
    assert_eq!(v["interface_c"], "connecting");
}

#[tokio::test]
async fn initiate_payment_returns_202_with_uuid_and_state_identity() {
    let payload = serde_json::json!({
        "value": { "currency": "KES", "amount": "50000.00" },
        "description": "Invoice payment INV-2026-0042",
        "to": {
            "other_bank_routing_scheme": "OBP",
            "other_bank_routing_address": "ke.01.kcs",
            "other_account_routing_scheme": "OBP",
            "other_account_routing_address": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1"
        },
        "originator": {
            "name": "Acme Coffee Ltd",
            "address": "12 Market Street, Nairobi, Kenya",
            "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" }
        }
    });
    let resp = router()
        .await
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/obp-bank-node/v5.1.0/transaction-requests")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["status"], "INITIATED");
    assert_eq!(v["type"], "OPEN_CORRIDOR_PROMISE");
    // `from` comes from state (bank identity), not from the URL.
    assert_eq!(v["from"]["bank_id"], "test.bank.id");
    assert_eq!(v["from"]["account_id"], "test-account-id");
    // Inline routing and originator are echoed back (snake_case, OPEN_CORRIDOR_PROMISE).
    assert_eq!(v["to"]["other_bank_routing_scheme"], "OBP");
    assert_eq!(v["originator"]["name"], "Acme Coffee Ltd");
    assert_eq!(v["originator"]["source"], "explicit");
    assert_eq!(v["value"]["currency"], "KES");
    assert_eq!(v["value"]["amount"], "50000.00");
    assert!(!v["transaction_request_id"].as_str().unwrap().is_empty());
    assert!(v["challenge"].is_null());
    assert!(v["promise_id"].is_null());
}

/// A well-formed OPEN_CORRIDOR_PROMISE body, as a starting point for the negative tests.
fn valid_payload() -> serde_json::Value {
    serde_json::json!({
        "value": { "currency": "KES", "amount": "1500.00" },
        "description": "Invoice 4471",
        "to": {
            "other_bank_routing_scheme": "BIC",
            "other_bank_routing_address": "NWBKGB2LXXX",
            "other_account_routing_scheme": "IBAN",
            "other_account_routing_address": "GB29NWBK60161331926819"
        },
        "originator": {
            "name": "Acme Coffee Ltd",
            "address": "12 Market Street, Nairobi, Kenya",
            "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" }
        }
    })
}

async fn post_initiate(payload: serde_json::Value) -> axum::response::Response {
    router()
        .await
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/obp-bank-node/v5.1.0/transaction-requests")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// A router whose routing registry is loaded with the demo schemes — routing
/// validation is live (unlike the default fail-open unloaded registry).
async fn router_with_routing() -> axum::Router {
    let (app, state) = router_with_state().await;
    state.routing.load(vec![
        crate::obp_client::ObpRoutingScheme {
            scheme: "OBP".into(),
            status: "ACTIVE".into(),
            address_pattern: "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$".into(),
            example_address: "rt.bank.b".into(),
        },
        crate::obp_client::ObpRoutingScheme {
            scheme: "BIC".into(),
            status: "ACTIVE".into(),
            address_pattern: "^[A-Z]{6}[A-Z0-9]{2}([A-Z0-9]{3})?$".into(),
            example_address: "NWBKGB2LXXX".into(),
        },
        crate::obp_client::ObpRoutingScheme {
            scheme: "IBAN".into(),
            status: "ACTIVE".into(),
            address_pattern: "^[A-Z]{2}[0-9]{2}[A-Z0-9]{1,30}$".into(),
            example_address: "GB29NWBK60161331926819".into(),
        },
    ]);
    app
}

async fn post_initiate_to(
    app: axum::Router,
    payload: serde_json::Value,
) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri("/obp-bank-node/v5.1.0/transaction-requests")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn initiate_payment_accepts_valid_routing_when_registry_loaded() {
    // valid_payload uses BIC + IBAN — both registered with matching addresses.
    let resp = post_initiate_to(router_with_routing().await, valid_payload()).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn initiate_payment_rejects_unknown_routing_scheme() {
    let mut payload = valid_payload();
    payload["to"]["other_account_routing_scheme"] = serde_json::json!("SORT_CODE");
    let resp = post_initiate_to(router_with_routing().await, payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-BANK-NODE-ROUTING-002");
    assert!(v["message"].as_str().unwrap().contains("SORT_CODE"));
}

#[tokio::test]
async fn initiate_payment_rejects_address_not_matching_scheme_pattern() {
    let mut payload = valid_payload();
    // A BIC must be 8 or 11 chars — this one is malformed.
    payload["to"]["other_bank_routing_address"] = serde_json::json!("not-a-bic");
    let resp = post_initiate_to(router_with_routing().await, payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-BANK-NODE-ROUTING-003");
    // The message carries the scheme's example address so callers can self-correct.
    assert!(v["message"].as_str().unwrap().contains("NWBKGB2LXXX"));
}

#[tokio::test]
async fn initiate_payment_skips_routing_validation_while_registry_unloaded() {
    // Default router: registry never loaded — fail-open, unknown scheme passes.
    let mut payload = valid_payload();
    payload["to"]["other_bank_routing_scheme"] = serde_json::json!("TOTALLY_UNKNOWN");
    let resp = post_initiate(payload).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn versioned_status_reports_identity_and_corridor_accounts() {
    let resp = router_with_corridor_accounts()
        .await
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/obp-bank-node/v5.1.0/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["service"], "OBP-Bank-Node");
    assert_eq!(v["bank_id"], "test.bank.id");
    assert_eq!(v["account_id"], "test-account-id");
    assert_eq!(v["corridor_accounts"]["KES"], "settlement-kes");
    assert_eq!(v["corridor_accounts"]["NGN"], "settlement-ngn");
    assert_eq!(v["finality_depth"], TEST_FINALITY_DEPTH);
}

/// A router whose bank sends per-currency: KES and NGN map to their corridor
/// accounts; any other currency must be refused with ROUTING-001.
async fn router_with_corridor_accounts() -> axum::Router {
    let (_, mut state) = router_with_state().await;
    state.corridor_accounts = [
        ("KES".to_string(), "settlement-kes".to_string()),
        ("NGN".to_string(), "settlement-ngn".to_string()),
    ]
    .into();
    build_router(state)
}

#[tokio::test]
async fn initiate_payment_books_on_the_currency_corridor_account() {
    let mut payload = valid_payload();
    payload["value"]["currency"] = serde_json::json!("NGN");
    let resp = post_initiate_to(router_with_corridor_accounts().await, payload).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["from"]["account_id"], "settlement-ngn");
}

#[tokio::test]
async fn initiate_payment_resolves_currency_case_insensitively() {
    let mut payload = valid_payload();
    payload["value"]["currency"] = serde_json::json!("kes");
    let resp = post_initiate_to(router_with_corridor_accounts().await, payload).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["from"]["account_id"], "settlement-kes");
}

#[tokio::test]
async fn initiate_payment_rejects_currency_without_corridor_account() {
    let mut payload = valid_payload();
    payload["value"]["currency"] = serde_json::json!("USD");
    let resp = post_initiate_to(router_with_corridor_accounts().await, payload).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-BANK-NODE-ROUTING-001");
    assert!(v["message"].as_str().unwrap().contains("USD"));
}

#[tokio::test]
async fn initiate_payment_rejects_zero_amount() {
    let mut payload = valid_payload();
    payload["value"]["amount"] = serde_json::json!("0.00");
    let resp = post_initiate(payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-40008");
}

#[tokio::test]
async fn initiate_payment_rejects_negative_amount() {
    let mut payload = valid_payload();
    payload["value"]["amount"] = serde_json::json!("-5.00");
    let resp = post_initiate(payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-40008");
}

#[tokio::test]
async fn initiate_payment_rejects_non_numeric_amount() {
    let mut payload = valid_payload();
    payload["value"]["amount"] = serde_json::json!("lots");
    let resp = post_initiate(payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-10001");
}

#[tokio::test]
async fn initiate_payment_rejects_empty_routing_field() {
    let mut payload = valid_payload();
    payload["to"]["other_account_routing_address"] = serde_json::json!("");
    let resp = post_initiate(payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-10001");
}

#[tokio::test]
async fn initiate_payment_rejects_empty_originator_name() {
    let mut payload = valid_payload();
    payload["originator"]["name"] = serde_json::json!("  ");
    let resp = post_initiate(payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-BANK-NODE-ORIGINATOR-001");
}

#[tokio::test]
async fn initiate_payment_rejects_missing_originator_block() {
    let mut payload = valid_payload();
    payload.as_object_mut().unwrap().remove("originator");
    let resp = post_initiate(payload).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-10001");
}

#[tokio::test]
async fn initiate_payment_rejects_malformed_json() {
    let resp = router()
        .await
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/obp-bank-node/v5.1.0/transaction-requests")
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-10001");
}

#[tokio::test]
async fn get_unknown_transaction_request_returns_404() {
    let resp = router()
        .await
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/obp-bank-node/v5.1.0/transaction-requests/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-BANK-NODE-NOT-FOUND-001");
}

#[tokio::test]
async fn initiate_then_get_returns_persisted_initiated_row() {
    // Same router/state across both requests so the GET sees the POST's row.
    let (app, _state) = router_with_state().await;

    let post = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/obp-bank-node/v5.1.0/transaction-requests")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&valid_payload()).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::ACCEPTED);
    let posted: serde_json::Value = serde_json::from_slice(&body_bytes(post).await).unwrap();
    let id = posted["transaction_request_id"].as_str().unwrap();

    let get = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/obp-bank-node/v5.1.0/transaction-requests/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(get).await).unwrap();
    assert_eq!(v["transaction_request_id"], id);
    // No dispatcher runs in this test, so the row is still at INITIATED.
    assert_eq!(v["status"], "INITIATED");
    assert!(v["promise_id"].is_null());
    // The stored payload is projected back onto the status view (the /app
    // position view nets these fields without consulting OBP-API).
    assert_eq!(v["value"]["currency"], "KES");
    assert_eq!(v["value"]["amount"], "1500.00");
    assert_eq!(v["other_bank_id"], "NWBKGB2LXXX");
    assert_eq!(v["description"], "Invoice 4471");
}

#[tokio::test]
async fn list_transaction_requests_returns_empty_array() {
    let resp = router()
        .await
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/obp-bank-node/v5.1.0/transaction-requests")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert!(v.is_array());
    assert_eq!(v.as_array().unwrap().len(), 0);
}

// ---------- settlement store read endpoints ----------

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    (status, v)
}

#[tokio::test]
async fn settlements_list_empty_and_unknown_key_404() {
    let (app, _state) = router_with_state().await;
    let (status, v) = get_json(app.clone(), "/obp-bank-node/v5.1.0/settlements").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 0);

    let (status, v) = get_json(app, "/obp-bank-node/v5.1.0/settlements/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error_code"], "OBP-BANK-NODE-NOT-FOUND-001");
}

#[tokio::test]
async fn settlement_row_is_readable_by_key_or_settlement_id() {
    let (app, state) = router_with_state().await;
    state
        .settlements
        .claim(NewClaim {
            idempotency_key: "idem-9",
            settlement_id: Some("settle-9"),
            snapshot_id: None,
            currency: "KES",
            net_amount_minor: 100_000,
            creditor_address: "addr_test1creditor",
            purpose: Some("PLATFORM_FEE"),
        })
        .await
        .unwrap();
    state
        .settlements
        .mark_submitted(
            "idem-9",
            "tx-9",
            "cardano",
            "ADA",
            "47125000",
            Some(&obp_blockchain::settlement::FxQuote {
                asset: "ADA".into(),
                currency: "KES".into(),
                minor_per_whole_asset: 2122,
                as_of: chrono::Utc::now(),
                source: "stub".into(),
            }),
        )
        .await
        .unwrap();
    state.settlements.record_depth("idem-9", 2).await.unwrap();

    let (status, v) = get_json(app.clone(), "/obp-bank-node/v5.1.0/settlements/idem-9").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "SUBMITTED");
    assert_eq!(v["tx_id"], "tx-9");
    assert_eq!(v["depth"], 2);
    assert_eq!(v["finality_depth"], TEST_FINALITY_DEPTH);
    assert_eq!(v["net_amount_minor"], "100000");
    // The settle-time rate is surfaced for the UI's rate column.
    assert_eq!(v["fx_minor_per_whole_asset"], "2122");
    assert_eq!(v["fx_source"], "stub");
    assert!(v["fx_as_of"].as_str().is_some());
    // Fee-sweep settlements are labeled for the UI.
    assert_eq!(v["purpose"], "PLATFORM_FEE");

    // Same row via settlement_id, and present in the list.
    let (status, by_sid) =
        get_json(app.clone(), "/obp-bank-node/v5.1.0/settlements/settle-9").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_sid["idempotency_key"], "idem-9");
    let (_, list) = get_json(app, "/obp-bank-node/v5.1.0/settlements").await;
    assert_eq!(list.as_array().unwrap().len(), 1);
}

// ---------- evidence store read endpoints ----------

#[tokio::test]
async fn evidence_endpoints_serve_triplet_and_cbs_result() {
    let (app, state) = router_with_state().await;
    state
        .evidence
        .upsert(NewEvidence {
            transaction_request_id: "obp-tr-1",
            promise_commitment: "c0ffee",
            promise_salt: "5a17",
            promise_preimage: r#"{"amount":"500.00"}"#,
            promise_id: Some("cardano-tx-1"),
            promise_blockchain: Some("cardano"),
            verified: true,
            currency: Some("KES"),
            amount: Some("500.00"),
            originator_name: Some("Acme Coffee Ltd"),
            beneficiary_name: Some("Bea Beneficiary"),
            beneficiary_account_routing_scheme: Some("OBP"),
            beneficiary_account_routing_address: Some("acct-77"),
            return_of_transaction_request_id: None,
            raw_message: "{}",
        })
        .await
        .unwrap();
    state
        .evidence
        .record_cbs_result("obp-tr-1", "DELIVERED", Some("CBS-77"))
        .await
        .unwrap();

    let (status, v) = get_json(app.clone(), "/obp-bank-node/v5.1.0/evidence/obp-tr-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["promise_commitment"], "c0ffee");
    assert_eq!(v["promise_salt"], "5a17");
    assert_eq!(v["verified"], true);
    assert_eq!(v["cbs_status"], "DELIVERED");
    assert_eq!(v["cbs_reference"], "CBS-77");
    assert_eq!(v["beneficiary_name"], "Bea Beneficiary");
    assert_eq!(v["beneficiary_account_routing_scheme"], "OBP");
    assert_eq!(v["beneficiary_account_routing_address"], "acct-77");

    let (_, list) = get_json(app.clone(), "/obp-bank-node/v5.1.0/evidence").await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, v) = get_json(app, "/obp-bank-node/v5.1.0/evidence/unknown").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error_code"], "OBP-BANK-NODE-NOT-FOUND-001");
}

// ---------- settle trigger + corridor proxy ----------

/// Spin up an OBP-API stand-in and return its base URL.
async fn spawn_obp_stub(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn obp_settle_result() -> serde_json::Value {
    serde_json::json!({
        "settlement_id": "settle-42",
        "settlement_transaction_request_id": "obp-tr-settle",
        "transaction_id": "txn-1",
        "debtor_bank_id": "test.bank.id",
        "creditor_bank_id": "other.bank",
        "currency": "KES",
        "net_amount": "1000.00",
        "covered_transaction_request_ids": ["obp-tr-a", "obp-tr-theirs"],
        "credit_notifications_enqueued": 2,
        "settlement_instructions_enqueued": 1
    })
}

async fn post_settle(
    app: axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/obp-bank-node/v5.1.0/settlements")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    (status, v)
}

#[tokio::test]
async fn request_settlement_calls_obp_and_stamps_covered_outbox_rows() {
    use axum::routing::post as axum_post;
    let stub = axum::Router::new().route(
        "/obp/v7.0.0/banks/test.bank.id/open-corridor/settlements",
        axum_post(
            |axum::Json(body): axum::Json<serde_json::Value>| async move {
                assert_eq!(body["other_bank_id"], "other.bank");
                assert_eq!(body["currency"], "KES");
                (StatusCode::CREATED, axum::Json(obp_settle_result()))
            },
        ),
    );
    let base = spawn_obp_stub(stub).await;
    let (app, state) = router_with_obp(&base).await;

    // A local outbound row whose OBP TR id is in the covered list, and one
    // that is not.
    for (id, obp_id) in [("tr-a", "obp-tr-a"), ("tr-b", "obp-tr-b")] {
        state
            .outbox
            .insert(crate::outbox::NewEntry {
                transaction_request_id: id,
                bank_id: "test.bank.id",
                account_id: "test-account-id",
                request_payload: "{}",
                commitment_salt: "aa",
            })
            .await
            .unwrap();
        state.outbox.mark_submitted(id, Some(obp_id)).await.unwrap();
    }

    let (status, v) = post_settle(
        app,
        serde_json::json!({ "other_bank_id": "other.bank", "currency": "KES" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // OBP's result is passed through, plus the local stamping count.
    assert_eq!(v["settlement_id"], "settle-42");
    assert_eq!(v["net_amount"], "1000.00");
    assert_eq!(v["covered_outbox_rows_stamped"], 1);

    // The covered row carries the linkage; the uncovered one does not.
    let a = state.outbox.get("tr-a").await.unwrap().unwrap();
    assert_eq!(a.settlement_id.as_deref(), Some("settle-42"));
    assert!(a.settled_at.is_some());
    assert!(state
        .outbox
        .get("tr-b")
        .await
        .unwrap()
        .unwrap()
        .settlement_id
        .is_none());
}

#[tokio::test]
async fn request_settlement_validates_body_locally() {
    let (app, _state) = router_with_state().await;
    let (status, v) = post_settle(
        app.clone(),
        serde_json::json!({ "other_bank_id": "", "currency": "KES" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error_code"], "OBP-10001");

    // Settling against yourself is refused before any OBP call.
    let (status, v) = post_settle(
        app,
        serde_json::json!({ "other_bank_id": "test.bank.id", "currency": "KES" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error_code"], "OBP-10001");
}

#[tokio::test]
async fn request_settlement_passes_obp_rejection_through() {
    use axum::routing::post as axum_post;
    let stub = axum::Router::new().route(
        "/obp/v7.0.0/banks/:bank/open-corridor/settlements",
        axum_post(|| async {
            (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "code": 403,
                    "message": "OBP-20006: User is missing one or more roles: CanSettleOpenCorridor"
                })),
            )
        }),
    );
    let base = spawn_obp_stub(stub).await;
    let (app, _state) = router_with_obp(&base).await;

    let (status, v) = post_settle(
        app,
        serde_json::json!({ "other_bank_id": "other.bank", "currency": "KES" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(v["error_code"], "OBP-20006");
}

#[tokio::test]
async fn request_settlement_upstream_down_is_502() {
    let (app, _state) = router_with_state().await; // OBP base points at a refused port
    let (status, v) = post_settle(
        app,
        serde_json::json!({ "other_bank_id": "other.bank", "currency": "KES" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(v["error_code"], "OBP-BANK-NODE-INTERFACE-B-001");
}

#[tokio::test]
async fn corridor_settlement_proxies_obp_and_stamps_linkage() {
    use axum::routing::get as axum_get;
    let stub = axum::Router::new().route(
        "/obp/v7.0.0/banks/test.bank.id/open-corridor/settlements/settle-42",
        axum_get(|| async {
            axum::Json(serde_json::json!({
                "settlement_id": "settle-42",
                "ledger_status": "COMPLETED",
                "settlement_status": "FINAL",
                "settlement_depth": 15,
                "covered_transaction_request_ids": ["obp-tr-a"],
                "messages": []
            }))
        }),
    );
    let base = spawn_obp_stub(stub).await;
    let (app, state) = router_with_obp(&base).await;

    // This node did NOT trigger the settlement — its row gets stamped by the
    // corridor status read instead.
    state
        .outbox
        .insert(crate::outbox::NewEntry {
            transaction_request_id: "tr-a",
            bank_id: "test.bank.id",
            account_id: "test-account-id",
            request_payload: "{}",
            commitment_salt: "aa",
        })
        .await
        .unwrap();
    state
        .outbox
        .mark_submitted("tr-a", Some("obp-tr-a"))
        .await
        .unwrap();

    let (status, v) = get_json(app, "/obp-bank-node/v5.1.0/settlements/settle-42/corridor").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["settlement_status"], "FINAL");

    let rec = state.outbox.get("tr-a").await.unwrap().unwrap();
    assert_eq!(rec.settlement_id.as_deref(), Some("settle-42"));
    // The status endpoint now surfaces the linkage.
}

#[tokio::test]
async fn corridor_settlement_passes_404_through() {
    use axum::routing::get as axum_get;
    let stub = axum::Router::new().route(
        "/obp/v7.0.0/banks/:bank/open-corridor/settlements/:id",
        axum_get(|| async {
            (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({
                    "code": 404,
                    "message": "OBP-40058: No Open Corridor settlement with this SETTLEMENT_ID exists for this bank."
                })),
            )
        }),
    );
    let base = spawn_obp_stub(stub).await;
    let (app, _state) = router_with_obp(&base).await;

    let (status, v) = get_json(app, "/obp-bank-node/v5.1.0/settlements/nope/corridor").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error_code"], "OBP-40058");
}

#[tokio::test]
async fn transaction_request_status_surfaces_settlement_linkage() {
    let (app, state) = router_with_state().await;
    state
        .outbox
        .insert(crate::outbox::NewEntry {
            transaction_request_id: "tr-linked",
            bank_id: "test.bank.id",
            account_id: "test-account-id",
            request_payload: "{}",
            commitment_salt: "aa",
        })
        .await
        .unwrap();
    state
        .outbox
        .mark_submitted("tr-linked", Some("obp-tr-x"))
        .await
        .unwrap();
    state
        .outbox
        .mark_settled(&["obp-tr-x".into()], "settle-7")
        .await
        .unwrap();

    let (status, v) = get_json(app, "/obp-bank-node/v5.1.0/transaction-requests/tr-linked").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["settlement_id"], "settle-7");
    assert!(!v["settled_at"].is_null());
}

fn idempotent_payment_body(amount: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "value": { "currency": "KES", "amount": amount },
        "description": "Invoice INV-7",
        "to": {
            "other_bank_routing_scheme": "OBP",
            "other_bank_routing_address": "ke.01.kcs",
            "other_account_routing_scheme": "OBP",
            "other_account_routing_address": "acct-7"
        },
        "originator": {
            "name": "Acme Coffee Ltd",
            "address": "12 Market Street, Nairobi, Kenya",
            "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" }
        }
    }))
    .unwrap()
}

async fn post_payment(app: axum::Router, key: Option<&str>, body: Vec<u8>) -> axum::response::Response {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/obp-bank-node/v5.1.0/transaction-requests")
        .header("content-type", "application/json");
    if let Some(k) = key {
        req = req.header("idempotency-key", k);
    }
    app.oneshot(req.body(Body::from(body)).unwrap()).await.unwrap()
}

#[tokio::test]
async fn initiate_payment_replays_same_idempotency_key_with_same_id() {
    let (app, _state) = router_with_state().await;
    let first = post_payment(app.clone(), Some("inv-7"), idempotent_payment_body("100.00")).await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    assert!(first.headers().get("idempotency-replayed").is_none());
    let first: serde_json::Value = serde_json::from_slice(&body_bytes(first).await).unwrap();

    let again = post_payment(app, Some("inv-7"), idempotent_payment_body("100.00")).await;
    assert_eq!(again.status(), StatusCode::ACCEPTED);
    assert_eq!(again.headers().get("idempotency-replayed").unwrap(), "true");
    let again: serde_json::Value = serde_json::from_slice(&body_bytes(again).await).unwrap();
    assert_eq!(again["transaction_request_id"], first["transaction_request_id"], "no second payment");
    assert_eq!(again["status"], "INITIATED");
}

#[tokio::test]
async fn initiate_payment_rejects_same_key_with_different_body_422() {
    let (app, _state) = router_with_state().await;
    assert_eq!(post_payment(app.clone(), Some("inv-8"), idempotent_payment_body("100.00")).await.status(), StatusCode::ACCEPTED);
    let resp = post_payment(app, Some("inv-8"), idempotent_payment_body("999.00")).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["error_code"], "OBP-BANK-NODE-IDEMPOTENCY-001");
}

#[tokio::test]
async fn initiate_payment_without_key_creates_distinct_rows() {
    let (app, state) = router_with_state().await;
    for _ in 0..2 {
        assert_eq!(post_payment(app.clone(), None, idempotent_payment_body("100.00")).await.status(), StatusCode::ACCEPTED);
    }
    assert_eq!(state.outbox.count_by_status().await.unwrap().iter().map(|(_, n)| n).sum::<i64>(), 2);
}

#[tokio::test]
async fn initiate_payment_rejects_an_overlong_idempotency_key() {
    let (app, _state) = router_with_state().await;
    let key = "k".repeat(256);
    let resp = post_payment(app, Some(&key), idempotent_payment_body("100.00")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

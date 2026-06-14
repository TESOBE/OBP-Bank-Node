//! Integration tests for the south-side REST router.
//!
//! Uses `tower::ServiceExt::oneshot` to call the router directly without
//! binding a TCP socket.

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use super::{build_router, BankNodeState};
use crate::outbox::OutboxStore;

/// A router backed by a fresh in-memory outbox. Each call gets its own store,
/// so tests are isolated; use [`router_with_state`] when a test needs the same
/// store across requests.
async fn router() -> axum::Router {
    let (app, _state) = router_with_state().await;
    app
}

async fn router_with_state() -> (axum::Router, BankNodeState) {
    let state = BankNodeState {
        outbox: OutboxStore::connect_in_memory().await.unwrap(),
        blockchain_label: "mock",
        bank_id: "test.bank.id".into(),
        account_id: "test-account-id".into(),
    };
    (build_router(state.clone()), state)
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn root_health_returns_200_with_blockchain_label() {
    let resp = router().await
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
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
}

#[tokio::test]
async fn versioned_health_endpoint_responds() {
    let resp = router().await
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
    let resp = router().await
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
    assert_eq!(v["type"], "OPEN_CORRIDOR");
    // `from` comes from state (bank identity), not from the URL.
    assert_eq!(v["from"]["bank_id"], "test.bank.id");
    assert_eq!(v["from"]["account_id"], "test-account-id");
    // Inline routing and originator are echoed back (snake_case, OPEN_CORRIDOR).
    assert_eq!(v["to"]["other_bank_routing_scheme"], "OBP");
    assert_eq!(v["originator"]["name"], "Acme Coffee Ltd");
    assert_eq!(v["originator"]["source"], "explicit");
    assert_eq!(v["value"]["currency"], "KES");
    assert_eq!(v["value"]["amount"], "50000.00");
    assert!(!v["transaction_request_id"].as_str().unwrap().is_empty());
    assert!(v["challenge"].is_null());
    assert!(v["promise_id"].is_null());
}

/// A well-formed OPEN_CORRIDOR body, as a starting point for the negative tests.
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
    let resp = router().await
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
    let resp = router().await
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
}

#[tokio::test]
async fn list_transaction_requests_returns_empty_array() {
    let resp = router().await
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

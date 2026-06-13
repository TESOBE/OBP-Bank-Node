//! Integration tests for the south-side REST router.
//!
//! Uses `tower::ServiceExt::oneshot` to call the router directly without
//! binding a TCP socket.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use obp_blockchain::mock::MockBackend;
use tower::ServiceExt;

use super::{build_router, BankNodeState};

fn router() -> axum::Router {
    let state = BankNodeState {
        backend: Arc::new(MockBackend::new()),
        blockchain_label: "mock",
        bank_id: "test.bank.id".into(),
        account_id: "test-account-id".into(),
    };
    build_router(state)
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn root_health_returns_200_with_blockchain_label() {
    let resp = router()
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
    let resp = router()
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
    let resp = router()
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
    assert!(v["transaction_request_id"].as_str().unwrap().len() > 0);
    assert!(v["challenge"].is_null());
    assert!(v["promise_id"].is_null());
}

#[tokio::test]
async fn get_transaction_request_returns_stub_status() {
    let resp = router()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/obp-bank-node/v5.1.0/transaction-requests/abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["transaction_request_id"], "abc-123");
    assert_eq!(v["status"], "INITIATED");
}

#[tokio::test]
async fn list_transaction_requests_returns_empty_array() {
    let resp = router()
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

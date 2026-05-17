//! Integration tests for the south-side REST router.
//!
//! Uses `tower::ServiceExt::oneshot` to call the router directly without
//! binding a TCP socket.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use obp_blockchain::mock::MockConnector;
use tower::ServiceExt;

use super::{build_router, BankNodeState};

fn router() -> axum::Router {
    let state = BankNodeState {
        connector: Arc::new(MockConnector::new()),
        blockchain_label: "mock",
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
async fn initiate_payment_returns_202_with_uuid_and_echoes_value() {
    let payload = serde_json::json!({
        "value": { "currency": "KES", "amount": "50000.00" },
        "description": "Invoice payment INV-2026-0042",
        "to": {
            "otherBankRoutingScheme": "OBP",
            "otherBankRoutingAddress": "ke.01.kcs",
            "otherAccountRoutingScheme": "OBP",
            "otherAccountRoutingAddress": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1"
        },
        "charge_policy": "SHARED"
    });
    let resp = router()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(
                    "/obp-bank-node/v5.1.0/banks/gh.29.uk/accounts/8ca8a7e4/views/owner\
                     /transaction-request-types/SIMPLE/transaction-requests",
                )
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["status"], "INITIATED");
    assert_eq!(v["type"], "COUNTERPARTY");
    assert_eq!(v["from"]["bank_id"], "gh.29.uk");
    assert_eq!(v["from"]["account_id"], "8ca8a7e4");
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
                .uri(
                    "/obp-bank-node/v5.1.0/banks/gh.29.uk/accounts/8ca8a7e4/views/owner\
                     /transaction-requests/abc-123",
                )
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
                .uri(
                    "/obp-bank-node/v5.1.0/banks/gh.29.uk/accounts/8ca8a7e4/views/owner\
                     /transaction-requests",
                )
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

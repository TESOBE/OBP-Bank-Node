//! Route handlers for the south-side REST API.
//!
//! Phase 1: stub responses. Once the outbox, OBP API client, and full
//! `CardanoConnector` write path land, these handlers will:
//!   1. Persist the request to the outbox (durability before any external call)
//!   2. Resolve beneficiary routing to an OBP API counterparty
//!   3. Submit the OBP Transaction Request
//!   4. Write the Cardano Promise record
//!   5. Return 202 with the real transaction_request_id

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use super::types::*;
use super::BankNodeState;

#[derive(Debug, Deserialize)]
pub struct InitiatePath {
    pub bank_id: String,
    pub account_id: String,
    pub view_id: String,
}

#[derive(Debug, Deserialize)]
pub struct StatusPath {
    pub bank_id: String,
    #[allow(dead_code)]
    pub account_id: String,
    #[allow(dead_code)]
    pub view_id: String,
    pub transaction_request_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ListPath {
    pub bank_id: String,
    #[allow(dead_code)]
    pub account_id: String,
    #[allow(dead_code)]
    pub view_id: String,
}

pub async fn initiate_payment(
    State(_state): State<BankNodeState>,
    Path(p): Path<InitiatePath>,
    Json(req): Json<InitiateRequest>,
) -> Response {
    let id = Uuid::new_v4().to_string();
    info!(
        bank_id = %p.bank_id,
        account_id = %p.account_id,
        view_id = %p.view_id,
        transaction_request_id = %id,
        currency = %req.value.currency,
        amount = %req.value.amount,
        scheme = %req.to.other_bank_routing_scheme,
        "initiate_payment (Phase 1 stub — outbox / OBP-API / chain wiring not yet implemented)"
    );

    let body = InitiatedResponse {
        transaction_request_id: id,
        kind: "COUNTERPARTY",
        from: FromAccount {
            bank_id: p.bank_id,
            account_id: p.account_id,
        },
        to: ToCounterparty {
            // Real counterparty resolution lands with the OBP API client.
            counterparty_id: "TODO-counterparty-resolution".into(),
        },
        value: req.value,
        description: req.description,
        status: "INITIATED",
        promise_id: None,
        start_date: Utc::now(),
        end_date: None,
        challenge: None,
    };
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

pub async fn get_transaction_request(
    State(_state): State<BankNodeState>,
    Path(p): Path<StatusPath>,
) -> Response {
    warn!(
        transaction_request_id = %p.transaction_request_id,
        bank_id = %p.bank_id,
        "get_transaction_request (Phase 1 stub — returns INITIATED for any id)"
    );
    let body = TransactionRequestStatus {
        transaction_request_id: p.transaction_request_id,
        status: "INITIATED".into(),
        promise_id: None,
        promise_blockchain: None,
        netting_snapshot_id: None,
        netting_blockchain: None,
        settlement_id: None,
        settlement_system: None,
        created_at: Utc::now(),
        settled_at: None,
    };
    Json(body).into_response()
}

pub async fn list_transaction_requests(
    State(_state): State<BankNodeState>,
    Path(p): Path<ListPath>,
) -> Response {
    warn!(
        bank_id = %p.bank_id,
        "list_transaction_requests (Phase 1 stub — returns empty array)"
    );
    Json(Vec::<TransactionRequestStatus>::new()).into_response()
}

pub async fn root_health(State(state): State<BankNodeState>) -> Response {
    Json(HealthBody {
        status: "healthy",
        service: "OBP-Bank-Node",
        version: env!("CARGO_PKG_VERSION"),
        blockchain: state.blockchain_label,
        timestamp: Utc::now(),
    })
    .into_response()
}

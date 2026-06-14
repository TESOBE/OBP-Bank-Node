//! Route handlers for the south-side REST API.
//!
//! Phase 1: stub responses. Once the outbox, OBP API client, and full
//! `CardanoBackend` write path land, these handlers will:
//!   1. Persist the request to the outbox (durability before any external call)
//!   2. Resolve `value.currency` to the bank's settlement account
//!   3. Submit the OBP OPEN_CORRIDOR Transaction Request (inline routing)
//!   4. Write the Cardano Promise record
//!   5. Return 202 with the real transaction_request_id
//!
//! Synchronous request validation (steps 1–2 of the A1.1 table in `A1_A2.md`)
//! is implemented: malformed body, zero/negative amount, and empty
//! beneficiary-routing / originator fields are rejected before the 202.

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::types::*;
use super::BankNodeState;
use crate::outbox::{NewEntry, OutboxRecord};

/// Build an OBP-style error response: an HTTP status plus an [`ErrorBody`]
/// carrying the OBP error code and a human-readable message.
fn error(status: StatusCode, error_code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error_code: error_code.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

/// Validate an A1.1 request body. Mirrors the error table in `A1_A2.md`:
///   - `OBP-40008` — amount is zero, negative, or not a number,
///   - `OBP-10001` — a required `value` / `to` field is empty,
///   - `OBP-BANK-NODE-ORIGINATOR-001` — an `originator` field is empty.
///
/// Returns the OBP error code and message on the first failure. Currency →
/// settlement-account resolution (`422 OBP-BANK-NODE-ROUTING-001`) lands with
/// the OBP API client and per-currency `bank` config, so it is not checked here.
fn validate(req: &InitiateRequest) -> Result<(), (StatusCode, &'static str, String)> {
    const BAD_REQUEST: StatusCode = StatusCode::BAD_REQUEST;

    if req.value.currency.trim().is_empty() {
        return Err((BAD_REQUEST, "OBP-10001", "value.currency is required".into()));
    }

    // Parse only to check the sign — the amount is carried as a string and is
    // never used for arithmetic here, so f64 is safe for this comparison.
    match req.value.amount.trim().parse::<f64>() {
        Err(_) => {
            return Err((
                BAD_REQUEST,
                "OBP-10001",
                format!("value.amount is not a valid number: {:?}", req.value.amount),
            ))
        }
        Ok(amount) if amount <= 0.0 => {
            return Err((
                BAD_REQUEST,
                "OBP-40008",
                "value.amount must be greater than zero".into(),
            ))
        }
        Ok(_) => {}
    }

    for (field, value) in [
        ("other_bank_routing_scheme", &req.to.other_bank_routing_scheme),
        ("other_bank_routing_address", &req.to.other_bank_routing_address),
        ("other_account_routing_scheme", &req.to.other_account_routing_scheme),
        ("other_account_routing_address", &req.to.other_account_routing_address),
    ] {
        if value.trim().is_empty() {
            return Err((BAD_REQUEST, "OBP-10001", format!("to.{field} is required")));
        }
    }

    if req.originator.name.trim().is_empty()
        || req.originator.address.trim().is_empty()
        || req.originator.account_routing.scheme.trim().is_empty()
        || req.originator.account_routing.address.trim().is_empty()
    {
        return Err((
            BAD_REQUEST,
            "OBP-BANK-NODE-ORIGINATOR-001",
            "originator.name, originator.address, and originator.account_routing must all be present and non-empty".into(),
        ));
    }

    Ok(())
}

pub async fn initiate_payment(
    State(state): State<BankNodeState>,
    body: Result<Json<InitiateRequest>, JsonRejection>,
) -> Response {
    // Malformed JSON or a missing required field surfaces as a deserialization
    // rejection — map it to OBP-10001 rather than axum's default plain-text 4xx.
    let Json(req) = match body {
        Ok(json) => json,
        Err(rejection) => {
            warn!(error = %rejection, "initiate_payment rejected malformed body");
            return error(StatusCode::BAD_REQUEST, "OBP-10001", rejection.body_text());
        }
    };

    if let Err((status, code, message)) = validate(&req) {
        warn!(%code, %message, "initiate_payment rejected invalid request");
        return error(status, code, message);
    }

    let id = Uuid::new_v4().to_string();

    // Re-serialize the validated request as the canonical payload the dispatcher
    // will replay to OBP-API.
    let request_payload = match serde_json::to_string(&req) {
        Ok(s) => s,
        Err(e) => {
            error!(transaction_request_id = %id, error = %e, "failed to serialize request payload");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to serialize transaction request",
            );
        }
    };

    // Persist to the outbox BEFORE returning — this is what makes the 202
    // durable. The dispatcher takes it from here (OBP-API submit, Promise
    // write). A 128-bit salt is minted now for the eventual Promise commitment.
    let salt = Uuid::new_v4().simple().to_string();
    if let Err(e) = state
        .outbox
        .insert(NewEntry {
            transaction_request_id: &id,
            bank_id: &state.bank_id,
            account_id: &state.account_id,
            request_payload: &request_payload,
            commitment_salt: &salt,
        })
        .await
    {
        error!(transaction_request_id = %id, error = %e, "failed to persist to outbox");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OBP-BANK-NODE-PLATFORM-001",
            "failed to persist transaction request",
        );
    }

    info!(
        bank_id = %state.bank_id,
        account_id = %state.account_id,
        transaction_request_id = %id,
        currency = %req.value.currency,
        amount = %req.value.amount,
        scheme = %req.to.other_bank_routing_scheme,
        "initiate_payment persisted to outbox (INITIATED) — dispatcher will submit"
    );

    // Echo the caller's inline routing and originator back in the 202. The
    // promise_id stays null here; it is filled asynchronously once the
    // dispatcher writes the Promise commitment (queryable via the status route).
    let mut originator = req.originator;
    originator.source = Some("explicit".into());

    let body = InitiatedResponse {
        transaction_request_id: id,
        kind: "OPEN_CORRIDOR",
        from: FromAccount {
            bank_id: state.bank_id.clone(),
            account_id: state.account_id.clone(),
        },
        to: req.to,
        originator,
        value: req.value,
        description: req.description,
        status: "INITIATED",
        promise_id: None,
        promise_blockchain: None,
        start_date: Utc::now(),
        challenge: None,
    };
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

pub async fn get_transaction_request(
    State(state): State<BankNodeState>,
    Path(transaction_request_id): Path<String>,
) -> Response {
    match state.outbox.get(&transaction_request_id).await {
        Ok(Some(rec)) => Json(status_from_record(rec)).into_response(),
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            "OBP-BANK-NODE-NOT-FOUND-001",
            format!("no transaction request with id {transaction_request_id}"),
        ),
        Err(e) => {
            error!(%transaction_request_id, error = %e, "outbox read failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to read transaction request",
            )
        }
    }
}

/// Cap on the number of rows returned by the list endpoint.
const LIST_LIMIT: i64 = 200;

pub async fn list_transaction_requests(State(state): State<BankNodeState>) -> Response {
    match state.outbox.list(LIST_LIMIT).await {
        Ok(recs) => {
            let body: Vec<_> = recs.into_iter().map(status_from_record).collect();
            Json(body).into_response()
        }
        Err(e) => {
            error!(bank_id = %state.bank_id, error = %e, "outbox list failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to list transaction requests",
            )
        }
    }
}

/// Project an outbox row onto the south-side status shape. Netting and
/// settlement fields stay null until those flows (Interface C) land.
fn status_from_record(rec: OutboxRecord) -> TransactionRequestStatus {
    TransactionRequestStatus {
        transaction_request_id: rec.transaction_request_id,
        status: rec.status,
        promise_id: rec.promise_tx_id,
        promise_blockchain: rec.promise_blockchain,
        netting_snapshot_id: None,
        netting_blockchain: None,
        settlement_id: None,
        settlement_system: None,
        created_at: parse_rfc3339(&rec.created_at),
        settled_at: None,
    }
}

/// Parse a stored RFC3339 timestamp, falling back to now if a row somehow holds
/// an unparseable value (it never should — the store writes RFC3339).
fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
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

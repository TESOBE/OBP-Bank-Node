//! Route handlers for the south-side REST API.
//!
//! Phase 1: stub responses. Once the outbox, OBP API client, and full
//! `CardanoBackend` write path land, these handlers will:
//!   1. Persist the request to the outbox (durability before any external call)
//!   2. Resolve `value.currency` to the bank's settlement account
//!   3. Submit the OBP OPEN_CORRIDOR_PROMISE Transaction Request (inline routing)
//!   4. Write the Cardano Promise record
//!   5. Return 202 with the real transaction_request_id
//!
//! Synchronous request validation (steps 1–2 of the A1.1 table in `DOCS/A1_A2.md`)
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
use crate::evidence::EvidenceRecord;
use crate::obp_client::ObpClientError;
use crate::outbox::{NewEntry, OutboxRecord};
use crate::routing::RoutingViolation;
use crate::settlement_store::SettlementRow;

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

/// Validate an A1.1 request body. Mirrors the error table in `DOCS/A1_A2.md`:
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
        return Err((
            BAD_REQUEST,
            "OBP-10001",
            "value.currency is required".into(),
        ));
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
        (
            "other_bank_routing_scheme",
            &req.to.other_bank_routing_scheme,
        ),
        (
            "other_bank_routing_address",
            &req.to.other_bank_routing_address,
        ),
        (
            "other_account_routing_scheme",
            &req.to.other_account_routing_scheme,
        ),
        (
            "other_account_routing_address",
            &req.to.other_account_routing_address,
        ),
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

    // Beneficiary routing against the cached OBP-API registry — reject an
    // unknown scheme or malformed address before anything is persisted.
    // Skipped while the registry has never loaded (fail-open).
    for (field, scheme, address) in [
        (
            "to.other_bank_routing",
            &req.to.other_bank_routing_scheme,
            &req.to.other_bank_routing_address,
        ),
        (
            "to.other_account_routing",
            &req.to.other_account_routing_scheme,
            &req.to.other_account_routing_address,
        ),
    ] {
        if let Err(v) = state.routing.check(field, scheme, address) {
            let (code, message) = routing_violation_message(v);
            warn!(%code, %message, "initiate_payment rejected beneficiary routing");
            return error(StatusCode::BAD_REQUEST, code, message);
        }
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
        kind: "OPEN_CORRIDOR_PROMISE",
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

/// Project an outbox row onto the south-side status shape. Netting fields stay
/// null until the snapshot flow lands; the settlement linkage is stamped onto
/// the row by the settle flows (see `OutboxStore::mark_settled`).
fn status_from_record(rec: OutboxRecord) -> TransactionRequestStatus {
    let parsed: Option<InitiateRequest> = serde_json::from_str(&rec.request_payload).ok();
    let (value, other_bank_id, description) = match parsed {
        Some(req) => (
            Some(req.value),
            Some(req.to.other_bank_routing_address),
            Some(req.description),
        ),
        None => (None, None, None),
    };
    TransactionRequestStatus {
        transaction_request_id: rec.transaction_request_id,
        status: rec.status,
        value,
        other_bank_id,
        description,
        promise_id: rec.promise_tx_id,
        promise_blockchain: rec.promise_blockchain,
        netting_snapshot_id: None,
        netting_blockchain: None,
        settlement_id: rec.settlement_id,
        settlement_system: None,
        created_at: parse_rfc3339(&rec.created_at),
        settled_at: rec.settled_at.as_deref().map(parse_rfc3339),
    }
}

/// Parse a stored RFC3339 timestamp, falling back to now if a row somehow holds
/// an unparseable value (it never should — the store writes RFC3339).
fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Map a routing-registry violation onto the south-side error code + message.
/// The mismatch message carries the scheme's example address so the caller can
/// self-correct without consulting the registry.
fn routing_violation_message(v: RoutingViolation) -> (&'static str, String) {
    match v {
        RoutingViolation::UnknownScheme { field, scheme } => (
            "OBP-BANK-NODE-ROUTING-002",
            format!("unknown or inactive routing scheme '{scheme}' in {field}"),
        ),
        RoutingViolation::AddressMismatch {
            field,
            scheme,
            example_address,
        } => (
            "OBP-BANK-NODE-ROUTING-003",
            format!(
                "address in {field} does not match the pattern registered for \
                 scheme '{scheme}' (example: {example_address})"
            ),
        ),
    }
}

/// Map an Interface B failure onto a south-side error response. An OBP
/// business rejection passes through with its original status and OBP code
/// (the caller needs the real answer — e.g. a 403 missing role or a 404
/// unknown settlement); a transport failure is this node's 502.
fn obp_error(e: ObpClientError) -> Response {
    match e {
        ObpClientError::Rejected {
            status,
            error_code,
            message,
        } => error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            &error_code,
            message,
        ),
        ObpClientError::Transport(message) => error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-INTERFACE-B-001",
            format!("OBP-API unreachable or gave no authoritative answer: {message}"),
        ),
    }
}

/// Project a settlement-store row onto the read-API shape.
fn settlement_view(row: SettlementRow, finality_depth: u32) -> SettlementView {
    SettlementView {
        idempotency_key: row.idempotency_key,
        settlement_id: row.settlement_id,
        snapshot_id: row.snapshot_id,
        currency: row.currency,
        net_amount_minor: row.net_amount_minor,
        creditor_address: row.creditor_address,
        status: row.status,
        tx_id: row.tx_id,
        blockchain: row.blockchain,
        asset: row.asset,
        asset_amount: row.asset_amount,
        depth: row.last_depth,
        finality_depth,
        error_reason: row.error_reason,
        retryable: row.retryable,
        created_at: row.created_at,
        updated_at: row.updated_at,
        finalized_at: row.finalized_at,
    }
}

fn evidence_view(rec: EvidenceRecord) -> EvidenceView {
    EvidenceView {
        transaction_request_id: rec.transaction_request_id,
        promise_commitment: rec.promise_commitment,
        promise_salt: rec.promise_salt,
        promise_preimage: rec.promise_preimage,
        promise_id: rec.promise_id,
        promise_blockchain: rec.promise_blockchain,
        verified: rec.verified,
        currency: rec.currency,
        amount: rec.amount,
        originator_name: rec.originator_name,
        cbs_status: rec.cbs_status,
        cbs_reference: rec.cbs_reference,
        cbs_recorded_at: rec.cbs_recorded_at,
        received_at: rec.received_at,
        settlement_id: rec.settlement_id,
        settled_at: rec.settled_at,
    }
}

pub async fn list_settlements(State(state): State<BankNodeState>) -> Response {
    match state.settlements.list(LIST_LIMIT).await {
        Ok(rows) => {
            let body: Vec<_> = rows
                .into_iter()
                .map(|r| settlement_view(r, state.finality_depth))
                .collect();
            Json(body).into_response()
        }
        Err(e) => {
            error!(bank_id = %state.bank_id, error = %e, "settlement store list failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to list settlements",
            )
        }
    }
}

pub async fn get_settlement(
    State(state): State<BankNodeState>,
    Path(key): Path<String>,
) -> Response {
    match state.settlements.find(&key).await {
        Ok(Some(row)) => Json(settlement_view(row, state.finality_depth)).into_response(),
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            "OBP-BANK-NODE-NOT-FOUND-001",
            format!("no settlement with idempotency_key or settlement_id {key}"),
        ),
        Err(e) => {
            error!(%key, error = %e, "settlement store read failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to read settlement",
            )
        }
    }
}

/// `POST .../settlements` — trigger bilateral settlement for this node's own
/// corridor by calling OBP-API's settlement resource over Interface B. The
/// 201 relays OBP-API's answer (ledger netting done, value moves later on the
/// rail) plus how many local outbox rows the covered-promise list stamped.
pub async fn request_settlement(
    State(state): State<BankNodeState>,
    body: Result<Json<SettleRequest>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(json) => json,
        Err(rejection) => {
            warn!(error = %rejection, "request_settlement rejected malformed body");
            return error(StatusCode::BAD_REQUEST, "OBP-10001", rejection.body_text());
        }
    };
    if req.other_bank_id.trim().is_empty() || req.currency.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "OBP-10001",
            "other_bank_id and currency are required",
        );
    }
    if req.other_bank_id == state.bank_id {
        return error(
            StatusCode::BAD_REQUEST,
            "OBP-10001",
            "other_bank_id must differ from this node's own bank",
        );
    }

    let result = match state
        .obp
        .create_settlement(&state.bank_id, &req.other_bank_id, &req.currency)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(bank_id = %state.bank_id, other_bank_id = %req.other_bank_id, error = %e, "settle trigger failed at OBP-API");
            return obp_error(e);
        }
    };

    // Stamp the settlement linkage onto the covered local rows. A stamping
    // failure must not fail the request — the settlement already exists at
    // OBP-API; the linkage can be re-stamped from the corridor status poll.
    let stamped = match result.settlement_id.as_deref() {
        Some(sid) => match state
            .outbox
            .mark_settled(&result.covered_transaction_request_ids, sid)
            .await
        {
            Ok(n) => n,
            Err(e) => {
                error!(settlement_id = %sid, error = %e, "failed to stamp settlement linkage onto outbox rows");
                0
            }
        },
        None => 0,
    };

    info!(
        bank_id = %state.bank_id,
        other_bank_id = %req.other_bank_id,
        currency = %req.currency,
        settlement_id = ?result.settlement_id,
        covered = result.covered_transaction_request_ids.len(),
        stamped,
        "settlement created at OBP-API"
    );

    let mut body = result.raw;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("covered_outbox_rows_stamped".into(), stamped.into());
    }
    (StatusCode::CREATED, Json(body)).into_response()
}

/// `GET .../settlements/{key}/corridor` — the corridor-wide view: proxy of
/// OBP-API's settlement resource (ledger status + rail status + covered
/// promises + message delivery states). Also re-stamps the settlement linkage
/// onto local outbox rows, which is how a node that did NOT trigger the
/// settlement picks the linkage up.
pub async fn get_corridor_settlement(
    State(state): State<BankNodeState>,
    Path(key): Path<String>,
) -> Response {
    let result = match state.obp.get_settlement(&state.bank_id, &key).await {
        Ok(r) => r,
        Err(e) => return obp_error(e),
    };

    if let Some(sid) = result.settlement_id.as_deref() {
        if let Err(e) = state
            .outbox
            .mark_settled(&result.covered_transaction_request_ids, sid)
            .await
        {
            error!(settlement_id = %sid, error = %e, "failed to stamp settlement linkage onto outbox rows");
        }
    }

    Json(result.raw).into_response()
}

pub async fn list_evidence(State(state): State<BankNodeState>) -> Response {
    match state.evidence.list(LIST_LIMIT).await {
        Ok(recs) => {
            let body: Vec<_> = recs.into_iter().map(evidence_view).collect();
            Json(body).into_response()
        }
        Err(e) => {
            error!(bank_id = %state.bank_id, error = %e, "evidence store list failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to list evidence",
            )
        }
    }
}

pub async fn get_evidence(
    State(state): State<BankNodeState>,
    Path(transaction_request_id): Path<String>,
) -> Response {
    match state.evidence.get(&transaction_request_id).await {
        Ok(Some(rec)) => Json(evidence_view(rec)).into_response(),
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            "OBP-BANK-NODE-NOT-FOUND-001",
            format!("no evidence for transaction request {transaction_request_id}"),
        ),
        Err(e) => {
            error!(%transaction_request_id, error = %e, "evidence store read failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to read evidence",
            )
        }
    }
}

pub async fn root_health(State(state): State<BankNodeState>) -> Response {
    Json(HealthBody {
        status: "healthy",
        service: "OBP-Bank-Node",
        version: env!("CARGO_PKG_VERSION"),
        blockchain: state.blockchain_label,
        bank_id: state.bank_id.clone(),
        account_id: state.account_id.clone(),
        timestamp: Utc::now(),
    })
    .into_response()
}

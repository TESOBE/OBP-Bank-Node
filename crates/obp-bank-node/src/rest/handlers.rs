// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

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
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::types::*;
use super::{BankNodeState, HostState};
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

/// The OBP host named in a URL, if this node is on it; otherwise the 404 to answer with.
// The error side is a ready-made response; handlers return it as it is, so its size does not matter.
#[allow(clippy::result_large_err)]
fn known_host<'a>(state: &'a BankNodeState, obp_host: &str) -> Result<&'a HostState, Response> {
    // Look the OBP host up among the node's configured hosts.
    state.host(obp_host).ok_or_else(|| {
        // The node is not on that OBP host.
        error(
            StatusCode::NOT_FOUND,
            "OBP-BANK-NODE-NOT-FOUND-001",
            format!("this Bank Node is not on OBP host {obp_host}"),
        )
    })
}

/// The OBP host and bank named in a `/hosts/HOST/banks/BANK_ID/...` URL, when BANK_ID is this node's bank on that OBP host; otherwise the 404 to answer with.
// The error side is a ready-made response; handlers return it as it is, so its size does not matter.
#[allow(clippy::result_large_err)]
fn bank_on_host<'a>(state: &'a BankNodeState, obp_host: &str, bank_id: &str) -> Result<&'a HostState, Response> {
    // The OBP host first: an unknown one is its own 404.
    let host_state = known_host(state, obp_host)?;
    // The node serves one bank on each OBP host.
    if host_state.bank_id != bank_id {
        // Name the bank the node does serve there, so the caller can correct the URL.
        return Err(error(
            StatusCode::NOT_FOUND,
            "OBP-BANK-NODE-NOT-FOUND-001",
            format!(
                "on OBP host {obp_host} this Bank Node serves bank {}, not {bank_id}",
                host_state.bank_id
            ),
        ));
    }
    // This node's bank on this OBP host.
    Ok(host_state)
}

/// Validate an A1.1 request body. Mirrors the error table in `DOCS/A1_A2.md`:
///   - `OBP-40008` — amount is zero, negative, or not a number,
///   - `OBP-10001` — a required `value` / `to` field is empty,
///   - `OBP-BANK-NODE-ORIGINATOR-001` — an `originator` field is empty.
///
/// Returns the OBP error code and message on the first failure. Currency →
/// corridor-account resolution (`422 OBP-BANK-NODE-ROUTING-001`) needs state,
/// so it happens in `initiate_payment` after these stateless checks.
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

    // A payment carried on by a hub names the final bank completely.
    if let Some(final_bank_routing) = &req.final_bank_routing {
        // Both the scheme and the address are needed to find the bank.
        if final_bank_routing.scheme.trim().is_empty() || final_bank_routing.address.trim().is_empty() {
            return Err((
                BAD_REQUEST,
                "OBP-10001",
                "final_bank_routing.scheme and final_bank_routing.address are required when final_bank_routing is given".into(),
            ));
        }
        // A return goes back the way it came, never on through a hub.
        if req.return_of.is_some() {
            return Err((
                BAD_REQUEST,
                "OBP-10001",
                "a return (return_of) cannot have a final_bank_routing".into(),
            ));
        }
    }

    Ok(())
}

/// The header a caller sends to make a `POST .../transaction-requests` safe
/// to repeat: the same key with the same body returns the first request's
/// 202 again (with `Idempotency-Replayed: true`); the same key with a
/// different body is refused.
pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const IDEMPOTENCY_KEY_MAX_LEN: usize = 255;

pub async fn initiate_payment(
    State(state): State<BankNodeState>,
    Path((obp_host, bank_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<InitiateRequest>, JsonRejection>,
) -> Response {
    // The OBP host to make the payment on, and this node's bank there.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // The node's bank on that OBP host.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    let idempotency_key = match headers.get(IDEMPOTENCY_KEY_HEADER).map(|v| v.to_str()) {
        None => None,
        Some(Ok(k)) if !k.trim().is_empty() && k.len() <= IDEMPOTENCY_KEY_MAX_LEN => Some(k.trim().to_string()),
        Some(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "OBP-10001",
                format!("Idempotency-Key must be 1 to {IDEMPOTENCY_KEY_MAX_LEN} printable characters"),
            )
        }
    };
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
        // The routing schemes registered on this OBP host.
        if let Err(v) = host_state.routing.check(field, scheme, address) {
            let (code, message) = routing_violation_message(v);
            warn!(%code, %message, "initiate_payment rejected beneficiary routing");
            return error(StatusCode::BAD_REQUEST, code, message);
        }
    }

    // Which corridor account sends this currency. With per-currency accounts
    // configured, an unmapped currency is refused here, synchronously — not
    // later in the Promise step as OBP-API's OBP-40003 currency mismatch.
    // The corridor accounts are this bank's on this OBP host.
    let Some(account_id) = host_state.corridor_account_for(&req.value.currency) else {
        let message = format!(
            "no corridor account is configured for currency {}",
            req.value.currency.trim().to_uppercase()
        );
        warn!(%message, "initiate_payment rejected unresolvable currency");
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OBP-BANK-NODE-ROUTING-001",
            message,
        );
    };
    let account_id = account_id.to_string();

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

    // A repeated request: answer from the row the key already names.
    if let Some(key) = idempotency_key.as_deref() {
        match state.outbox.get_by_idempotency_key(key).await {
            Ok(Some(existing)) => return replay_or_refuse(host_state, existing, &request_payload),
            Ok(None) => {}
            Err(e) => {
                error!(error = %e, "failed to look up the idempotency key");
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "OBP-BANK-NODE-PLATFORM-001",
                    "failed to persist transaction request",
                );
            }
        }
    }

    // Persist to the outbox BEFORE returning — this is what makes the 202
    // durable. The dispatcher takes it from here (OBP-API submit, Promise
    // write). A 128-bit salt is minted now for the eventual Promise commitment.
    let salt = Uuid::new_v4().simple().to_string();
    let entry = NewEntry {
        transaction_request_id: &id,
        // The OBP host the payment is made on.
        obp_host: &host_state.host,
        bank_id: &host_state.bank_id,
        account_id: &account_id,
        request_payload: &request_payload,
        commitment_salt: &salt,
    };
    let inserted = match idempotency_key.as_deref() {
        Some(key) => state.outbox.insert_with_idempotency_key(entry, key).await,
        None => state.outbox.insert(entry).await,
    };
    match inserted {
        Ok(()) => {}
        // Two identical requests raced: the other one won; answer from its row.
        Err(crate::outbox::OutboxError::DuplicateIdempotencyKey) => {
            let key = idempotency_key.as_deref().unwrap_or_default();
            return match state.outbox.get_by_idempotency_key(key).await {
                Ok(Some(existing)) => replay_or_refuse(host_state, existing, &request_payload),
                _ => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "OBP-BANK-NODE-PLATFORM-001",
                    "failed to persist transaction request",
                ),
            };
        }
        Err(e) => {
            error!(transaction_request_id = %id, error = %e, "failed to persist to outbox");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to persist transaction request",
            );
        }
    }

    info!(
        obp_host = %host_state.host,
        bank_id = %host_state.bank_id,
        account_id = %account_id,
        transaction_request_id = %id,
        currency = %req.value.currency,
        amount = %req.value.amount,
        scheme = %req.to.other_bank_routing_scheme,
        "initiate_payment persisted to outbox (INITIATED) — dispatcher will submit"
    );

    // Echo the caller's inline routing and originator back in the 202. The
    // promise_id stays null here; it is filled asynchronously once the
    // dispatcher writes the Promise commitment (queryable via the status route).
    (StatusCode::ACCEPTED, Json(initiated_response(&host_state.bank_id, id, account_id, req, Utc::now()))).into_response()
}

/// The 202 body for a freshly accepted (or replayed) request.
fn initiated_response(
    bank_id: &str,
    id: String,
    account_id: String,
    req: InitiateRequest,
    start_date: DateTime<Utc>,
) -> InitiatedResponse {
    let mut originator = req.originator;
    originator.source = Some("explicit".into());
    InitiatedResponse {
        transaction_request_id: id,
        kind: "OPEN_CORRIDOR_PROMISE",
        from: FromAccount {
            // This node's bank on the OBP host the payment is made on.
            bank_id: bank_id.to_string(),
            account_id,
        },
        to: req.to,
        originator,
        value: req.value,
        description: req.description,
        status: "INITIATED",
        promise_id: None,
        promise_blockchain: None,
        start_date,
        challenge: None,
    }
}

/// Answer a repeated `Idempotency-Key`: the same body gets the original
/// request back (its id, with the current status), a different body is refused.
fn replay_or_refuse(host_state: &HostState, existing: OutboxRecord, request_payload: &str) -> Response {
    // A key already used for a payment on another OBP host, or from another bank, is not a repeat of this request.
    if existing.obp_host != host_state.host || existing.bank_id != host_state.bank_id {
        // Say where the key was used.
        warn!(transaction_request_id = %existing.transaction_request_id, "Idempotency-Key reused for another OBP host or bank");
        // Refuse, as for a different body.
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OBP-BANK-NODE-IDEMPOTENCY-001",
            "this Idempotency-Key was already used for a payment from another bank or on another OBP host",
        );
    }
    if existing.request_payload != request_payload {
        warn!(transaction_request_id = %existing.transaction_request_id, "Idempotency-Key reused with a different body");
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OBP-BANK-NODE-IDEMPOTENCY-001",
            "this Idempotency-Key was already used with a different request body",
        );
    }
    let req: InitiateRequest = match serde_json::from_str(&existing.request_payload) {
        Ok(r) => r,
        Err(_) => return Json(status_from_record(existing)).into_response(),
    };
    let mut body = initiated_response(
        &host_state.bank_id,
        existing.transaction_request_id.clone(),
        existing.account_id.clone(),
        req,
        parse_rfc3339(&existing.created_at),
    );
    body.status = status_label(&existing.status);
    body.promise_id = existing.promise_tx_id.clone();
    body.promise_blockchain = existing.promise_blockchain.clone();
    (
        StatusCode::ACCEPTED,
        [("idempotency-replayed", "true")],
        Json(body),
    )
        .into_response()
}

/// The outbox status as a `'static` label for the 202 body.
fn status_label(status: &str) -> &'static str {
    use crate::outbox::status as s;
    match status {
        s::INITIATED => "INITIATED",
        s::SUBMITTED => "SUBMITTED",
        s::PROMISE_WRITTEN => "PROMISE_WRITTEN",
        s::REPORTED => "REPORTED",
        s::EXCEPTION => "EXCEPTION",
        _ => "INITIATED",
    }
}

pub async fn get_transaction_request(
    State(state): State<BankNodeState>,
    Path((obp_host, bank_id, transaction_request_id)): Path<(String, String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    match state.outbox.get(&transaction_request_id).await {
        // Only a payment made by this bank on this OBP host is shown here.
        Ok(Some(rec)) if rec.obp_host == host_state.host && rec.bank_id == host_state.bank_id => {
            // The payment's status.
            Json(status_from_record(rec)).into_response()
        }
        // No such payment, or one made elsewhere.
        Ok(_) => error(
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

pub async fn list_transaction_requests(
    State(state): State<BankNodeState>,
    Path((obp_host, bank_id)): Path<(String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    // Only the payments this bank made on this OBP host.
    match state.outbox.list(&host_state.host, &host_state.bank_id, LIST_LIMIT).await {
        Ok(recs) => {
            let body: Vec<_> = recs.into_iter().map(status_from_record).collect();
            Json(body).into_response()
        }
        Err(e) => {
            error!(obp_host = %host_state.host, bank_id = %host_state.bank_id, error = %e, "outbox list failed");
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
        attempt_count: rec.attempt_count,
        last_attempted_at: rec.last_attempted_at.as_deref().map(parse_rfc3339),
        last_error: rec.last_error,
        exception_reason: rec.exception_reason,
        exception_id: rec.exception_tx_id,
        exception_blockchain: rec.exception_blockchain,
        obp_status: rec.obp_status,
        obp_status_at: rec.obp_status_at.as_deref().map(parse_rfc3339),
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
        fx_minor_per_whole_asset: row.fx_minor_per_whole_asset,
        fx_source: row.fx_source,
        fx_as_of: row.fx_as_of,
        purpose: row.purpose,
        depth: row.last_depth,
        finality_depth,
        error_reason: row.error_reason,
        retryable: row.retryable,
        created_at: row.created_at,
        updated_at: row.updated_at,
        finalized_at: row.finalized_at,
        record_tx_id: row.record_tx_id,
        record_blockchain: row.record_blockchain,
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
        beneficiary_name: rec.beneficiary_name,
        beneficiary_account_routing_scheme: rec.beneficiary_account_routing_scheme,
        beneficiary_account_routing_address: rec.beneficiary_account_routing_address,
        return_of_transaction_request_id: rec.return_of_transaction_request_id,
        returned_by_transaction_request_id: rec.returned_by_transaction_request_id,
        // The second leg, when this bank carried the credit on.
        forwarded_as_transaction_request_id: rec.forwarded_as_transaction_request_id,
        forwarded_on_obp_host: rec.forwarded_on_obp_host,
        cbs_status: rec.cbs_status,
        cbs_reference: rec.cbs_reference,
        cbs_recorded_at: rec.cbs_recorded_at,
        received_at: rec.received_at,
        settlement_id: rec.settlement_id,
        settled_at: rec.settled_at,
    }
}

pub async fn list_settlements(
    State(state): State<BankNodeState>,
    Path((obp_host, bank_id)): Path<(String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    // Only this bank's settlements on this OBP host.
    match state.settlements.list(&host_state.host, &host_state.bank_id, LIST_LIMIT).await {
        Ok(rows) => {
            let body: Vec<_> = rows
                .into_iter()
                .map(|r| settlement_view(r, state.finality_depth))
                .collect();
            Json(body).into_response()
        }
        Err(e) => {
            error!(obp_host = %host_state.host, bank_id = %host_state.bank_id, error = %e, "settlement store list failed");
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
    Path((obp_host, bank_id, key)): Path<(String, String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    // Only this bank's settlements on this OBP host are found.
    match state.settlements.find(&host_state.host, &host_state.bank_id, &key).await {
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
    Path((obp_host, bank_id)): Path<(String, String)>,
    body: Result<Json<SettleRequest>, JsonRejection>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
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
    if req.other_bank_id == host_state.bank_id {
        return error(
            StatusCode::BAD_REQUEST,
            "OBP-10001",
            "other_bank_id must differ from this node's own bank",
        );
    }

    // Settle on this OBP host's OBP-API.
    let result = match host_state
        .obp
        .create_settlement(&host_state.bank_id, &req.other_bank_id, &req.currency)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(obp_host = %host_state.host, bank_id = %host_state.bank_id, other_bank_id = %req.other_bank_id, error = %e, "settle trigger failed at OBP-API");
            return obp_error(e);
        }
    };

    // Stamp the settlement linkage onto the covered local rows. A stamping
    // failure must not fail the request — the settlement already exists at
    // OBP-API; the linkage can be re-stamped from the corridor status poll.
    let stamped = match result.settlement_id.as_deref() {
        Some(sid) => match state
            .outbox
            // Only the payments made on this OBP host.
            .mark_settled(&host_state.host, &result.covered_transaction_request_ids, sid)
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
        obp_host = %host_state.host,
        bank_id = %host_state.bank_id,
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
    Path((obp_host, bank_id, key)): Path<(String, String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    // Ask this OBP host's OBP-API.
    let result = match host_state.obp.get_settlement(&host_state.bank_id, &key).await {
        Ok(r) => r,
        Err(e) => return obp_error(e),
    };

    if let Some(sid) = result.settlement_id.as_deref() {
        if let Err(e) = state
            .outbox
            // Only the payments made on this OBP host.
            .mark_settled(&host_state.host, &result.covered_transaction_request_ids, sid)
            .await
        {
            error!(settlement_id = %sid, error = %e, "failed to stamp settlement linkage onto outbox rows");
        }
    }

    Json(result.raw).into_response()
}

/// The banks on this OBP host, as OBP-API's getBanks lists them
/// (`GET /obp/v6.0.0/banks`), with each bank's `bank_routings` and active `attributes`. This is how
/// the bank, or the Bank Node UI, learns which other banks it can pay and
/// which routing pairs address them. The body is OBP-API's, unchanged.
pub async fn list_banks(
    State(state): State<BankNodeState>,
    Path(obp_host): Path<String>,
) -> Response {
    // The OBP host named in the URL.
    let host_state = match known_host(&state, &obp_host) {
        // The node is on it.
        Ok(host_state) => host_state,
        // The node is not on it.
        Err(response) => return response,
    };
    // Ask that OBP host's OBP-API for the list; a failure maps onto the usual Interface B errors.
    match host_state.obp.get_banks().await {
        // Relay OBP-API's body as it is.
        Ok(body) => Json(body).into_response(),
        // A rejection keeps OBP-API's status and code; no answer is this node's 502.
        Err(e) => obp_error(e),
    }
}

/// Settlements this bank receives as creditor, and whether each payment has
/// been seen arriving (see `incoming.rs`).
pub async fn list_incoming_settlements(
    State(state): State<BankNodeState>,
    Path((obp_host, bank_id)): Path<(String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    // Only the settlements this bank receives on this OBP host.
    match state.settlements.list_incoming(&host_state.host, &host_state.bank_id, LIST_LIMIT).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            error!(obp_host = %host_state.host, bank_id = %host_state.bank_id, error = %e, "incoming settlements list failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OBP-BANK-NODE-PLATFORM-001",
                "failed to list incoming settlements",
            )
        }
    }
}

pub async fn list_evidence(
    State(state): State<BankNodeState>,
    Path((obp_host, bank_id)): Path<(String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    // Only the credits this bank received on this OBP host.
    match state.evidence.list(&host_state.host, &host_state.bank_id, LIST_LIMIT).await {
        Ok(recs) => {
            let body: Vec<_> = recs.into_iter().map(evidence_view).collect();
            Json(body).into_response()
        }
        Err(e) => {
            error!(obp_host = %host_state.host, bank_id = %host_state.bank_id, error = %e, "evidence store list failed");
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
    Path((obp_host, bank_id, transaction_request_id)): Path<(String, String, String)>,
) -> Response {
    // This node's bank on the OBP host named in the URL.
    let host_state = match bank_on_host(&state, &obp_host, &bank_id) {
        // Found.
        Ok(host_state) => host_state,
        // Not this node's bank, or not an OBP host it is on.
        Err(response) => return response,
    };
    match state.evidence.get(&transaction_request_id).await {
        // Only a credit this bank received on this OBP host is shown here.
        Ok(Some(rec)) if rec.obp_host == host_state.host && rec.bank_id == host_state.bank_id => {
            // The credit.
            Json(evidence_view(rec)).into_response()
        }
        // No such credit, or one received elsewhere.
        Ok(_) => error(
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

/// Each OBP host the node is on, with the node's bank there, as `/status` and `/obp-bank-node/v5.1.0/hosts` report it.
fn host_views(state: &BankNodeState) -> Vec<HostView> {
    // One view per configured OBP host, in config order.
    state
        .hosts
        .iter()
        .map(|host_state| {
            // Interface C state on this OBP host.
            let (interface_c, interface_c_detail) = host_state.interface_c.snapshot();
            // The view.
            HostView {
                // The OBP host.
                host: host_state.host.clone(),
                // Its OBP-API base URL.
                obp_api_url: host_state.obp_api_url.clone(),
                // This bank's id there.
                bank_id: host_state.bank_id.clone(),
                // Its shorthand account there.
                account_id: host_state.account_id.clone(),
                // Its corridor accounts there, sorted so the JSON is stable.
                corridor_accounts: host_state
                    .corridor_accounts
                    .iter()
                    .map(|(currency, account)| (currency.clone(), account.clone()))
                    .collect(),
                // Its Interface C state.
                interface_c,
                // Why Interface C is not connected, when it is not.
                interface_c_detail,
            }
        })
        .collect()
}

/// `GET /obp-bank-node/v5.1.0/hosts` — the OBP hosts this node is on, with its bank on each. The URLs of everything else name one of these: `/obp-bank-node/v5.1.0/hosts/HOST/banks/BANK_ID/...`.
pub async fn list_hosts(State(state): State<BankNodeState>) -> Response {
    // The node's OBP hosts.
    Json(HostsBody { hosts: host_views(&state) }).into_response()
}

/// `/obp-bank-node/v5.1.0/status` — configuration and identity detail, read by the Bank Node apps. Kept separate from `/obp-bank-node/v5.1.0/health`, which stays minimal. The root `/alive`, `/health` and `/status` follow OBP-API's format (`status_page`).
pub async fn root_status(State(state): State<BankNodeState>) -> Response {
    Json(StatusBody {
        service: "OBP-Bank-Node",
        // The node's name.
        name: state.name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        blockchain: state.blockchain_label,
        finality_depth: state.finality_depth,
        // Each OBP host the node is on, with its bank there.
        hosts: host_views(&state),
        timestamp: Utc::now(),
    })
    .into_response()
}

/// Interface C across the node's OBP hosts: `disabled` when it is off on every OBP host, `connected` when every OBP host that has it on is connected, and otherwise the first OBP host's state that is neither, with that OBP host named in the detail.
fn overall_interface_c(state: &BankNodeState) -> (&'static str, Option<String>) {
    // Each OBP host's state.
    let snapshots: Vec<(&str, &'static str, Option<String>)> = state
        .hosts
        .iter()
        .map(|host_state| {
            // The OBP host's state and detail.
            let (interface_c, detail) = host_state.interface_c.snapshot();
            // Keep the OBP host's name with it.
            (host_state.host.as_str(), interface_c, detail)
        })
        .collect();
    // An OBP host whose consumer is neither connected nor deliberately off.
    if let Some((obp_host, interface_c, detail)) = snapshots
        .iter()
        .find(|(_, interface_c, _)| !matches!(*interface_c, "connected" | "disabled"))
    {
        // Name the OBP host, and add its detail when there is one.
        let detail = match detail {
            // The detail the consumer gave.
            Some(detail) => format!("OBP host {obp_host}: {detail}"),
            // Just the OBP host.
            None => format!("OBP host {obp_host}"),
        };
        // That OBP host's state stands for the node.
        return (interface_c, Some(detail));
    }
    // Off everywhere.
    if snapshots.iter().all(|(_, interface_c, _)| *interface_c == "disabled") {
        // Nothing to say.
        return ("disabled", None);
    }
    // Every OBP host with it on is connected.
    ("connected", None)
}

pub async fn root_health(State(state): State<BankNodeState>) -> Response {
    // Interface C across the node's OBP hosts.
    let (interface_c, interface_c_detail) = overall_interface_c(&state);
    Json(HealthBody {
        status: "healthy",
        service: "OBP-Bank-Node",
        version: env!("CARGO_PKG_VERSION"),
        blockchain: state.blockchain_label,
        interface_c,
        interface_c_detail,
        timestamp: Utc::now(),
    })
    .into_response()
}

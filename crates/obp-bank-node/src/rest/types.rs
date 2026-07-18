//! Request / response types for the south-side REST API.
//!
//! Field names mirror the OBP API surface so banks already integrated with
//! OBP can reuse their existing payload shapes unchanged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Body of `POST .../transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests`.
///
/// Also `Serialize`: the handler re-serializes the validated request into the
/// outbox as the canonical payload the dispatcher replays to OBP-API.
#[derive(Debug, Deserialize, Serialize)]
pub struct InitiateRequest {
    pub value: MoneyValue,
    pub description: String,
    pub to: BeneficiaryRouting,
    /// FATF Travel Rule data on the upstream payer. Mandatory for OPEN_CORRIDOR_PROMISE.
    pub originator: Originator,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MoneyValue {
    pub currency: String,
    pub amount: String,
}

/// Inline beneficiary routing — no pre-registered counterparty required.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BeneficiaryRouting {
    pub other_bank_routing_scheme: String,
    pub other_bank_routing_address: String,
    pub other_account_routing_scheme: String,
    pub other_account_routing_address: String,
}

/// FATF Travel Rule (Recommendation 16) data on the actual payer upstream of
/// the bank's settlement account — i.e. the bank's customer.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Originator {
    pub name: String,
    pub address: String,
    pub account_routing: AccountRouting,
    /// `"explicit"` on responses; absent on the inbound request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AccountRouting {
    pub scheme: String,
    pub address: String,
}

/// HTTP 202 body returned from payment initiation. Mirrors the OBP
/// OPEN_CORRIDOR_PROMISE Transaction Request response shape (see `DOCS/A1_A2.md`).
#[derive(Debug, Serialize)]
pub struct InitiatedResponse {
    pub transaction_request_id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub from: FromAccount,
    pub to: BeneficiaryRouting,
    pub originator: Originator,
    pub value: MoneyValue,
    pub description: String,
    pub status: &'static str,
    pub promise_id: Option<String>,
    pub promise_blockchain: Option<String>,
    pub start_date: DateTime<Utc>,
    /// Always null — the OBP Bank Node does not trigger SCA challenges.
    pub challenge: Option<()>,
}

#[derive(Debug, Serialize)]
pub struct FromAccount {
    pub bank_id: String,
    pub account_id: String,
}

/// Body of `GET .../transaction-requests/{id}`.
#[derive(Debug, Serialize)]
pub struct TransactionRequestStatus {
    pub transaction_request_id: String,
    pub status: String,
    pub promise_id: Option<String>,
    pub promise_blockchain: Option<String>,
    pub netting_snapshot_id: Option<String>,
    pub netting_blockchain: Option<String>,
    pub settlement_id: Option<String>,
    pub settlement_system: Option<String>,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct HealthBody {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
    pub blockchain: &'static str,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error_code: String,
    pub message: String,
}

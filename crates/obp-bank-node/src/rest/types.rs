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
    /// OBP charge policy (`SHARED` / `SENDER` / `RECEIVER`). Part of the OBP
    /// wire body; defaults to `SHARED` when the A1.1 caller omits it.
    #[serde(default = "default_charge_policy")]
    pub charge_policy: String,
    /// Set when this payment is a RETURN of an earlier inbound promise whose
    /// credit this bank's CBS refused: the original transaction_request_id.
    /// Omitted from the wire when absent so ordinary payloads are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_of: Option<String>,
}

fn default_charge_policy() -> String {
    "SHARED".to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MoneyValue {
    pub currency: String,
    pub amount: String,
}

/// Inline beneficiary routing — no pre-registered counterparty required.
///
/// Mirrors OBP-API's `PostSimpleCounterpartyJson400` field-for-field: the
/// dispatcher replays this block verbatim on Interface B, and OBP-API's JSON
/// extraction has no defaults, so every field must be present on the wire.
/// Callers may omit the optional ones (serde fills empty strings).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BeneficiaryRouting {
    /// Beneficiary display name — used by OBP-API for counterparty creation.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub other_bank_routing_scheme: String,
    pub other_bank_routing_address: String,
    pub other_account_routing_scheme: String,
    pub other_account_routing_address: String,
    #[serde(default)]
    pub other_account_secondary_routing_scheme: String,
    #[serde(default)]
    pub other_account_secondary_routing_address: String,
    #[serde(default)]
    pub other_branch_routing_scheme: String,
    #[serde(default)]
    pub other_branch_routing_address: String,
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
    /// Requested amount, projected from the stored A1.1 payload. `None` only
    /// for rows whose payload no longer parses.
    pub value: Option<MoneyValue>,
    /// The beneficiary bank (`to.other_bank_routing_address`) — the far side
    /// of this outbound leg's corridor.
    pub other_bank_id: Option<String>,
    pub description: Option<String>,
    pub promise_id: Option<String>,
    pub promise_blockchain: Option<String>,
    pub netting_snapshot_id: Option<String>,
    pub netting_blockchain: Option<String>,
    pub settlement_id: Option<String>,
    pub settlement_system: Option<String>,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

/// Body of `POST .../settlements` — ask this node to trigger bilateral Open
/// Corridor settlement between its own bank and `other_bank_id`, by
/// calling OBP-API's settlement resource over Interface B.
#[derive(Debug, Deserialize)]
pub struct SettleRequest {
    pub other_bank_id: String,
    pub currency: String,
}

/// One row of the node's local (rail-side) settlement store, as returned by
/// `GET .../settlements[/{key}]`. This is the debtor node's own record of the
/// value leg; the corridor-wide ledger view lives behind
/// `GET .../settlements/{key}/corridor`.
#[derive(Debug, Serialize)]
pub struct SettlementView {
    pub idempotency_key: String,
    pub settlement_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub currency: String,
    /// Net owed in minor units (string — may exceed u64).
    pub net_amount_minor: String,
    pub creditor_address: String,
    /// `SETTLING` / `SUBMITTED` / `FINAL` / `ERROR`.
    pub status: String,
    pub tx_id: Option<String>,
    pub blockchain: Option<String>,
    pub asset: Option<String>,
    pub asset_amount: Option<String>,
    /// The settle-time rate the transfer was sized with: minor units of
    /// `currency` per whole `asset`, the source label, and the quote time.
    pub fx_minor_per_whole_asset: Option<String>,
    pub fx_source: Option<String>,
    pub fx_as_of: Option<String>,
    /// Last confirmation depth observed by the finality watcher.
    pub depth: i64,
    /// Depth at which this node promotes a settlement to `FINAL`.
    pub finality_depth: u32,
    pub error_reason: Option<String>,
    pub retryable: bool,
    pub created_at: String,
    pub updated_at: String,
    pub finalized_at: Option<String>,
}

/// One row of the beneficiary-side evidence store, as returned by
/// `GET .../evidence[/{transaction_request_id}]`: the commit–reveal triplet,
/// whether it verified at receive time, and the A2 CBS delivery outcome.
#[derive(Debug, Serialize)]
pub struct EvidenceView {
    pub transaction_request_id: String,
    pub promise_commitment: String,
    pub promise_salt: String,
    pub promise_preimage: String,
    pub promise_id: Option<String>,
    pub promise_blockchain: Option<String>,
    pub verified: bool,
    pub currency: Option<String>,
    pub amount: Option<String>,
    pub originator_name: Option<String>,
    /// Whom the CBS was asked to credit; absent on rows predating the
    /// notification's `beneficiary` block.
    pub beneficiary_name: Option<String>,
    pub beneficiary_account_routing_scheme: Option<String>,
    pub beneficiary_account_routing_address: Option<String>,
    /// Set when this credit IS a return (the original promise it repays) /
    /// when this credit was rejected and a return was initiated for it.
    pub return_of_transaction_request_id: Option<String>,
    pub returned_by_transaction_request_id: Option<String>,
    pub cbs_status: Option<String>,
    pub cbs_reference: Option<String>,
    pub cbs_recorded_at: Option<String>,
    pub received_at: String,
    pub settlement_id: Option<String>,
    pub settled_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HealthBody {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
    pub blockchain: &'static str,
    /// This node's own bank and corridor account — lets UIs derive
    /// per-node beneficiary defaults instead of hardcoding them.
    pub bank_id: String,
    pub account_id: String,
    /// Interface C consumer state: `connected` / `connecting` /
    /// `reconnecting` / `disabled`. Anything but `connected` means the node
    /// cannot receive settlement instructions or credit notifications.
    pub interface_c: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_c_detail: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error_code: String,
    pub message: String,
}

//! Request / response types for the south-side REST API.
//!
//! Field names mirror the OBP API surface so banks already integrated with
//! OBP can reuse their existing payload shapes unchanged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Body of `POST .../transaction-request-types/SIMPLE/transaction-requests`.
#[derive(Debug, Deserialize)]
pub struct InitiateRequest {
    pub value: MoneyValue,
    pub description: String,
    pub to: BeneficiaryRouting,
    #[serde(default = "default_charge_policy")]
    pub charge_policy: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MoneyValue {
    pub currency: String,
    pub amount: String,
}

/// Inline beneficiary routing — no pre-registered counterparty required.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BeneficiaryRouting {
    #[serde(rename = "otherBankRoutingScheme")]
    pub other_bank_routing_scheme: String,
    #[serde(rename = "otherBankRoutingAddress")]
    pub other_bank_routing_address: String,
    #[serde(rename = "otherAccountRoutingScheme")]
    pub other_account_routing_scheme: String,
    #[serde(rename = "otherAccountRoutingAddress")]
    pub other_account_routing_address: String,
}

fn default_charge_policy() -> String {
    "SHARED".to_string()
}

/// HTTP 202 body returned from payment initiation.
#[derive(Debug, Serialize)]
pub struct InitiatedResponse {
    pub transaction_request_id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub from: FromAccount,
    pub to: ToCounterparty,
    pub value: MoneyValue,
    pub description: String,
    pub status: &'static str,
    pub promise_id: Option<String>,
    pub start_date: DateTime<Utc>,
    pub end_date: Option<DateTime<Utc>>,
    /// Always null — the OBP Bank Node does not trigger SCA challenges.
    pub challenge: Option<()>,
}

#[derive(Debug, Serialize)]
pub struct FromAccount {
    pub bank_id: String,
    pub account_id: String,
}

#[derive(Debug, Serialize)]
pub struct ToCounterparty {
    pub counterparty_id: String,
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

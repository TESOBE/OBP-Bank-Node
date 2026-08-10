//! Wire types for Interface C — the inbound RabbitMQ channel from OBP-API.
//!
//! Two conventions live here, deliberately:
//!   - The **envelope** ([`ReplyEnvelope`]) uses OBP's connector camelCase
//!     (`inboundAdapterCallContext`, `correlationId`, `errorCode`).
//!   - The **business bodies** use `lower_snake_case`, like the rest of the OBP
//!     payload surface.

use serde::{Deserialize, Serialize};

/// AMQP `MessageId` values the Bank Node dispatches on.
pub mod message_id {
    pub const CREDIT_NOTIFICATION: &str = "obp_credit_notification";
    pub const NETTING_SNAPSHOT: &str = "obp_netting_snapshot";
    pub const SETTLEMENT_ADVICE: &str = "obp_settlement_advice";
    pub const SETTLEMENT_INSTRUCTION: &str = "obp_settlement_instruction";
    pub const STATUS_UPDATE: &str = "obp_status_update";
}

/// OBP error codes the Bank Node returns on this interface.
pub mod error_code {
    /// Unrecognised `MessageId`.
    pub const NOT_IMPLEMENTED: &str = "OBP-BANK-NODE-NOT-IMPLEMENTED";
    /// A credit notification whose revealed salt+preimage don't hash to the commitment.
    pub const COMMITMENT_MISMATCH: &str = "OBP-BANK-NODE-COMMITMENT-MISMATCH";
    /// The credit could not be delivered to the bank's CBS (transient:
    /// transport error or CBS 5xx — OBP-API's outbox retries it).
    pub const CBS_DELIVERY_FAILED: &str = "OBP-BANK-NODE-CBS-DELIVERY-FAILED";
    /// The CBS answered and refused the credit (4xx: unknown account, name
    /// mismatch, …). Permanent — OBP-API parks the outbox row STICKY for an
    /// operator instead of retrying.
    pub const CBS_REJECTED: &str = "OBP-BANK-NODE-CBS-REJECTED";
    /// The message body could not be parsed.
    pub const BAD_MESSAGE: &str = "OBP-BANK-NODE-BAD-MESSAGE";
    /// A local persistence error while handling the message.
    pub const PLATFORM: &str = "OBP-BANK-NODE-PLATFORM-001";
    /// The on-chain settlement transfer failed.
    pub const SETTLEMENT_FAILED: &str = "OBP-BANK-NODE-SETTLEMENT-FAILED";
    /// A settlement instruction arrived but no settlement backend is configured.
    pub const SETTLEMENT_NOT_CONFIGURED: &str = "OBP-BANK-NODE-SETTLEMENT-NOT-CONFIGURED";
}

/// The OBP inbound-envelope reply published back to `replyTo`.
#[derive(Debug, Serialize)]
pub struct ReplyEnvelope {
    #[serde(rename = "inboundAdapterCallContext")]
    pub inbound_adapter_call_context: InboundCallContext,
    pub status: ReplyStatus,
    pub data: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct InboundCallContext {
    #[serde(rename = "correlationId")]
    pub correlation_id: String,
}

#[derive(Debug, Serialize)]
pub struct ReplyStatus {
    #[serde(rename = "errorCode")]
    pub error_code: String,
    #[serde(rename = "backendMessages")]
    pub backend_messages: Vec<String>,
}

impl ReplyEnvelope {
    /// A success reply (`errorCode = ""`) carrying `data`.
    pub fn ok(correlation_id: &str, data: serde_json::Value) -> Self {
        Self {
            inbound_adapter_call_context: InboundCallContext {
                correlation_id: correlation_id.to_string(),
            },
            status: ReplyStatus {
                error_code: String::new(),
                backend_messages: Vec::new(),
            },
            data,
        }
    }

    /// An error reply carrying an OBP `errorCode` and a human-readable message.
    pub fn error(correlation_id: &str, error_code: &str, message: impl Into<String>) -> Self {
        Self {
            inbound_adapter_call_context: InboundCallContext {
                correlation_id: correlation_id.to_string(),
            },
            status: ReplyStatus {
                error_code: error_code.to_string(),
                backend_messages: vec![message.into()],
            },
            data: serde_json::Value::Null,
        }
    }

    #[allow(dead_code)] // used in tests; handy for callers inspecting replies
    pub fn is_ok(&self) -> bool {
        self.status.error_code.is_empty()
    }
}

#[derive(Debug, Deserialize)]
pub struct MoneyValue {
    pub currency: String,
    pub amount: String,
}

// These mirror the inbound wire shape; not every field is consumed yet (the
// unread ones are surfaced for evidence/audit and future handlers).
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct Originator {
    pub name: String,
    #[serde(default)]
    pub address: Option<String>,
}

/// The customer the beneficiary bank's CBS is asked to credit.
#[derive(Debug, Deserialize)]
pub struct Beneficiary {
    pub name: String,
    pub account_routing: BeneficiaryAccountRouting,
}

#[derive(Debug, Deserialize)]
pub struct BeneficiaryAccountRouting {
    pub scheme: String,
    pub address: String,
}

/// `obp_credit_notification` — "credit your customer". Extended (vs the stock
/// OBP shape) with the three evidence fields that carry the commit–reveal data
/// to Bank B: `promise_commitment`, `promise_salt`, `promise_preimage`.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct CreditNotification {
    pub transaction_request_id: String,
    pub value: MoneyValue,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub originator: Option<Originator>,
    /// The customer the CBS is asked to credit. `None` only for messages
    /// predating the field (added 2026-08-10).
    #[serde(default)]
    pub beneficiary: Option<Beneficiary>,
    #[serde(default)]
    pub netting_snapshot_id: Option<String>,
    /// On-chain reference to Bank A's Promise (the tx id) and its chain.
    #[serde(default)]
    pub promise_id: Option<String>,
    #[serde(default)]
    pub promise_blockchain: Option<String>,
    /// Evidence triplet — present when salt delivery is in effect.
    #[serde(default)]
    pub promise_commitment: Option<String>,
    #[serde(default)]
    pub promise_salt: Option<String>,
    /// The exact canonical instruction bytes that were hashed (the preimage).
    #[serde(default)]
    pub promise_preimage: Option<String>,
}

/// `obp_settlement_advice` — "the promises you already paid out against are now
/// covered". Purely reconciliatory: stamps the listed credits settled; no money
/// moves on this message. Credit notifications themselves arrive at promise
/// report-back time.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct SettlementAdvice {
    pub settlement_id: String,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub net_amount: Option<String>,
    #[serde(default)]
    pub debtor_bank_id: Option<String>,
    #[serde(default)]
    pub creditor_bank_id: Option<String>,
    #[serde(default)]
    pub covered_transaction_request_ids: Vec<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// `obp_settlement_instruction` — "settle this now". Most fields feed the
/// not-yet-wired settlement trigger (see the router seam).
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct SettlementInstruction {
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub settlement_id: Option<String>,
    #[serde(default)]
    pub settlement_system: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    /// Net owed, in major units (e.g. `"25000.00"`).
    #[serde(default)]
    pub amount: Option<String>,
    /// Beneficiary (creditor) bank + rail address to pay, for on-chain settlement.
    #[serde(default)]
    pub creditor_bank_id: Option<String>,
    #[serde(default)]
    pub creditor_address: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// `obp_status_update` — a Transaction Request's status changed.
#[derive(Debug, Deserialize)]
pub struct StatusUpdate {
    pub transaction_request_id: String,
    pub status: String,
}

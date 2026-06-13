//! Blockchain backend abstraction for OBP Bank Node.
//!
//! Implementations live in submodules (`cardano`, `mock`). Callers depend
//! only on the [`BlockchainBackend`] trait and the chain-agnostic record
//! types declared in this module.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod cardano;
pub mod mock;

#[derive(Debug, thiserror::Error)]
pub enum BlockchainError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("transaction rejected: {0}")]
    Rejected(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, BlockchainError>;

/// Chain-agnostic transaction reference. Hides chain-specific identifiers
/// (Cardano `TxId`, Ethereum tx hash, etc.) behind a uniform shape so the
/// rest of the node never imports a chain-specific type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxReference {
    pub chain: String,
    pub tx_id: String,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ConfirmationStatus {
    Pending,
    Confirmed { depth: u32 },
    Rejected,
}

/// A Promise records that a payment has been initiated and is awaiting
/// netting. Written immediately after the payment is durably stored in the
/// outbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromiseRecord {
    pub bank_id: String,
    pub transaction_request_id: String,
    pub amount: String,
    pub currency: String,
    pub corridor: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A Settlement records the netted-and-settled outcome of a group of
/// Promises.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementRecord {
    pub bank_id: String,
    pub snapshot_id: String,
    pub net_amount: String,
    pub currency: String,
    pub promise_tx_ids: Vec<String>,
    pub settled_at: chrono::DateTime<chrono::Utc>,
}

/// An Exception records that a Promise could not be settled within the
/// configured retry window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionRecord {
    pub bank_id: String,
    pub transaction_request_id: String,
    pub reason: String,
    pub raised_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait]
pub trait BlockchainBackend: Send + Sync + 'static {
    async fn write_promise(&self, p: &PromiseRecord) -> Result<TxReference>;
    async fn write_settlement(&self, s: &SettlementRecord) -> Result<TxReference>;
    async fn write_exception(&self, e: &ExceptionRecord) -> Result<TxReference>;
    async fn confirm(&self, r: &TxReference) -> Result<ConfirmationStatus>;
}

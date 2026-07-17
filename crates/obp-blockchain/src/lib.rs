//! Blockchain backend abstraction for OBP Bank Node.
//!
//! Implementations live in submodules (`cardano`, `mock`). Callers depend
//! only on the [`BlockchainBackend`] trait and the chain-agnostic record
//! types declared in this module.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod cardano;
pub mod mock;
pub mod settlement;

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

/// The commitment hash shared by every notary record type: hex SHA-256 over
/// `salt ‖ canonical_bytes`. The single definition of the scheme — writers
/// ([`PromiseRecord::commit_v1`], [`SettlementRecord::commit_v1`],
/// [`ExceptionRecord::commit_v1`]) and verifiers all go through here so they
/// can't drift. The salt keeps low-entropy cleartext (amounts, currencies,
/// short reasons) from being brute-forced off the public chain; it must be
/// retained off-chain — and shared with the counterparty — so the commitment
/// can be revealed and verified later.
pub fn compute_commitment(canonical_bytes: &[u8], salt: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(canonical_bytes);
    hex::encode(hasher.finalize())
}

/// Verify a revealed `(canonical_bytes, salt)` pair against an
/// `expected_commitment` (hex) — recompute the hash and compare,
/// case-insensitively. The counterparty's check in the commit–reveal proof.
pub fn verify_commitment(canonical_bytes: &[u8], salt: &[u8], expected_commitment: &str) -> bool {
    compute_commitment(canonical_bytes, salt).eq_ignore_ascii_case(expected_commitment)
}

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
///
/// **Hash-commitment only.** The Promise that reaches the chain carries *no*
/// cleartext payment data — only a salted SHA-256 commitment over the
/// instruction, a schema tag, and a coarse timestamp. The cleartext and salt
/// stay off-chain with the corridor parties and are revealed in a dispute
/// (commit–reveal). This keeps PII/amounts off an immutable public ledger;
/// non-repudiation comes from the Cardano tx being signed by the originating
/// bank's wallet, not from the commitment itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromiseRecord {
    /// Domain-separated schema tag, e.g. `obp.promise.v1`. Carries no payment data.
    pub schema: String,
    /// Hex-encoded SHA-256 over `salt ‖ canonical_instruction`.
    pub commitment: String,
    /// Coarse on-chain timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl PromiseRecord {
    pub const SCHEMA_V1: &'static str = "obp.promise.v1";

    /// Build a v1 Promise commitment. `canonical_instruction` is a deterministic
    /// serialization of the cleartext instruction (identifiers + payload);
    /// `salt` must be retained off-chain — and shared with the counterparty —
    /// so the commitment can be revealed and verified later. Neither input is
    /// recoverable from the returned record.
    pub fn commit_v1(
        canonical_instruction: &[u8],
        salt: &[u8],
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            schema: Self::SCHEMA_V1.to_string(),
            commitment: Self::compute_commitment(canonical_instruction, salt),
            created_at,
        }
    }

    /// The commitment hash: hex SHA-256 over `salt ‖ canonical_instruction`.
    /// Delegates to the crate-level [`compute_commitment`] so all record
    /// types share one scheme definition.
    pub fn compute_commitment(canonical_instruction: &[u8], salt: &[u8]) -> String {
        compute_commitment(canonical_instruction, salt)
    }

    /// Verify a revealed `(canonical_instruction, salt)` pair against an
    /// `expected_commitment` (hex). This is the beneficiary's check in the
    /// commit–reveal proof — recompute the hash and compare, case-insensitively.
    pub fn verify_v1(canonical_instruction: &[u8], salt: &[u8], expected_commitment: &str) -> bool {
        verify_commitment(canonical_instruction, salt, expected_commitment)
    }
}

/// A Settlement records the netted-and-settled outcome of a group of
/// Promises.
///
/// Like the Promise, only a salted commitment over this record reaches the
/// chain (see [`Self::commit_v1`]); the cleartext and salt stay off-chain
/// with the corridor parties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementRecord {
    pub bank_id: String,
    pub snapshot_id: String,
    pub net_amount: String,
    pub currency: String,
    pub promise_tx_ids: Vec<String>,
    pub settled_at: chrono::DateTime<chrono::Utc>,
}

impl SettlementRecord {
    pub const SCHEMA_V1: &'static str = "obp.settlement.v1";

    /// Deterministic serialization committed on-chain: the record's JSON in
    /// struct declaration order. Reordering, adding, or removing fields
    /// changes every commitment — bump [`Self::SCHEMA_V1`] alongside any
    /// struct change.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("SettlementRecord serializes infallibly")
    }

    /// Build the v1 salted commitment over this record. `salt` must be
    /// retained off-chain — and shared with the counterparty — so the
    /// commitment can be revealed and verified later.
    pub fn commit_v1(&self, salt: &[u8]) -> String {
        compute_commitment(&self.canonical_bytes(), salt)
    }

    /// Verify this (revealed) record + salt against an on-chain commitment.
    pub fn verify_v1(&self, salt: &[u8], expected_commitment: &str) -> bool {
        verify_commitment(&self.canonical_bytes(), salt, expected_commitment)
    }
}

/// An Exception records that a Promise could not be settled within the
/// configured retry window.
///
/// Like the Promise, only a salted commitment over this record reaches the
/// chain (see [`Self::commit_v1`]); the cleartext and salt stay off-chain
/// with the corridor parties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionRecord {
    pub bank_id: String,
    pub transaction_request_id: String,
    pub reason: String,
    pub raised_at: chrono::DateTime<chrono::Utc>,
}

impl ExceptionRecord {
    pub const SCHEMA_V1: &'static str = "obp.exception.v1";

    /// Deterministic serialization committed on-chain: the record's JSON in
    /// struct declaration order. Reordering, adding, or removing fields
    /// changes every commitment — bump [`Self::SCHEMA_V1`] alongside any
    /// struct change.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ExceptionRecord serializes infallibly")
    }

    /// Build the v1 salted commitment over this record. `salt` must be
    /// retained off-chain — and shared with the counterparty — so the
    /// commitment can be revealed and verified later.
    pub fn commit_v1(&self, salt: &[u8]) -> String {
        compute_commitment(&self.canonical_bytes(), salt)
    }

    /// Verify this (revealed) record + salt against an on-chain commitment.
    pub fn verify_v1(&self, salt: &[u8], expected_commitment: &str) -> bool {
        verify_commitment(&self.canonical_bytes(), salt, expected_commitment)
    }
}

#[async_trait]
pub trait BlockchainBackend: Send + Sync + 'static {
    async fn write_promise(&self, p: &PromiseRecord) -> Result<TxReference>;
    /// Write the Settlement's salted commitment on-chain. The caller mints
    /// `salt` per record and persists it durably (alongside the cleartext
    /// record) before calling — the pair is what gets revealed in a dispute.
    async fn write_settlement(&self, s: &SettlementRecord, salt: &[u8]) -> Result<TxReference>;
    /// Write the Exception's salted commitment on-chain. Same salt contract
    /// as [`Self::write_settlement`].
    async fn write_exception(&self, e: &ExceptionRecord, salt: &[u8]) -> Result<TxReference>;
    async fn confirm(&self, r: &TxReference) -> Result<ConfirmationStatus>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exception(reason: &str) -> ExceptionRecord {
        ExceptionRecord {
            bank_id: "ke.01.kcs".into(),
            transaction_request_id: "tr-1".into(),
            reason: reason.into(),
            raised_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn commit_v1_is_deterministic_and_hides_cleartext() {
        let a = exception("unroutable destination").commit_v1(b"salt-1");
        let b = exception("unroutable destination").commit_v1(b"salt-1");
        assert_eq!(a, b, "same record + same salt ⇒ same commitment");
        assert_eq!(a.len(), 64, "hex SHA-256");
        // The cleartext reason must not be recoverable from the commitment.
        assert!(!a.contains("unroutable"));
    }

    #[test]
    fn commit_v1_differs_on_different_input_or_salt() {
        let r = exception("reason a");
        assert_ne!(r.commit_v1(b"salt-1"), exception("reason b").commit_v1(b"salt-1"));
        // The salt is load-bearing: same cleartext, different salt ⇒ different
        // commitment, so the record can't be brute-forced from the chain.
        assert_ne!(r.commit_v1(b"salt-1"), r.commit_v1(b"salt-2"));
    }

    #[test]
    fn verify_v1_roundtrips_and_rejects_wrong_salt() {
        let r = exception("timeout");
        let commitment = r.commit_v1(b"salt-1");
        assert!(r.verify_v1(b"salt-1", &commitment));
        assert!(r.verify_v1(b"salt-1", &commitment.to_uppercase()));
        assert!(!r.verify_v1(b"salt-2", &commitment));
        assert!(!exception("dispute").verify_v1(b"salt-1", &commitment));
    }

    #[test]
    fn settlement_commit_v1_roundtrips() {
        let s = SettlementRecord {
            bank_id: "ke.01.kcs".into(),
            snapshot_id: "snap-1".into(),
            net_amount: "2500".into(),
            currency: "KES".into(),
            promise_tx_ids: vec!["tx-1".into(), "tx-2".into()],
            settled_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        let commitment = s.commit_v1(b"salt-1");
        assert!(s.verify_v1(b"salt-1", &commitment));
        assert!(!s.verify_v1(b"salt-2", &commitment));
        assert!(!commitment.contains("2500"), "amount must not leak");
    }
}

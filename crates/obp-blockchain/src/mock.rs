//! In-memory backend for tests and offline development.
//!
//! Writes deterministic SHA-256-derived tx IDs and records every call.

use std::sync::Mutex;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::{
    BlockchainBackend, ConfirmationStatus, ExceptionRecord, PromiseRecord, Result,
    SettlementRecord, TxReference,
};

#[derive(Debug, Default)]
pub struct MockBackend {
    writes: Mutex<Vec<TxReference>>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot every TxReference returned so far. Test-only helper.
    pub fn writes(&self) -> Vec<TxReference> {
        self.writes.lock().expect("mock backend mutex poisoned").clone()
    }
}

fn make_ref(kind: &str, id: &str) -> TxReference {
    let mut h = Sha256::new();
    h.update(kind.as_bytes());
    h.update(b":");
    h.update(id.as_bytes());
    TxReference {
        chain: "mock".into(),
        tx_id: format!("{:x}", h.finalize()),
        submitted_at: chrono::Utc::now(),
    }
}

#[async_trait]
impl BlockchainBackend for MockBackend {
    async fn write_promise(&self, p: &PromiseRecord) -> Result<TxReference> {
        let r = make_ref("promise", &p.transaction_request_id);
        self.writes.lock().expect("mutex poisoned").push(r.clone());
        Ok(r)
    }

    async fn write_settlement(&self, s: &SettlementRecord) -> Result<TxReference> {
        let r = make_ref("settlement", &s.snapshot_id);
        self.writes.lock().expect("mutex poisoned").push(r.clone());
        Ok(r)
    }

    async fn write_exception(&self, e: &ExceptionRecord) -> Result<TxReference> {
        let r = make_ref("exception", &e.transaction_request_id);
        self.writes.lock().expect("mutex poisoned").push(r.clone());
        Ok(r)
    }

    async fn confirm(&self, _r: &TxReference) -> Result<ConfirmationStatus> {
        Ok(ConfirmationStatus::Confirmed { depth: 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn sample_promise(id: &str) -> PromiseRecord {
        PromiseRecord {
            bank_id: "bank.test".into(),
            transaction_request_id: id.into(),
            amount: "100.00".into(),
            currency: "EUR".into(),
            corridor: "EUR/EUR".into(),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn promise_writes_are_content_deterministic() {
        let c = MockBackend::new();
        let r1 = c.write_promise(&sample_promise("tr-1")).await.unwrap();
        let r2 = c.write_promise(&sample_promise("tr-1")).await.unwrap();
        let r3 = c.write_promise(&sample_promise("tr-2")).await.unwrap();
        assert_eq!(r1.tx_id, r2.tx_id, "same input must produce same tx_id");
        assert_ne!(r1.tx_id, r3.tx_id, "different input must produce different tx_id");
    }

    #[tokio::test]
    async fn writes_are_recorded() {
        let c = MockBackend::new();
        c.write_promise(&sample_promise("a")).await.unwrap();
        c.write_promise(&sample_promise("b")).await.unwrap();
        assert_eq!(c.writes().len(), 2);
    }

    #[tokio::test]
    async fn confirm_returns_confirmed() {
        let c = MockBackend::new();
        let r = c.write_promise(&sample_promise("a")).await.unwrap();
        let status = c.confirm(&r).await.unwrap();
        assert_eq!(status, ConfirmationStatus::Confirmed { depth: 1 });
    }
}

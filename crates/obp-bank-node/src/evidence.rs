//! Evidence store — the beneficiary bank's durable record of received promises.
//!
//! When Bank B receives an `obp_credit_notification` over Interface C, it lands
//! here: the cleartext instruction preimage, the **salt**, and the on-chain
//! commitment. This is what lets Bank B run the commit–reveal proof in a dispute
//! (see `how_hashes_would_be_used_by_lawyers_for_bank_b.md`) — it holds the salt
//! independently of Bank A, so it can recompute `SHA-256(salt ‖ preimage)` and
//! match it against the commitment Bank A signed onto the chain.
//!
//! Each row also records whether that recomputation matched at receive time
//! (`verified`), so a tampered or malformed notification is flagged immediately.

use std::path::Path;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::outbox::OutboxError;

/// One received-and-recorded credit notification. Mirrors the `evidence` table.
#[allow(dead_code)] // several columns are held for dispute/audit, not yet read
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EvidenceRecord {
    pub transaction_request_id: String,
    /// Hex SHA-256 commitment Bank A wrote on-chain.
    pub promise_commitment: String,
    /// The salt — held off Bank A so Bank B can open the commitment unaided.
    pub promise_salt: String,
    /// The exact canonical instruction bytes that were hashed (the preimage).
    pub promise_preimage: String,
    /// On-chain tx id of Bank A's Promise, and its chain.
    pub promise_id: Option<String>,
    pub promise_blockchain: Option<String>,
    /// Did `SHA-256(salt ‖ preimage) == commitment` at receive time?
    pub verified: bool,
    /// A few business fields surfaced for display/reconciliation.
    pub currency: Option<String>,
    pub amount: Option<String>,
    pub originator_name: Option<String>,
    /// The full credit-notification JSON, kept verbatim for audit.
    pub raw_message: String,
    pub received_at: String,
}

/// Fields to record a newly-received credit notification.
pub struct NewEvidence<'a> {
    pub transaction_request_id: &'a str,
    pub promise_commitment: &'a str,
    pub promise_salt: &'a str,
    pub promise_preimage: &'a str,
    pub promise_id: Option<&'a str>,
    pub promise_blockchain: Option<&'a str>,
    pub verified: bool,
    pub currency: Option<&'a str>,
    pub amount: Option<&'a str>,
    pub originator_name: Option<&'a str>,
    pub raw_message: &'a str,
}

#[derive(Clone)]
pub struct EvidenceStore {
    pool: SqlitePool,
}

impl EvidenceStore {
    pub async fn connect(path: &Path) -> Result<Self, OutboxError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| OutboxError::Dir {
                    path: parent.display().to_string(),
                    source,
                })?;
            }
        }
        let options = SqliteConnectOptions::new().filename(path).create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        Self::init_schema(&pool).await?;
        Ok(Self { pool })
    }

    #[cfg(test)]
    pub async fn connect_in_memory() -> Result<Self, OutboxError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        Self::init_schema(&pool).await?;
        Ok(Self { pool })
    }

    async fn init_schema(pool: &SqlitePool) -> Result<(), OutboxError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS evidence (
                transaction_request_id TEXT PRIMARY KEY,
                promise_commitment     TEXT NOT NULL,
                promise_salt           TEXT NOT NULL,
                promise_preimage       TEXT NOT NULL,
                promise_id             TEXT,
                promise_blockchain     TEXT,
                verified               INTEGER NOT NULL,
                currency               TEXT,
                amount                 TEXT,
                originator_name        TEXT,
                raw_message            TEXT NOT NULL,
                received_at            TEXT NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Record a received credit notification. Idempotent on
    /// `transaction_request_id`: a duplicate delivery replaces the row rather
    /// than erroring, so an at-least-once broker redelivery is safe.
    pub async fn upsert(&self, e: NewEvidence<'_>) -> Result<(), OutboxError> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query(
            "INSERT INTO evidence \
                (transaction_request_id, promise_commitment, promise_salt, promise_preimage, \
                 promise_id, promise_blockchain, verified, currency, amount, originator_name, \
                 raw_message, received_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(transaction_request_id) DO UPDATE SET \
                promise_commitment = excluded.promise_commitment, \
                promise_salt = excluded.promise_salt, \
                promise_preimage = excluded.promise_preimage, \
                promise_id = excluded.promise_id, \
                promise_blockchain = excluded.promise_blockchain, \
                verified = excluded.verified, \
                currency = excluded.currency, \
                amount = excluded.amount, \
                originator_name = excluded.originator_name, \
                raw_message = excluded.raw_message, \
                received_at = excluded.received_at",
        )
        .bind(e.transaction_request_id)
        .bind(e.promise_commitment)
        .bind(e.promise_salt)
        .bind(e.promise_preimage)
        .bind(e.promise_id)
        .bind(e.promise_blockchain)
        .bind(e.verified)
        .bind(e.currency)
        .bind(e.amount)
        .bind(e.originator_name)
        .bind(e.raw_message)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // `get`/`list` back the beneficiary status/dashboard views (and tests);
    // not yet called from the binary's hot path.
    #[allow(dead_code)]
    pub async fn get(&self, id: &str) -> Result<Option<EvidenceRecord>, OutboxError> {
        let rec = sqlx::query_as::<_, EvidenceRecord>(
            "SELECT * FROM evidence WHERE transaction_request_id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(rec)
    }

    #[allow(dead_code)]
    pub async fn list(&self, limit: i64) -> Result<Vec<EvidenceRecord>, OutboxError> {
        let recs = sqlx::query_as::<_, EvidenceRecord>(
            "SELECT * FROM evidence ORDER BY received_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(recs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, verified: bool) -> NewEvidence<'_> {
        NewEvidence {
            transaction_request_id: id,
            promise_commitment: "abcd",
            promise_salt: "00112233",
            promise_preimage: "{\"amount\":\"10\"}",
            promise_id: Some("cardano-tx-1"),
            promise_blockchain: Some("cardano"),
            verified,
            currency: Some("KES"),
            amount: Some("1500.00"),
            originator_name: Some("Acme Coffee Ltd"),
            raw_message: "{}",
        }
    }

    #[tokio::test]
    async fn upsert_then_get_roundtrips() {
        let store = EvidenceStore::connect_in_memory().await.unwrap();
        store.upsert(sample("tr-1", true)).await.unwrap();
        let rec = store.get("tr-1").await.unwrap().unwrap();
        assert_eq!(rec.promise_commitment, "abcd");
        assert_eq!(rec.promise_salt, "00112233");
        assert!(rec.verified);
        assert_eq!(rec.promise_id.as_deref(), Some("cardano-tx-1"));
        assert_eq!(rec.currency.as_deref(), Some("KES"));
    }

    #[tokio::test]
    async fn redelivery_replaces_not_errors() {
        let store = EvidenceStore::connect_in_memory().await.unwrap();
        store.upsert(sample("tr-2", false)).await.unwrap();
        // Same id again (broker at-least-once redelivery) must not error.
        store.upsert(sample("tr-2", true)).await.unwrap();
        let rec = store.get("tr-2").await.unwrap().unwrap();
        assert!(rec.verified, "the later delivery's value wins");
        assert_eq!(store.list(10).await.unwrap().len(), 1, "still one row");
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = EvidenceStore::connect_in_memory().await.unwrap();
        assert!(store.get("nope").await.unwrap().is_none());
    }
}

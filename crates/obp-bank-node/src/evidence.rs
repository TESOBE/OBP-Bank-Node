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
    /// Whom the CBS was asked to credit (from the notification's
    /// `beneficiary` block; absent on rows predating it).
    pub beneficiary_name: Option<String>,
    pub beneficiary_account_routing_scheme: Option<String>,
    pub beneficiary_account_routing_address: Option<String>,
    /// The full credit-notification JSON, kept verbatim for audit.
    pub raw_message: String,
    pub received_at: String,
    /// Outcome of the A2 delivery to the bank's CBS: `DELIVERED` / `FAILED`;
    /// `None` while no delivery attempt has been recorded.
    pub cbs_status: Option<String>,
    /// The CBS's own reference for the posted credit, when it returned one.
    pub cbs_reference: Option<String>,
    pub cbs_recorded_at: Option<String>,
    /// Stamped by `obp_settlement_advice` when the promise behind this credit
    /// was covered by a netted settle. `None` while unsettled.
    pub settlement_id: Option<String>,
    pub settled_at: Option<String>,
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
    pub beneficiary_name: Option<&'a str>,
    pub beneficiary_account_routing_scheme: Option<&'a str>,
    pub beneficiary_account_routing_address: Option<&'a str>,
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
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
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
                beneficiary_name       TEXT,
                beneficiary_account_routing_scheme  TEXT,
                beneficiary_account_routing_address TEXT,
                raw_message            TEXT NOT NULL,
                received_at            TEXT NOT NULL,
                cbs_status             TEXT,
                cbs_reference          TEXT,
                cbs_recorded_at        TEXT,
                settlement_id          TEXT,
                settled_at             TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Migration for databases created before the CBS-result / settlement
        // columns existed. SQLite has no `ADD COLUMN IF NOT EXISTS`; a
        // duplicate-column error means the column is already there, which is fine.
        for col in [
            "cbs_status TEXT",
            "cbs_reference TEXT",
            "cbs_recorded_at TEXT",
            "settlement_id TEXT",
            "settled_at TEXT",
            "beneficiary_name TEXT",
            "beneficiary_account_routing_scheme TEXT",
            "beneficiary_account_routing_address TEXT",
        ] {
            if let Err(e) = sqlx::query(&format!("ALTER TABLE evidence ADD COLUMN {col}"))
                .execute(pool)
                .await
            {
                if !e.to_string().contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }
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
                 beneficiary_name, beneficiary_account_routing_scheme, \
                 beneficiary_account_routing_address, raw_message, received_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
                beneficiary_name = excluded.beneficiary_name, \
                beneficiary_account_routing_scheme = excluded.beneficiary_account_routing_scheme, \
                beneficiary_account_routing_address = excluded.beneficiary_account_routing_address, \
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
        .bind(e.beneficiary_name)
        .bind(e.beneficiary_account_routing_scheme)
        .bind(e.beneficiary_account_routing_address)
        .bind(e.raw_message)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record the outcome of the A2 CBS delivery for this credit notification.
    /// A no-op when no evidence row exists for the id (the row is always
    /// upserted before delivery is attempted, so that indicates a bug).
    pub async fn record_cbs_result(
        &self,
        transaction_request_id: &str,
        status: &str,
        cbs_reference: Option<&str>,
    ) -> Result<(), OutboxError> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query(
            "UPDATE evidence SET cbs_status = ?, cbs_reference = ?, cbs_recorded_at = ? \
             WHERE transaction_request_id = ?",
        )
        .bind(status)
        .bind(cbs_reference)
        .bind(&now)
        .bind(transaction_request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stamp the credits covered by a settlement advice. Only unstamped rows
    /// are touched — a redelivered advice preserves the original `settled_at`.
    /// Returns the number of rows stamped.
    pub async fn mark_settled(
        &self,
        transaction_request_ids: &[String],
        settlement_id: &str,
    ) -> Result<u64, OutboxError> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut stamped = 0u64;
        for id in transaction_request_ids {
            let result = sqlx::query(
                "UPDATE evidence SET settlement_id = ?, settled_at = ? \
                 WHERE transaction_request_id = ? AND settled_at IS NULL",
            )
            .bind(settlement_id)
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?;
            stamped += result.rows_affected();
        }
        Ok(stamped)
    }

    pub async fn get(&self, id: &str) -> Result<Option<EvidenceRecord>, OutboxError> {
        let rec = sqlx::query_as::<_, EvidenceRecord>(
            "SELECT * FROM evidence WHERE transaction_request_id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(rec)
    }

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
            beneficiary_name: Some("Bea Beneficiary"),
            beneficiary_account_routing_scheme: Some("OBP"),
            beneficiary_account_routing_address: Some("acct-77"),
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
    async fn mark_settled_stamps_only_unstamped_rows() {
        let store = EvidenceStore::connect_in_memory().await.unwrap();
        store.upsert(sample("tr-a", true)).await.unwrap();
        store.upsert(sample("tr-b", true)).await.unwrap();

        let covered = vec!["tr-a".to_string(), "tr-b".to_string(), "tr-ghost".to_string()];
        let stamped = store.mark_settled(&covered, "settle-1").await.unwrap();
        assert_eq!(stamped, 2, "ghost id stamps nothing");
        let a = store.get("tr-a").await.unwrap().unwrap();
        assert_eq!(a.settlement_id.as_deref(), Some("settle-1"));
        let settled_at_first = a.settled_at.clone().expect("settled_at set");

        // Redelivered advice: no re-stamp, original settled_at preserved.
        let again = store.mark_settled(&covered, "settle-1").await.unwrap();
        assert_eq!(again, 0);
        let a = store.get("tr-a").await.unwrap().unwrap();
        assert_eq!(a.settled_at.as_deref(), Some(settled_at_first.as_str()));
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = EvidenceStore::connect_in_memory().await.unwrap();
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cbs_result_recorded_after_upsert() {
        let store = EvidenceStore::connect_in_memory().await.unwrap();
        store.upsert(sample("tr-cbs", true)).await.unwrap();
        let rec = store.get("tr-cbs").await.unwrap().unwrap();
        assert!(rec.cbs_status.is_none(), "no delivery attempted yet");

        store
            .record_cbs_result("tr-cbs", "DELIVERED", Some("CBS-REF-9"))
            .await
            .unwrap();
        let rec = store.get("tr-cbs").await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("DELIVERED"));
        assert_eq!(rec.cbs_reference.as_deref(), Some("CBS-REF-9"));
        assert!(rec.cbs_recorded_at.is_some());

        // A failed delivery overwrites (latest attempt wins), reference absent.
        store
            .record_cbs_result("tr-cbs", "FAILED", None)
            .await
            .unwrap();
        let rec = store.get("tr-cbs").await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("FAILED"));
        assert!(rec.cbs_reference.is_none());
    }

    #[tokio::test]
    async fn schema_migration_adds_cbs_columns_to_old_databases() {
        // A database created before the CBS-result columns must be upgraded
        // in place by init_schema.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            r#"
            CREATE TABLE evidence (
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
        .execute(&pool)
        .await
        .unwrap();

        EvidenceStore::init_schema(&pool).await.unwrap();
        let store = EvidenceStore { pool };
        store.upsert(sample("tr-old", true)).await.unwrap();
        let rec = store.get("tr-old").await.unwrap().unwrap();
        assert!(rec.cbs_status.is_none());
        store
            .record_cbs_result("tr-old", "DELIVERED", Some("R1"))
            .await
            .unwrap();
        assert_eq!(
            store
                .get("tr-old")
                .await
                .unwrap()
                .unwrap()
                .cbs_status
                .as_deref(),
            Some("DELIVERED")
        );
    }
}

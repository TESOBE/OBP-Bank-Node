//! Durable settlement state — idempotency and finality tracking.
//!
//! Interface C settlement instructions are redelivered by design (OBP-API
//! publishes them from a transactional outbox), so the node must never treat
//! delivery as "settle exactly once". This store makes the settle path
//! idempotent and gives finality a durable home:
//!
//! - **Claim before pay.** The handler inserts a row keyed by
//!   `idempotency_key` *before* calling the settlement backend. A redelivered
//!   or concurrent instruction finds the row and gets the recorded state back
//!   instead of a second payment. A row stuck in `SETTLING` (crash mid-settle)
//!   is ambiguous — it is surfaced, never automatically retried, because the
//!   transfer may already be on chain.
//! - **Submitted is not final.** A settlement is `SUBMITTED` at broadcast and
//!   only becomes `FINAL` once the chain-sync follower has reported the
//!   configured confirmation depth (see `finality.rs`). OBP-API polls by
//!   redelivering the instruction; the reply carries the current status.
//! - **Errors record retryability.** A rejection that provably never reached
//!   the chain (`BlockchainError::Rejected`) is retryable on redelivery;
//!   transport/internal errors are ambiguous and stick until reconciled.

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::outbox::OutboxError;

/// Row lifecycle values. Plain strings in SQLite; constants here so handler,
/// watcher, and tests cannot drift.
pub mod status {
    /// Claimed; the settlement backend call is (or was) in flight. A row that
    /// stays here across a restart is ambiguous — reconcile before retrying.
    pub const SETTLING: &str = "SETTLING";
    /// Broadcast to the chain; awaiting confirmation depth.
    pub const SUBMITTED: &str = "SUBMITTED";
    /// Confirmation depth reached the configured finality threshold.
    pub const FINAL: &str = "FINAL";
    /// The attempt failed; `retryable` says whether redelivery may try again.
    pub const ERROR: &str = "ERROR";
}

// Some columns (`snapshot_id`, `currency`, `creditor_address`, `updated_at`)
// are persisted for audit/reconciliation and not yet read by code.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SettlementRow {
    pub idempotency_key: String,
    pub settlement_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub currency: String,
    /// u128 minor units as text (SQLite has no u128).
    pub net_amount_minor: String,
    pub creditor_address: String,
    pub status: String,
    pub tx_id: Option<String>,
    pub blockchain: Option<String>,
    pub asset: Option<String>,
    pub asset_amount: Option<String>,
    /// Last confirmation depth observed by the finality watcher.
    pub last_depth: i64,
    pub error_reason: Option<String>,
    pub retryable: bool,
    pub created_at: String,
    pub updated_at: String,
    pub finalized_at: Option<String>,
}

/// Fields captured at claim time (from the inbound instruction).
pub struct NewClaim<'a> {
    pub idempotency_key: &'a str,
    pub settlement_id: Option<&'a str>,
    pub snapshot_id: Option<&'a str>,
    pub currency: &'a str,
    pub net_amount_minor: u128,
    pub creditor_address: &'a str,
}

/// Result of a claim attempt.
pub enum Claim {
    /// We inserted the row — this delivery owns the settlement attempt.
    Claimed,
    /// A row already exists — return its state instead of paying again.
    /// (Boxed: the row dwarfs the empty variant, per clippy.)
    Existing(Box<SettlementRow>),
}

#[derive(Clone)]
pub struct SettlementStore {
    pool: SqlitePool,
}

impl SettlementStore {
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
            "CREATE TABLE IF NOT EXISTS settlements (
                idempotency_key   TEXT PRIMARY KEY,
                settlement_id     TEXT,
                snapshot_id       TEXT,
                currency          TEXT NOT NULL,
                net_amount_minor  TEXT NOT NULL,
                creditor_address  TEXT NOT NULL,
                status            TEXT NOT NULL,
                tx_id             TEXT,
                blockchain        TEXT,
                asset             TEXT,
                asset_amount      TEXT,
                last_depth        INTEGER NOT NULL DEFAULT 0,
                error_reason      TEXT,
                retryable         INTEGER NOT NULL DEFAULT 0,
                created_at        TEXT NOT NULL,
                updated_at        TEXT NOT NULL,
                finalized_at      TEXT
            )",
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Atomically claim the settlement for this delivery. `INSERT OR IGNORE`
    /// on the primary key is the mutual exclusion: exactly one concurrent
    /// delivery inserts; everyone else sees the existing row.
    pub async fn claim(&self, c: NewClaim<'_>) -> Result<Claim, OutboxError> {
        let now = now();
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO settlements \
                (idempotency_key, settlement_id, snapshot_id, currency, net_amount_minor, \
                 creditor_address, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(c.idempotency_key)
        .bind(c.settlement_id)
        .bind(c.snapshot_id)
        .bind(c.currency)
        .bind(c.net_amount_minor.to_string())
        .bind(c.creditor_address)
        .bind(status::SETTLING)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if inserted == 1 {
            Ok(Claim::Claimed)
        } else {
            let row = self
                .get(c.idempotency_key)
                .await?
                .ok_or(OutboxError::Db(sqlx::Error::RowNotFound))?;
            Ok(Claim::Existing(Box::new(row)))
        }
    }

    /// An `ERROR` row whose failure was provably-not-on-chain may be retried:
    /// flip it back to `SETTLING` so this delivery owns a fresh attempt.
    pub async fn reclaim_for_retry(&self, idempotency_key: &str) -> Result<(), OutboxError> {
        sqlx::query(
            "UPDATE settlements SET status = ?, error_reason = NULL, retryable = 0, updated_at = ? \
             WHERE idempotency_key = ? AND status = ? AND retryable = 1",
        )
        .bind(status::SETTLING)
        .bind(now())
        .bind(idempotency_key)
        .bind(status::ERROR)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_submitted(
        &self,
        idempotency_key: &str,
        tx_id: &str,
        blockchain: &str,
        asset: &str,
        asset_amount: &str,
    ) -> Result<(), OutboxError> {
        sqlx::query(
            "UPDATE settlements SET status = ?, tx_id = ?, blockchain = ?, asset = ?, \
             asset_amount = ?, updated_at = ? WHERE idempotency_key = ?",
        )
        .bind(status::SUBMITTED)
        .bind(tx_id)
        .bind(blockchain)
        .bind(asset)
        .bind(asset_amount)
        .bind(now())
        .bind(idempotency_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_error(
        &self,
        idempotency_key: &str,
        reason: &str,
        retryable: bool,
    ) -> Result<(), OutboxError> {
        sqlx::query(
            "UPDATE settlements SET status = ?, error_reason = ?, retryable = ?, updated_at = ? \
             WHERE idempotency_key = ?",
        )
        .bind(status::ERROR)
        .bind(reason)
        .bind(retryable)
        .bind(now())
        .bind(idempotency_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record the latest observed confirmation depth (watcher heartbeat). A
    /// rollback that reverts the tx to pending records depth 0.
    pub async fn record_depth(&self, idempotency_key: &str, depth: u32) -> Result<(), OutboxError> {
        sqlx::query(
            "UPDATE settlements SET last_depth = ?, updated_at = ? WHERE idempotency_key = ?",
        )
        .bind(depth as i64)
        .bind(now())
        .bind(idempotency_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_final(&self, idempotency_key: &str, depth: u32) -> Result<(), OutboxError> {
        let now = now();
        sqlx::query(
            "UPDATE settlements SET status = ?, last_depth = ?, finalized_at = ?, updated_at = ? \
             WHERE idempotency_key = ? AND status = ?",
        )
        .bind(status::FINAL)
        .bind(depth as i64)
        .bind(&now)
        .bind(&now)
        .bind(idempotency_key)
        .bind(status::SUBMITTED)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, idempotency_key: &str) -> Result<Option<SettlementRow>, OutboxError> {
        Ok(sqlx::query_as::<_, SettlementRow>(
            "SELECT * FROM settlements WHERE idempotency_key = ?",
        )
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The finality watcher's worklist: submitted, not yet final.
    pub async fn list_submitted(&self, limit: i64) -> Result<Vec<SettlementRow>, OutboxError> {
        Ok(sqlx::query_as::<_, SettlementRow>(
            "SELECT * FROM settlements WHERE status = ? ORDER BY updated_at ASC LIMIT ?",
        )
        .bind(status::SUBMITTED)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim_fields(key: &str) -> NewClaim<'_> {
        NewClaim {
            idempotency_key: key,
            settlement_id: Some("settle-1"),
            snapshot_id: None,
            currency: "KES",
            net_amount_minor: 2_500_000,
            creditor_address: "addr_test1creditor",
        }
    }

    #[tokio::test]
    async fn first_claim_wins_second_sees_existing() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        assert!(matches!(s.claim(claim_fields("k1")).await.unwrap(), Claim::Claimed));
        match s.claim(claim_fields("k1")).await.unwrap() {
            Claim::Existing(row) => {
                assert_eq!(row.status, status::SETTLING);
                assert_eq!(row.net_amount_minor, "2500000");
            }
            Claim::Claimed => panic!("second claim must not win"),
        }
    }

    #[tokio::test]
    async fn full_lifecycle_settling_submitted_final() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        s.claim(claim_fields("k1")).await.unwrap();
        s.mark_submitted("k1", "tx-1", "cardano", "ADA", "10000000").await.unwrap();
        let row = s.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::SUBMITTED);
        assert_eq!(row.tx_id.as_deref(), Some("tx-1"));

        s.record_depth("k1", 3).await.unwrap();
        assert_eq!(s.get("k1").await.unwrap().unwrap().last_depth, 3);

        s.mark_final("k1", 15).await.unwrap();
        let row = s.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::FINAL);
        assert_eq!(row.last_depth, 15);
        assert!(row.finalized_at.is_some());
    }

    #[tokio::test]
    async fn mark_final_only_applies_to_submitted_rows() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        s.claim(claim_fields("k1")).await.unwrap();
        // Still SETTLING — a stray finalize must not fire.
        s.mark_final("k1", 15).await.unwrap();
        assert_eq!(s.get("k1").await.unwrap().unwrap().status, status::SETTLING);
    }

    #[tokio::test]
    async fn retryable_error_can_be_reclaimed_sticky_error_cannot() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        s.claim(claim_fields("k1")).await.unwrap();
        s.mark_error("k1", "node rejected tx", true).await.unwrap();
        s.reclaim_for_retry("k1").await.unwrap();
        let row = s.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::SETTLING, "retryable error reopens");
        assert!(row.error_reason.is_none());

        s.mark_error("k1", "transport lost mid-submit", false).await.unwrap();
        s.reclaim_for_retry("k1").await.unwrap();
        let row = s.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::ERROR, "ambiguous error must stick");
    }

    #[tokio::test]
    async fn watcher_worklist_is_submitted_only() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        for k in ["a", "b", "c"] {
            s.claim(claim_fields(k)).await.unwrap();
        }
        s.mark_submitted("a", "tx-a", "cardano", "ADA", "1").await.unwrap();
        s.mark_submitted("b", "tx-b", "cardano", "ADA", "1").await.unwrap();
        s.mark_final("b", 20).await.unwrap();
        let work = s.list_submitted(10).await.unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].idempotency_key, "a");
    }
}

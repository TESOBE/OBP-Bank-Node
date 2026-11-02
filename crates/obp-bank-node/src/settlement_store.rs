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
    /// The settle-time FX quote the transfer was sized with: minor units of
    /// `currency` per whole `asset` (u128 as text), the source label, and the
    /// quote timestamp. NULL on rows predating the columns or when the rail
    /// needed no conversion.
    pub fx_minor_per_whole_asset: Option<String>,
    pub fx_source: Option<String>,
    pub fx_as_of: Option<String>,
    /// Last confirmation depth observed by the finality watcher.
    pub last_depth: i64,
    pub error_reason: Option<String>,
    pub retryable: bool,
    pub created_at: String,
    pub updated_at: String,
    pub finalized_at: Option<String>,
    /// `PLATFORM_FEE` for fee-sweep settlements; NULL = corridor settlement.
    pub purpose: Option<String>,
}

/// Fields captured at claim time (from the inbound instruction).
pub struct NewClaim<'a> {
    pub idempotency_key: &'a str,
    pub settlement_id: Option<&'a str>,
    pub snapshot_id: Option<&'a str>,
    pub currency: &'a str,
    pub net_amount_minor: u128,
    pub creditor_address: &'a str,
    pub purpose: Option<&'a str>,
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
    /// Whether the database answers, for the root `/status` page.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await.map(|_| ())
    }

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
                fx_minor_per_whole_asset TEXT,
                fx_source         TEXT,
                fx_as_of          TEXT,
                last_depth        INTEGER NOT NULL DEFAULT 0,
                error_reason      TEXT,
                retryable         INTEGER NOT NULL DEFAULT 0,
                created_at        TEXT NOT NULL,
                updated_at        TEXT NOT NULL,
                finalized_at      TEXT,
                purpose           TEXT
            )",
        )
        .execute(pool)
        .await?;

        // Migration for databases created before the FX columns. SQLite has no
        // `ADD COLUMN IF NOT EXISTS`; a duplicate-column error means the
        // column is already there, which is fine.
        for col in [
            "fx_minor_per_whole_asset TEXT",
            "fx_source TEXT",
            "fx_as_of TEXT",
            "purpose TEXT",
        ] {
            if let Err(e) = sqlx::query(&format!("ALTER TABLE settlements ADD COLUMN {col}"))
                .execute(pool)
                .await
            {
                if !e.to_string().contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }

        // Settlements this bank receives as creditor, from obp_settlement_advice,
        // and whether the payment has been seen arriving (see incoming.rs).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS incoming_settlements (
                settlement_id     TEXT PRIMARY KEY,
                debtor_bank_id    TEXT,
                currency          TEXT,
                net_amount        TEXT,
                covered_ids       TEXT NOT NULL,
                status            TEXT NOT NULL,
                tx_id             TEXT,
                asset             TEXT,
                asset_amount      TEXT,
                received_amount   TEXT,
                depth             INTEGER NOT NULL DEFAULT 0,
                detail            TEXT,
                received_at       TEXT NOT NULL,
                updated_at        TEXT NOT NULL
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
                 creditor_address, purpose, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(c.idempotency_key)
        .bind(c.settlement_id)
        .bind(c.snapshot_id)
        .bind(c.currency)
        .bind(c.net_amount_minor.to_string())
        .bind(c.creditor_address)
        .bind(c.purpose)
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
        fx: Option<&obp_blockchain::settlement::FxQuote>,
    ) -> Result<(), OutboxError> {
        sqlx::query(
            "UPDATE settlements SET status = ?, tx_id = ?, blockchain = ?, asset = ?, \
             asset_amount = ?, fx_minor_per_whole_asset = ?, fx_source = ?, fx_as_of = ?, \
             updated_at = ? WHERE idempotency_key = ?",
        )
        .bind(status::SUBMITTED)
        .bind(tx_id)
        .bind(blockchain)
        .bind(asset)
        .bind(asset_amount)
        .bind(fx.map(|q| q.minor_per_whole_asset.to_string()))
        .bind(fx.map(|q| q.source.as_str()))
        .bind(fx.map(|q| q.as_of.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
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

    /// Look a settlement up by `idempotency_key` **or** `settlement_id` —
    /// the south-side read API accepts either (they are often the same value;
    /// when they differ, the key wins on a collision because it is the PK).
    pub async fn find(&self, key: &str) -> Result<Option<SettlementRow>, OutboxError> {
        Ok(sqlx::query_as::<_, SettlementRow>(
            "SELECT * FROM settlements WHERE idempotency_key = ? OR settlement_id = ? \
             ORDER BY (idempotency_key = ?) DESC LIMIT 1",
        )
        .bind(key)
        .bind(key)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Most-recent-first listing for the south-side read API.
    pub async fn list(&self, limit: i64) -> Result<Vec<SettlementRow>, OutboxError> {
        Ok(sqlx::query_as::<_, SettlementRow>(
            "SELECT * FROM settlements ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
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

/// Statuses of a settlement this bank receives.
pub mod incoming_status {
    /// The advice arrived: OBP-API has netted the promises on its ledger.
    pub const NETTED: &str = "NETTED";
    /// The debtor's node reports paying; not yet seen here at finality depth.
    pub const PAID: &str = "PAID";
    /// Seen paying this node's own address at least the reported amount, at
    /// this node's finality depth.
    pub const RECEIVED: &str = "RECEIVED";
    /// Seen paying this node's address less than the debtor reported.
    pub const MISMATCH: &str = "MISMATCH";
}

/// One settlement this bank receives as creditor.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct IncomingSettlement {
    pub settlement_id: String,
    pub debtor_bank_id: Option<String>,
    pub currency: Option<String>,
    pub net_amount: Option<String>,
    /// JSON array of the covered OBP transaction request ids.
    pub covered_ids: String,
    pub status: String,
    pub tx_id: Option<String>,
    pub asset: Option<String>,
    /// What the debtor reports paying, in the asset's smallest unit.
    pub asset_amount: Option<String>,
    /// What was seen arriving at this node's address, same unit.
    pub received_amount: Option<String>,
    pub depth: i64,
    pub detail: Option<String>,
    pub received_at: String,
    pub updated_at: String,
}

/// What the incoming watcher learnt about one settlement this tick.
#[derive(Clone, Copy)]
pub struct IncomingUpdate<'a> {
    pub status: &'a str,
    pub tx_id: Option<&'a str>,
    pub asset: Option<&'a str>,
    pub asset_amount: Option<&'a str>,
    pub received_amount: Option<u64>,
    pub depth: u32,
    pub detail: &'a str,
}

impl SettlementStore {
    /// Record a settlement advice naming this bank as creditor. Idempotent:
    /// advices are redelivered, and a known settlement keeps its progress.
    pub async fn record_incoming(
        &self,
        settlement_id: &str,
        debtor_bank_id: Option<&str>,
        currency: Option<&str>,
        net_amount: Option<&str>,
        covered_ids: &[String],
    ) -> Result<(), OutboxError> {
        let now = now();
        sqlx::query(
            "INSERT OR IGNORE INTO incoming_settlements \
                (settlement_id, debtor_bank_id, currency, net_amount, covered_ids, status, \
                 received_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(settlement_id)
        .bind(debtor_bank_id)
        .bind(currency)
        .bind(net_amount)
        .bind(serde_json::to_string(covered_ids).unwrap_or_else(|_| "[]".into()))
        .bind(incoming_status::NETTED)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Most-recent-first listing for the south-side read API.
    pub async fn list_incoming(&self, limit: i64) -> Result<Vec<IncomingSettlement>, OutboxError> {
        Ok(sqlx::query_as::<_, IncomingSettlement>(
            "SELECT * FROM incoming_settlements ORDER BY received_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The incoming watcher's worklist: not yet RECEIVED (MISMATCH is
    /// re-checked too: a payment can still deepen or be followed by more).
    pub async fn list_open_incoming(&self) -> Result<Vec<IncomingSettlement>, OutboxError> {
        Ok(sqlx::query_as::<_, IncomingSettlement>(
            "SELECT * FROM incoming_settlements WHERE status != ? ORDER BY received_at ASC LIMIT 100",
        )
        .bind(incoming_status::RECEIVED)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn update_incoming(&self, settlement_id: &str, u: IncomingUpdate<'_>) -> Result<(), OutboxError> {
        sqlx::query(
            "UPDATE incoming_settlements SET status = ?, tx_id = COALESCE(?, tx_id), \
                asset = COALESCE(?, asset), asset_amount = COALESCE(?, asset_amount), \
                received_amount = COALESCE(?, received_amount), depth = ?, detail = ?, updated_at = ? \
             WHERE settlement_id = ?",
        )
        .bind(u.status)
        .bind(u.tx_id)
        .bind(u.asset)
        .bind(u.asset_amount)
        .bind(u.received_amount.map(|n| n.to_string()))
        .bind(u.depth as i64)
        .bind(u.detail)
        .bind(now())
        .bind(settlement_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod incoming_tests {
    use super::*;

    #[tokio::test]
    async fn incoming_settlement_moves_from_netted_to_received() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        s.record_incoming("set-1", Some("rt.bank.a"), Some("KES"), Some("1530.00"), &["tr-1".into()])
            .await
            .unwrap();
        assert_eq!(s.list_open_incoming().await.unwrap().len(), 1);

        let base = IncomingUpdate {
            status: incoming_status::PAID,
            tx_id: Some("tx-9"),
            asset: Some("ADA"),
            asset_amount: Some("61943319"),
            received_amount: None,
            depth: 0,
            detail: "reported",
        };
        s.update_incoming("set-1", base).await.unwrap();
        s.update_incoming("set-1", IncomingUpdate {
            status: incoming_status::RECEIVED,
            tx_id: None,
            received_amount: Some(61_943_319),
            depth: 15,
            detail: "received",
            ..base
        })
        .await
        .unwrap();

        let row = &s.list_incoming(10).await.unwrap()[0];
        assert_eq!(row.status, incoming_status::RECEIVED);
        assert_eq!(row.tx_id.as_deref(), Some("tx-9"), "a later update without tx id keeps it");
        assert_eq!(row.received_amount.as_deref(), Some("61943319"));
        assert_eq!(row.depth, 15);
        assert!(s.list_open_incoming().await.unwrap().is_empty());
    }
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
            purpose: Some("PLATFORM_FEE"),
        }
    }

    #[tokio::test]
    async fn first_claim_wins_second_sees_existing() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        assert!(matches!(
            s.claim(claim_fields("k1")).await.unwrap(),
            Claim::Claimed
        ));
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
        let quote = obp_blockchain::settlement::FxQuote {
            asset: "ADA".into(),
            currency: "KES".into(),
            minor_per_whole_asset: 7998,
            as_of: chrono::Utc::now(),
            source: "coingecko×er-api".into(),
        };
        s.mark_submitted("k1", "tx-1", "cardano", "ADA", "10000000", Some(&quote))
            .await
            .unwrap();
        let row = s.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::SUBMITTED);
        // The settle-time rate is part of the audit row.
        assert_eq!(row.fx_minor_per_whole_asset.as_deref(), Some("7998"));
        assert_eq!(row.fx_source.as_deref(), Some("coingecko×er-api"));
        assert!(row.fx_as_of.is_some());
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

        s.mark_error("k1", "transport lost mid-submit", false)
            .await
            .unwrap();
        s.reclaim_for_retry("k1").await.unwrap();
        let row = s.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::ERROR, "ambiguous error must stick");
    }

    #[tokio::test]
    async fn find_matches_idempotency_key_or_settlement_id() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        s.claim(claim_fields("key-1")).await.unwrap(); // settlement_id = "settle-1"

        assert_eq!(
            s.find("key-1").await.unwrap().unwrap().idempotency_key,
            "key-1"
        );
        assert_eq!(
            s.find("settle-1").await.unwrap().unwrap().idempotency_key,
            "key-1"
        );
        assert!(s.find("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_is_most_recent_first_and_capped() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        for k in ["a", "b", "c"] {
            s.claim(claim_fields(k)).await.unwrap();
        }
        let all = s.list(10).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(s.list(2).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn watcher_worklist_is_submitted_only() {
        let s = SettlementStore::connect_in_memory().await.unwrap();
        for k in ["a", "b", "c"] {
            s.claim(claim_fields(k)).await.unwrap();
        }
        s.mark_submitted("a", "tx-a", "cardano", "ADA", "1", None)
            .await
            .unwrap();
        s.mark_submitted("b", "tx-b", "cardano", "ADA", "1", None)
            .await
            .unwrap();
        s.mark_final("b", 20).await.unwrap();
        let work = s.list_submitted(10).await.unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].idempotency_key, "a");
    }
}

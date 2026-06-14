//! Durable transactional outbox (SQLite via `sqlx`).
//!
//! Every payment is written here — committed to disk — *before* the Bank Node
//! makes any external call. That is what makes the synchronous `202` durable:
//! if the process dies the instant after responding, the request survives and
//! the dispatcher (`crate::dispatcher`) replays it on the next boot.
//!
//! Lifecycle of a row (the `status` column):
//!
//! ```text
//!   INITIATED ──submit OBP TR──▶ SUBMITTED ──write Promise──▶ PROMISE_WRITTEN
//!       │                            │
//!       └────────── OBP hard-reject ─┴──────────────────────▶ EXCEPTION
//! ```
//!
//! Transport failures (OBP / chain unreachable) leave the row at its current
//! status; the dispatcher retries it after a backoff window. Hard rejects move
//! it to `EXCEPTION`, a terminal state.
//!
//! Timestamps are stored as RFC3339 UTC strings. In UTC the lexical ordering of
//! RFC3339 matches chronological ordering, so the backoff cutoff in
//! [`OutboxStore::claim_due`] is a plain string comparison in SQL — no date
//! functions or type-mapping required.

use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

/// Outbox row states. Stored as the uppercase string in the `status` column.
pub mod status {
    pub const INITIATED: &str = "INITIATED";
    pub const SUBMITTED: &str = "SUBMITTED";
    pub const PROMISE_WRITTEN: &str = "PROMISE_WRITTEN";
    pub const EXCEPTION: &str = "EXCEPTION";
}

#[derive(Debug, thiserror::Error)]
pub enum OutboxError {
    #[error("outbox database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("failed to prepare outbox directory {path}: {source}")]
    Dir {
        path: String,
        source: std::io::Error,
    },
}

/// One persisted payment request and its lifecycle bookkeeping. Mirrors the
/// `outbox` table one-to-one. Some columns (`last_attempted_at`,
/// `exception_reason`, `updated_at`) are surfaced for the status/Interface-C
/// flows that will read them; allow them to be unread for now.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutboxRecord {
    pub transaction_request_id: String,
    pub status: String,
    pub bank_id: String,
    pub account_id: String,
    /// The original A1.1 request body, serialized as JSON. Replayed verbatim
    /// to OBP-API by the dispatcher.
    pub request_payload: String,
    /// Per-request salt for the Cardano Promise commitment. Stored so the
    /// commitment is recoverable; must also be shared with the counterparty
    /// for the commit–reveal dispute scheme (see Interface C work).
    pub commitment_salt: String,
    pub attempt_count: i64,
    /// RFC3339 UTC; `None` until the dispatcher first touches the row.
    pub last_attempted_at: Option<String>,
    pub promise_tx_id: Option<String>,
    pub promise_blockchain: Option<String>,
    pub exception_reason: Option<String>,
    /// RFC3339 UTC.
    pub created_at: String,
    /// RFC3339 UTC.
    pub updated_at: String,
}

/// The fields needed to enqueue a new payment. Everything else
/// (`status = INITIATED`, `attempt_count = 0`, timestamps) is set by the store.
pub struct NewEntry<'a> {
    pub transaction_request_id: &'a str,
    pub bank_id: &'a str,
    pub account_id: &'a str,
    pub request_payload: &'a str,
    /// Hex-encoded random salt minted for this request. See
    /// [`OutboxRecord::commitment_salt`].
    pub commitment_salt: &'a str,
}

#[derive(Clone)]
pub struct OutboxStore {
    pool: SqlitePool,
}

impl OutboxStore {
    /// Open (creating if absent) the SQLite database at `path` and ensure the
    /// schema exists. The parent directory is created if missing.
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

    /// Open an in-memory database — used by tests.
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
            CREATE TABLE IF NOT EXISTS outbox (
                transaction_request_id TEXT PRIMARY KEY,
                status                 TEXT NOT NULL,
                bank_id                TEXT NOT NULL,
                account_id             TEXT NOT NULL,
                request_payload        TEXT NOT NULL,
                commitment_salt        TEXT NOT NULL,
                attempt_count          INTEGER NOT NULL DEFAULT 0,
                last_attempted_at      TEXT,
                promise_tx_id          TEXT,
                promise_blockchain     TEXT,
                exception_reason       TEXT,
                created_at             TEXT NOT NULL,
                updated_at             TEXT NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Backoff scans filter by status then by last_attempted_at.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_outbox_status_attempted \
             ON outbox (status, last_attempted_at)",
        )
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Insert a new request at `INITIATED`. Must be committed before the caller
    /// returns its `202`. Fails if the `transaction_request_id` already exists.
    pub async fn insert(&self, entry: NewEntry<'_>) -> Result<(), OutboxError> {
        let now = rfc3339(Utc::now());
        sqlx::query(
            "INSERT INTO outbox \
                (transaction_request_id, status, bank_id, account_id, request_payload, \
                 commitment_salt, attempt_count, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(entry.transaction_request_id)
        .bind(status::INITIATED)
        .bind(entry.bank_id)
        .bind(entry.account_id)
        .bind(entry.request_payload)
        .bind(entry.commitment_salt)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<OutboxRecord>, OutboxError> {
        let rec = sqlx::query_as::<_, OutboxRecord>("SELECT * FROM outbox WHERE transaction_request_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(rec)
    }

    /// Most-recent-first listing, capped at `limit`.
    pub async fn list(&self, limit: i64) -> Result<Vec<OutboxRecord>, OutboxError> {
        let recs = sqlx::query_as::<_, OutboxRecord>(
            "SELECT * FROM outbox ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(recs)
    }

    /// Rows that still need work: status `INITIATED` or `SUBMITTED`, and either
    /// never attempted or last attempted before `cutoff` (RFC3339 UTC). Oldest
    /// first, capped at `limit`. The dispatcher branches on each row's status.
    pub async fn claim_due(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, OutboxError> {
        let cutoff = rfc3339(cutoff);
        let recs = sqlx::query_as::<_, OutboxRecord>(
            "SELECT * FROM outbox \
             WHERE status IN (?, ?) \
               AND (last_attempted_at IS NULL OR last_attempted_at < ?) \
             ORDER BY created_at ASC LIMIT ?",
        )
        .bind(status::INITIATED)
        .bind(status::SUBMITTED)
        .bind(&cutoff)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(recs)
    }

    /// Bump `attempt_count` and stamp `last_attempted_at = now`. Called by the
    /// dispatcher each time it picks a row up, so a failing row backs off
    /// instead of spinning.
    pub async fn record_attempt(&self, id: &str) -> Result<(), OutboxError> {
        let now = rfc3339(Utc::now());
        sqlx::query(
            "UPDATE outbox SET attempt_count = attempt_count + 1, \
                last_attempted_at = ?, updated_at = ? WHERE transaction_request_id = ?",
        )
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_submitted(&self, id: &str) -> Result<(), OutboxError> {
        self.set_status(id, status::SUBMITTED).await
    }

    pub async fn mark_promise_written(
        &self,
        id: &str,
        promise_tx_id: &str,
        promise_blockchain: &str,
    ) -> Result<(), OutboxError> {
        let now = rfc3339(Utc::now());
        sqlx::query(
            "UPDATE outbox SET status = ?, promise_tx_id = ?, promise_blockchain = ?, \
                updated_at = ? WHERE transaction_request_id = ?",
        )
        .bind(status::PROMISE_WRITTEN)
        .bind(promise_tx_id)
        .bind(promise_blockchain)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_exception(&self, id: &str, reason: &str) -> Result<(), OutboxError> {
        let now = rfc3339(Utc::now());
        sqlx::query(
            "UPDATE outbox SET status = ?, exception_reason = ?, updated_at = ? \
             WHERE transaction_request_id = ?",
        )
        .bind(status::EXCEPTION)
        .bind(reason)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_status(&self, id: &str, new_status: &str) -> Result<(), OutboxError> {
        let now = rfc3339(Utc::now());
        sqlx::query("UPDATE outbox SET status = ?, updated_at = ? WHERE transaction_request_id = ?")
            .bind(new_status)
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// RFC3339 with second precision and a `Z` offset — stable and lexically
/// sortable in UTC.
fn rfc3339(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(id: &'a str, payload: &'a str) -> NewEntry<'a> {
        NewEntry {
            transaction_request_id: id,
            bank_id: "ke.01.kcs",
            account_id: "acct-1",
            request_payload: payload,
            commitment_salt: "00112233445566778899aabbccddeeff",
        }
    }

    #[tokio::test]
    async fn insert_then_get_roundtrips_at_initiated() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-1", r#"{"amount":"10"}"#)).await.unwrap();

        let rec = store.get("tr-1").await.unwrap().expect("row present");
        assert_eq!(rec.status, status::INITIATED);
        assert_eq!(rec.bank_id, "ke.01.kcs");
        assert_eq!(rec.request_payload, r#"{"amount":"10"}"#);
        assert_eq!(rec.commitment_salt, "00112233445566778899aabbccddeeff");
        assert_eq!(rec.attempt_count, 0);
        assert!(rec.last_attempted_at.is_none());
        assert!(rec.promise_tx_id.is_none());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_id_is_rejected() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-dup", "{}")).await.unwrap();
        let err = store.insert(entry("tr-dup", "{}")).await;
        assert!(err.is_err(), "second insert of same id must fail");
    }

    #[tokio::test]
    async fn lifecycle_initiated_submitted_promise_written() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-2", "{}")).await.unwrap();

        store.mark_submitted("tr-2").await.unwrap();
        assert_eq!(store.get("tr-2").await.unwrap().unwrap().status, status::SUBMITTED);

        store.mark_promise_written("tr-2", "txhash-abc", "cardano").await.unwrap();
        let rec = store.get("tr-2").await.unwrap().unwrap();
        assert_eq!(rec.status, status::PROMISE_WRITTEN);
        assert_eq!(rec.promise_tx_id.as_deref(), Some("txhash-abc"));
        assert_eq!(rec.promise_blockchain.as_deref(), Some("cardano"));
    }

    #[tokio::test]
    async fn mark_exception_records_reason() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-3", "{}")).await.unwrap();
        store.mark_exception("tr-3", "unroutable destination").await.unwrap();

        let rec = store.get("tr-3").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert_eq!(rec.exception_reason.as_deref(), Some("unroutable destination"));
    }

    #[tokio::test]
    async fn record_attempt_bumps_count_and_stamps_time() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-4", "{}")).await.unwrap();

        store.record_attempt("tr-4").await.unwrap();
        let rec = store.get("tr-4").await.unwrap().unwrap();
        assert_eq!(rec.attempt_count, 1);
        assert!(rec.last_attempted_at.is_some());

        store.record_attempt("tr-4").await.unwrap();
        assert_eq!(store.get("tr-4").await.unwrap().unwrap().attempt_count, 2);
    }

    #[tokio::test]
    async fn claim_due_returns_unattempted_and_skips_terminal() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("due-1", "{}")).await.unwrap();
        store.insert(entry("done-1", "{}")).await.unwrap();
        store.mark_promise_written("done-1", "tx", "cardano").await.unwrap();

        // A far-future cutoff makes every non-terminal row "due".
        let cutoff = Utc::now() + chrono::Duration::hours(1);
        let due = store.claim_due(cutoff, 10).await.unwrap();
        let ids: Vec<_> = due.iter().map(|r| r.transaction_request_id.as_str()).collect();
        assert_eq!(ids, vec!["due-1"], "only the non-terminal row is due");
    }

    #[tokio::test]
    async fn claim_due_respects_backoff_cutoff() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("recent", "{}")).await.unwrap();
        store.record_attempt("recent").await.unwrap();

        // Cutoff in the past: the just-attempted row is still backing off.
        let cutoff = Utc::now() - chrono::Duration::hours(1);
        let due = store.claim_due(cutoff, 10).await.unwrap();
        assert!(due.is_empty(), "row attempted after the cutoff must not be due");
    }
}

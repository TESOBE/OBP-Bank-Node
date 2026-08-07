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
//!   INITIATED ─submit OBP TR─▶ SUBMITTED ─write Promise─▶ PROMISE_WRITTEN ─report evidence─▶ REPORTED
//!       │                          │                            │
//!       └───────── OBP hard-reject ┴────────────────────────────┴─────────▶ EXCEPTION
//! ```
//!
//! Transport failures (OBP / chain unreachable) leave the row at its current
//! status; the dispatcher retries it after a backoff window. Hard rejects move
//! it to `EXCEPTION`, a terminal state. `REPORTED` — the Promise evidence
//! (tx hash + commit–reveal triplet) delivered to OBP-API's report-back
//! endpoint — is the terminal success state.
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
    pub const REPORTED: &str = "REPORTED";
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
    /// The Transaction Request id OBP-API assigned at submit time. Addresses
    /// the promise report-back endpoint; `None` until `SUBMITTED` (or forever,
    /// if OBP-API's create response carried no id — such a row cannot report).
    pub obp_transaction_request_id: Option<String>,
    pub promise_tx_id: Option<String>,
    pub promise_blockchain: Option<String>,
    pub exception_reason: Option<String>,
    /// The Open Corridor settlement that covered (netted) this promise, stamped
    /// when the settle result / corridor status names this row's OBP TR id in
    /// `covered_transaction_request_ids`. `None` while unsettled.
    pub settlement_id: Option<String>,
    /// RFC3339 UTC; set together with `settlement_id`.
    pub settled_at: Option<String>,
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
                obp_transaction_request_id TEXT,
                promise_tx_id          TEXT,
                promise_blockchain     TEXT,
                exception_reason       TEXT,
                settlement_id          TEXT,
                settled_at             TEXT,
                created_at             TEXT NOT NULL,
                updated_at             TEXT NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Migrations for databases created before later columns existed
        // (report-back step; settlement linkage). SQLite has no
        // `ADD COLUMN IF NOT EXISTS`; a duplicate-column error means the
        // column is already there, which is fine.
        for col in [
            "obp_transaction_request_id TEXT",
            "settlement_id TEXT",
            "settled_at TEXT",
        ] {
            if let Err(e) = sqlx::query(&format!("ALTER TABLE outbox ADD COLUMN {col}"))
                .execute(pool)
                .await
            {
                if !e.to_string().contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }

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
        let rec = sqlx::query_as::<_, OutboxRecord>(
            "SELECT * FROM outbox WHERE transaction_request_id = ?",
        )
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

    /// Rows that still need work: status `INITIATED`, `SUBMITTED`, or
    /// `PROMISE_WRITTEN`, and either never attempted or last attempted before
    /// `cutoff` (RFC3339 UTC). Oldest first, capped at `limit`. The dispatcher
    /// branches on each row's status.
    pub async fn claim_due(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<OutboxRecord>, OutboxError> {
        let cutoff = rfc3339(cutoff);
        let recs = sqlx::query_as::<_, OutboxRecord>(
            "SELECT * FROM outbox \
             WHERE status IN (?, ?, ?) \
               AND (last_attempted_at IS NULL OR last_attempted_at < ?) \
             ORDER BY created_at ASC LIMIT ?",
        )
        .bind(status::INITIATED)
        .bind(status::SUBMITTED)
        .bind(status::PROMISE_WRITTEN)
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

    /// Move a row to `SUBMITTED`, recording the Transaction Request id OBP-API
    /// assigned — the report-back endpoint is addressed by that id.
    pub async fn mark_submitted(
        &self,
        id: &str,
        obp_transaction_request_id: Option<&str>,
    ) -> Result<(), OutboxError> {
        let now = rfc3339(Utc::now());
        sqlx::query(
            "UPDATE outbox SET status = ?, obp_transaction_request_id = ?, updated_at = ? \
             WHERE transaction_request_id = ?",
        )
        .bind(status::SUBMITTED)
        .bind(obp_transaction_request_id)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Terminal success: the Promise evidence has been reported to OBP-API.
    pub async fn mark_reported(&self, id: &str) -> Result<(), OutboxError> {
        self.set_status(id, status::REPORTED).await
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

    /// Stamp the settlement linkage onto every row whose **OBP** Transaction
    /// Request id appears in `covered_obp_tr_ids` (the settle result's
    /// `covered_transaction_request_ids`). Rows already stamped keep their
    /// original `settled_at` — re-stamping from a redelivered/polled result is
    /// idempotent. Returns how many rows were newly stamped. Ids belonging to
    /// the other bank's promises simply match nothing here.
    pub async fn mark_settled(
        &self,
        covered_obp_tr_ids: &[String],
        settlement_id: &str,
    ) -> Result<u64, OutboxError> {
        if covered_obp_tr_ids.is_empty() {
            return Ok(0);
        }
        let now = rfc3339(Utc::now());
        let placeholders = vec!["?"; covered_obp_tr_ids.len()].join(", ");
        let sql = format!(
            "UPDATE outbox SET settlement_id = ?, settled_at = ?, updated_at = ? \
             WHERE settlement_id IS NULL AND obp_transaction_request_id IN ({placeholders})"
        );
        let mut query = sqlx::query(&sql).bind(settlement_id).bind(&now).bind(&now);
        for id in covered_obp_tr_ids {
            query = query.bind(id);
        }
        Ok(query.execute(&self.pool).await?.rows_affected())
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
        sqlx::query(
            "UPDATE outbox SET status = ?, updated_at = ? WHERE transaction_request_id = ?",
        )
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
        store
            .insert(entry("tr-1", r#"{"amount":"10"}"#))
            .await
            .unwrap();

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
    async fn lifecycle_initiated_submitted_promise_written_reported() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-2", "{}")).await.unwrap();

        store
            .mark_submitted("tr-2", Some("obp-tr-42"))
            .await
            .unwrap();
        let rec = store.get("tr-2").await.unwrap().unwrap();
        assert_eq!(rec.status, status::SUBMITTED);
        assert_eq!(rec.obp_transaction_request_id.as_deref(), Some("obp-tr-42"));

        store
            .mark_promise_written("tr-2", "txhash-abc", "cardano")
            .await
            .unwrap();
        let rec = store.get("tr-2").await.unwrap().unwrap();
        assert_eq!(rec.status, status::PROMISE_WRITTEN);
        assert_eq!(rec.promise_tx_id.as_deref(), Some("txhash-abc"));
        assert_eq!(rec.promise_blockchain.as_deref(), Some("cardano"));

        store.mark_reported("tr-2").await.unwrap();
        let rec = store.get("tr-2").await.unwrap().unwrap();
        assert_eq!(rec.status, status::REPORTED);
        assert_eq!(rec.obp_transaction_request_id.as_deref(), Some("obp-tr-42"));
    }

    #[tokio::test]
    async fn mark_submitted_without_obp_id_leaves_column_null() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-noid", "{}")).await.unwrap();
        store.mark_submitted("tr-noid", None).await.unwrap();
        let rec = store.get("tr-noid").await.unwrap().unwrap();
        assert_eq!(rec.status, status::SUBMITTED);
        assert!(rec.obp_transaction_request_id.is_none());
    }

    #[tokio::test]
    async fn mark_exception_records_reason() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("tr-3", "{}")).await.unwrap();
        store
            .mark_exception("tr-3", "unroutable destination")
            .await
            .unwrap();

        let rec = store.get("tr-3").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert_eq!(
            rec.exception_reason.as_deref(),
            Some("unroutable destination")
        );
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
        store.insert(entry("unreported-1", "{}")).await.unwrap();
        store
            .mark_promise_written("unreported-1", "tx", "cardano")
            .await
            .unwrap();
        store.insert(entry("done-1", "{}")).await.unwrap();
        store
            .mark_promise_written("done-1", "tx", "cardano")
            .await
            .unwrap();
        store.mark_reported("done-1").await.unwrap();
        store.insert(entry("failed-1", "{}")).await.unwrap();
        store.mark_exception("failed-1", "nope").await.unwrap();

        // A far-future cutoff makes every non-terminal row "due". A
        // PROMISE_WRITTEN row still owes the report-back, so it is due;
        // REPORTED and EXCEPTION are terminal.
        let cutoff = Utc::now() + chrono::Duration::hours(1);
        let due = store.claim_due(cutoff, 10).await.unwrap();
        let ids: Vec<_> = due
            .iter()
            .map(|r| r.transaction_request_id.as_str())
            .collect();
        assert_eq!(ids, vec!["due-1", "unreported-1"], "non-terminal rows only");
    }

    #[tokio::test]
    async fn mark_settled_stamps_only_covered_unstamped_rows() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        for (id, obp_id) in [("tr-a", "obp-1"), ("tr-b", "obp-2"), ("tr-c", "obp-3")] {
            store.insert(entry(id, "{}")).await.unwrap();
            store.mark_submitted(id, Some(obp_id)).await.unwrap();
        }

        // The covered list carries OBP TR ids — including the other bank's,
        // which match no local row and are silently skipped.
        let covered = vec![
            "obp-1".to_string(),
            "obp-3".to_string(),
            "obp-theirs".to_string(),
        ];
        let stamped = store.mark_settled(&covered, "settle-1").await.unwrap();
        assert_eq!(stamped, 2);

        let a = store.get("tr-a").await.unwrap().unwrap();
        assert_eq!(a.settlement_id.as_deref(), Some("settle-1"));
        let settled_at_first = a.settled_at.clone().expect("settled_at set");
        assert!(store
            .get("tr-b")
            .await
            .unwrap()
            .unwrap()
            .settlement_id
            .is_none());

        // Re-stamping (redelivered result / poll) is idempotent: no new rows,
        // original settled_at preserved.
        let again = store.mark_settled(&covered, "settle-1").await.unwrap();
        assert_eq!(again, 0);
        assert_eq!(
            store.get("tr-a").await.unwrap().unwrap().settled_at,
            Some(settled_at_first)
        );

        // Empty list is a no-op (and must not produce `IN ()` SQL).
        assert_eq!(store.mark_settled(&[], "settle-2").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn schema_migration_adds_obp_tr_id_to_old_databases() {
        // A database created before the report-back step lacks the
        // obp_transaction_request_id column; init_schema must add it.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            r#"
            CREATE TABLE outbox (
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
        .execute(&pool)
        .await
        .unwrap();

        OutboxStore::init_schema(&pool).await.unwrap();
        let store = OutboxStore { pool };
        store.insert(entry("tr-old", "{}")).await.unwrap();
        let rec = store.get("tr-old").await.unwrap().unwrap();
        assert!(rec.obp_transaction_request_id.is_none());
        // The settlement-linkage columns were added by the same migration.
        assert!(rec.settlement_id.is_none());
        assert!(rec.settled_at.is_none());
        store
            .mark_submitted("tr-old", Some("obp-old"))
            .await
            .unwrap();
        assert_eq!(
            store
                .mark_settled(&["obp-old".into()], "s-1")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn claim_due_respects_backoff_cutoff() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        store.insert(entry("recent", "{}")).await.unwrap();
        store.record_attempt("recent").await.unwrap();

        // Cutoff in the past: the just-attempted row is still backing off.
        let cutoff = Utc::now() - chrono::Duration::hours(1);
        let due = store.claim_due(cutoff, 10).await.unwrap();
        assert!(
            due.is_empty(),
            "row attempted after the cutoff must not be due"
        );
    }
}

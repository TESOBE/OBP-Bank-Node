// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! Background dispatcher — drains the outbox.
//!
//! The south-side handler only persists to the outbox and returns `202`. This
//! task does the external work, asynchronously, so the durability guarantee
//! holds: a `202` survives even if the process dies before any external call.
//!
//! Per outbox row, in lifecycle order:
//!   1. `INITIATED` → submit the OPEN_CORRIDOR_PROMISE Transaction Request to OBP-API
//!      (Interface B). Success → `SUBMITTED` (recording OBP's TR id). A
//!      terminal OBP rejection → `EXCEPTION`. A transport failure leaves the
//!      row `INITIATED` to retry.
//!   2. `SUBMITTED` → write the Cardano Promise *commitment* (Interface D).
//!      Success → `PROMISE_WRITTEN`. Any failure leaves the row `SUBMITTED`
//!      to retry.
//!   3. `PROMISE_WRITTEN` → report the evidence (tx hash + commitment + salt +
//!      preimage) back to OBP-API's report-back endpoint, addressed by OBP's
//!      TR id, so OBP-API can relay the salt to the beneficiary bank in
//!      `obp_credit_notification`. Success → `REPORTED`. A terminal rejection
//!      (e.g. OBP-40053 conflicting evidence) → `EXCEPTION`. A transport
//!      failure leaves the row `PROMISE_WRITTEN` to retry.
//!
//! A healthy row walks all steps in a single tick. Retries are paced by the
//! `retry_interval`: [`OutboxStore::claim_due`] only returns rows whose last
//! attempt is older than the cutoff.
//!
//! Retries are not endless. A row still failing a retryable step once
//! `max_retry_duration` has passed since it was created is moved to
//! `EXCEPTION`, with the last error in `exception_reason`, for an operator to
//! reconcile. Without that limit, a failure misread as temporary was retried
//! every 15 seconds for weeks (three promises, about 217,000 attempts).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use chrono::{SecondsFormat, Timelike};
use obp_blockchain::{BlockchainBackend, ExceptionRecord, PromiseRecord};
use tracing::{error, info, warn};

use crate::obp_client::{ObpClient, PromiseEvidence};
use crate::outbox::{status, OutboxRecord, OutboxStore};

pub struct DispatcherConfig {
    /// How often to scan the outbox.
    pub tick_interval: Duration,
    /// Minimum time between attempts on a single row.
    pub retry_interval: Duration,
    /// Maximum rows processed per scan.
    pub batch_size: i64,
    /// How long after a row was created its retryable failures are still
    /// retried. A row still failing after this is moved to `EXCEPTION`.
    pub max_retry_duration: Duration,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            retry_interval: Duration::from_secs(15),
            batch_size: 50,
            max_retry_duration: Duration::from_secs(24 * 60 * 60),
        }
    }
}

pub struct Dispatcher {
    store: OutboxStore,
    obp: Arc<ObpClient>,
    backend: Arc<dyn BlockchainBackend>,
    blockchain_label: &'static str,
    config: DispatcherConfig,
}

impl Dispatcher {
    pub fn new(
        store: OutboxStore,
        obp: Arc<ObpClient>,
        backend: Arc<dyn BlockchainBackend>,
        blockchain_label: &'static str,
        config: DispatcherConfig,
    ) -> Self {
        Self {
            store,
            obp,
            backend,
            blockchain_label,
            config,
        }
    }

    /// Run forever, scanning on each tick. Intended to be `tokio::spawn`ed.
    pub async fn run(self) {
        info!(
            tick_secs = self.config.tick_interval.as_secs(),
            retry_secs = self.config.retry_interval.as_secs(),
            "outbox dispatcher started"
        );
        loop {
            if let Err(e) = self.process_due_once().await {
                error!(error = %e, "dispatcher scan failed");
            }
            tokio::time::sleep(self.config.tick_interval).await;
        }
    }

    /// Process one batch of due rows. Returns how many rows were advanced to a
    /// new state (terminal or progressed). Exposed for tests.
    pub async fn process_due_once(&self) -> Result<usize, crate::outbox::OutboxError> {
        let retry = chrono::Duration::from_std(self.config.retry_interval)
            .unwrap_or_else(|_| chrono::Duration::seconds(15));
        let cutoff = Utc::now() - retry;
        let due = self.store.claim_due(cutoff, self.config.batch_size).await?;

        let mut advanced = 0;
        for row in due {
            self.store
                .record_attempt(&row.transaction_request_id)
                .await?;
            if self.process_row(&row).await? {
                advanced += 1;
            }
        }
        self.write_pending_exception_records().await?;
        Ok(advanced)
    }

    /// Advance a single row as far as it can go this tick. Returns whether its
    /// status changed.
    async fn process_row(&self, row: &OutboxRecord) -> Result<bool, crate::outbox::OutboxError> {
        let id = &row.transaction_request_id;
        let mut current = row.status.clone();
        let mut obp_tr_id = row.obp_transaction_request_id.clone();
        let mut promise_tx_id = row.promise_tx_id.clone();
        let mut advanced = false;

        // Step 1: submit to OBP-API if not already done.
        if current == status::INITIATED {
            match self
                .obp
                .submit_open_corridor(&row.bank_id, &row.account_id, &row.request_payload)
                .await
            {
                Ok(accepted) => {
                    info!(
                        transaction_request_id = %id,
                        obp_tr_id = ?accepted.obp_transaction_request_id,
                        "OBP-API accepted OPEN_CORRIDOR_PROMISE transaction request"
                    );
                    self.store
                        .mark_submitted(id, accepted.obp_transaction_request_id.as_deref())
                        .await?;
                    obp_tr_id = accepted.obp_transaction_request_id;
                    current = status::SUBMITTED.to_string();
                    advanced = true;
                }
                Err(e) if e.is_retryable() => {
                    if self.retries_exhausted(row) {
                        self.give_up(row, "submitting the transaction request to OBP-API", &e.to_string())
                            .await?;
                        return Ok(true);
                    }
                    warn!(
                        transaction_request_id = %id,
                        attempt = row.attempt_count + 1,
                        error = %e,
                        "OBP-API submit failed (retryable) — leaving INITIATED for backoff"
                    );
                    self.store.record_error(id, &e.to_string()).await?;
                    return Ok(false);
                }
                Err(e) => {
                    warn!(
                        transaction_request_id = %id,
                        error = %e,
                        "OBP-API rejected the request (terminal) — marking EXCEPTION"
                    );
                    self.raise_exception(row, &e.to_string()).await?;
                    return Ok(true);
                }
            }
        }

        // Step 2: write the Cardano Promise commitment.
        if current == status::SUBMITTED {
            let promise = self.build_commitment(row);
            match self.backend.write_promise(&promise).await {
                Ok(tx) => {
                    info!(
                        transaction_request_id = %id,
                        promise_tx_id = %tx.tx_id,
                        blockchain = %tx.chain,
                        "Promise commitment written"
                    );
                    self.store
                        .mark_promise_written(id, &tx.tx_id, self.blockchain_label)
                        .await?;
                    promise_tx_id = Some(tx.tx_id);
                    current = status::PROMISE_WRITTEN.to_string();
                    advanced = true;
                }
                Err(e) => {
                    if self.retries_exhausted(row) {
                        self.give_up(row, "writing the Promise commitment", &e.to_string())
                            .await?;
                        return Ok(true);
                    }
                    warn!(
                        transaction_request_id = %id,
                        error = %e,
                        "Promise write failed — leaving SUBMITTED for retry"
                    );
                    self.store.record_error(id, &e.to_string()).await?;
                    return Ok(advanced);
                }
            }
        }

        // Step 3: report the evidence back to OBP-API (the salt relay), so it
        // can be forwarded to the beneficiary bank in obp_credit_notification.
        if current == status::PROMISE_WRITTEN {
            let (Some(obp_tr_id), Some(tx_hash)) = (obp_tr_id.as_deref(), promise_tx_id.as_deref())
            else {
                // Without OBP's TR id there is no endpoint to address; without
                // the tx hash there is no evidence. Neither can appear later,
                // so retrying is pointless — surface for manual reconciliation.
                warn!(
                    transaction_request_id = %id,
                    obp_tr_id = ?obp_tr_id,
                    promise_tx_id = ?promise_tx_id,
                    "cannot report promise evidence — marking EXCEPTION"
                );
                self.raise_exception(
                    row,
                    "cannot report promise evidence to OBP-API: the OBP transaction \
                     request id or the promise tx hash was never recorded",
                )
                .await?;
                return Ok(true);
            };
            let preimage = self.canonical_preimage(row);
            let commitment = PromiseRecord::compute_commitment(
                preimage.as_bytes(),
                row.commitment_salt.as_bytes(),
            );
            let blockchain = row
                .promise_blockchain
                .as_deref()
                .unwrap_or(self.blockchain_label);
            let evidence = PromiseEvidence {
                tx_hash,
                blockchain,
                commitment: &commitment,
                salt: &row.commitment_salt,
                preimage: &preimage,
            };
            match self
                .obp
                .report_promise(&row.bank_id, &row.account_id, obp_tr_id, &evidence)
                .await
            {
                Ok(()) => {
                    info!(
                        transaction_request_id = %id,
                        obp_tr_id = %obp_tr_id,
                        promise_tx_id = %tx_hash,
                        "Promise evidence reported to OBP-API"
                    );
                    self.store.mark_reported(id).await?;
                    advanced = true;
                }
                Err(e) if e.is_retryable() => {
                    if self.retries_exhausted(row) {
                        self.give_up(row, "reporting the promise evidence to OBP-API", &e.to_string())
                            .await?;
                        advanced = true;
                    } else {
                        warn!(
                            transaction_request_id = %id,
                            error = %e,
                            "evidence report failed (retryable) — leaving PROMISE_WRITTEN for backoff"
                        );
                        self.store.record_error(id, &e.to_string()).await?;
                    }
                }
                Err(e) => {
                    warn!(
                        transaction_request_id = %id,
                        error = %e,
                        "OBP-API refused the promise evidence (terminal) — marking EXCEPTION"
                    );
                    self.raise_exception(row, &e.to_string()).await?;
                    advanced = true;
                }
            }
        }

        Ok(advanced)
    }

    /// Whether `max_retry_duration` has passed since the row was created. A row
    /// whose `created_at` cannot be read is treated as exhausted: it cannot be
    /// shown to be within the limit, and retrying it forever is the failure
    /// this limit exists to prevent.
    fn retries_exhausted(&self, row: &OutboxRecord) -> bool {
        let limit = chrono::Duration::from_std(self.config.max_retry_duration)
            .unwrap_or_else(|_| chrono::Duration::days(1));
        match chrono::DateTime::parse_from_rfc3339(&row.created_at) {
            Ok(created) => Utc::now().signed_duration_since(created.with_timezone(&Utc)) >= limit,
            Err(_) => true,
        }
    }

    /// Stop retrying a row: move it to `EXCEPTION`, recording which step kept
    /// failing, how often, and the last error, so an operator can reconcile it.
    async fn give_up(
        &self,
        row: &OutboxRecord,
        step: &str,
        last_error: &str,
    ) -> Result<(), crate::outbox::OutboxError> {
        let reason = format!(
            "gave up {step} after {attempts} attempts since {created}; the retry limit is {limit_hours} hours. Last error: {last_error}",
            attempts = row.attempt_count + 1,
            created = row.created_at,
            limit_hours = self.config.max_retry_duration.as_secs() / 3600,
        );
        warn!(transaction_request_id = %row.transaction_request_id, %reason, "retry limit reached — marking EXCEPTION");
        self.raise_exception(row, &reason).await
    }

    /// Move a row to `EXCEPTION` and raise the on-chain Exception record for it
    /// (Record 4): the salt and timestamp are stored first, so the record can
    /// be rebuilt from the row, then the chain write is attempted. The write
    /// is best-effort: a failure is retried on later ticks and never changes
    /// the row's outcome.
    async fn raise_exception(
        &self,
        row: &OutboxRecord,
        reason: &str,
    ) -> Result<(), crate::outbox::OutboxError> {
        let id = &row.transaction_request_id;
        self.store.mark_exception(id, reason).await?;
        let salt = uuid::Uuid::new_v4().simple().to_string();
        // Whole seconds: the stored text must re-serialize to the same bytes
        // the commitment was computed over.
        let raised_at = Utc::now().with_nanosecond(0).unwrap_or_else(Utc::now);
        self.store
            .record_exception_salt(id, &salt, &raised_at.to_rfc3339_opts(SecondsFormat::Secs, true))
            .await?;
        let record = ExceptionRecord {
            bank_id: row.bank_id.clone(),
            transaction_request_id: id.clone(),
            reason: reason.to_string(),
            raised_at,
        };
        self.write_exception_record(&record, &salt).await
    }

    async fn write_exception_record(
        &self,
        record: &ExceptionRecord,
        salt: &str,
    ) -> Result<(), crate::outbox::OutboxError> {
        let id = &record.transaction_request_id;
        match self.backend.write_exception(record, salt.as_bytes()).await {
            Ok(tx) => {
                info!(transaction_request_id = %id, exception_tx_id = %tx.tx_id, blockchain = %tx.chain, "Exception record written");
                self.store.record_exception_tx(id, &tx.tx_id, &tx.chain).await
            }
            Err(e) => {
                warn!(transaction_request_id = %id, error = %e, "Exception record write failed; will retry");
                Ok(())
            }
        }
    }

    /// Retry the Exception records that did not reach the chain when their
    /// rows were marked, up to `max_retry_duration` after they were raised.
    async fn write_pending_exception_records(&self) -> Result<(), crate::outbox::OutboxError> {
        let limit = chrono::Duration::from_std(self.config.max_retry_duration)
            .unwrap_or_else(|_| chrono::Duration::days(1));
        for row in self.store.list_exceptions_without_record(self.config.batch_size).await? {
            let (Some(salt), Some(raised_at), Some(reason)) = (
                row.exception_salt.as_deref(),
                row.exception_raised_at.as_deref(),
                row.exception_reason.as_deref(),
            ) else {
                continue;
            };
            let Ok(raised_at) = chrono::DateTime::parse_from_rfc3339(raised_at) else {
                continue;
            };
            let raised_at = raised_at.with_timezone(&Utc);
            if Utc::now().signed_duration_since(raised_at) >= limit {
                continue;
            }
            let record = ExceptionRecord {
                bank_id: row.bank_id.clone(),
                transaction_request_id: row.transaction_request_id.clone(),
                reason: reason.to_string(),
                raised_at,
            };
            self.write_exception_record(&record, salt).await?;
        }
        Ok(())
    }

    /// The canonical preimage for this row's commitment: a deterministic JSON
    /// object binding the identifiers to the original payload. This exact
    /// string is what gets reported to OBP-API and revealed to the beneficiary,
    /// who recomputes `SHA-256(salt ‖ preimage)` against the on-chain hash.
    fn canonical_preimage(&self, row: &OutboxRecord) -> String {
        let payload: serde_json::Value =
            serde_json::from_str(&row.request_payload).unwrap_or(serde_json::Value::Null);
        // serde_json's Map is a BTreeMap (no `preserve_order` feature), so key
        // ordering is sorted and stable — the serialization is canonical.
        let canonical = serde_json::json!({
            "transaction_request_id": row.transaction_request_id,
            "originating_bank_id": row.bank_id,
            "originating_account_id": row.account_id,
            "instruction": payload,
        });
        serde_json::to_string(&canonical).unwrap_or_default()
    }

    /// Compute the hash-commitment for this row's instruction over
    /// [`Self::canonical_preimage`] and the row's stored salt. No cleartext is
    /// retained in the returned record (see [`PromiseRecord`]).
    fn build_commitment(&self, row: &OutboxRecord) -> PromiseRecord {
        PromiseRecord::commit_v1(
            self.canonical_preimage(row).as_bytes(),
            row.commitment_salt.as_bytes(),
            Utc::now(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obp_client::ObpAuth;
    use crate::outbox::NewEntry;
    use axum::{routing::post, Json, Router};
    use obp_blockchain::mock::MockBackend;
    use std::net::SocketAddr;

    async fn spawn_obp_stub(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    const CREATE_TR_PATH: &str = "/obp/v7.0.0/banks/:bank/accounts/:acct/owner/transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests";
    const REPORT_PATH: &str =
        "/obp/v7.0.0/banks/:bank/accounts/:acct/transaction-requests/:tr/open-corridor/promise";

    /// OBP stub accepting the TR create but with no report-back endpoint (404).
    fn accepting_obp() -> Router {
        Router::new().route(
            CREATE_TR_PATH,
            post(|| async { Json(serde_json::json!({ "transaction_request_id": "obp-tr-1" })) }),
        )
    }

    /// OBP stub accepting both the TR create and the evidence report-back,
    /// forwarding each reported body to `captured`.
    fn full_obp(captured: tokio::sync::mpsc::Sender<serde_json::Value>) -> Router {
        accepting_obp().route(
            REPORT_PATH,
            post(move |Json(body): Json<serde_json::Value>| async move {
                captured.send(body).await.unwrap();
                (axum::http::StatusCode::CREATED, Json(serde_json::json!({})))
            }),
        )
    }

    fn rejecting_obp() -> Router {
        Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/owner/transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "message": "OBP-30018: Bank Account not found." })),
                )
            }),
        )
    }

    async fn insert_row(store: &OutboxStore, id: &str) {
        store
            .insert(NewEntry {
                transaction_request_id: id,
                bank_id: "ke.01.kcs",
                account_id: "acct-1",
                request_payload: r#"{"value":{"currency":"KES","amount":"1500.00"}}"#,
                commitment_salt: "00112233445566778899aabbccddeeff",
            })
            .await
            .unwrap();
    }

    /// No-retry-window config so `claim_due` returns rows immediately in tests.
    /// The retry limit is an hour, far longer than any test runs.
    fn dispatcher(
        store: OutboxStore,
        obp_base: String,
        backend: Arc<dyn BlockchainBackend>,
    ) -> Dispatcher {
        dispatcher_with_retry_limit(store, obp_base, backend, Duration::from_secs(3600))
    }

    fn dispatcher_with_retry_limit(
        store: OutboxStore,
        obp_base: String,
        backend: Arc<dyn BlockchainBackend>,
        max_retry_duration: Duration,
    ) -> Dispatcher {
        let obp = Arc::new(ObpClient::new(obp_base, ObpAuth::None).unwrap());
        Dispatcher::new(
            store,
            obp,
            backend,
            "mock",
            DispatcherConfig {
                tick_interval: Duration::from_millis(10),
                retry_interval: Duration::from_secs(0),
                batch_size: 50,
                max_retry_duration,
            },
        )
    }

    /// OBP stub that is temporarily unavailable: a retryable failure.
    fn unavailable_obp() -> Router {
        Router::new().route(
            CREATE_TR_PATH,
            post(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({ "code": 503, "message": "OBP-50000: Unknown Error." })),
                )
            }),
        )
    }

    /// OBP stub refusing the TR create the way the counterparty failure did:
    /// a 404 carrying an OBP business code.
    fn counterparty_refused_obp() -> Router {
        Router::new().route(
            CREATE_TR_PATH,
            post(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "code": 404, "message": "OBP-30014: Counterparty already exists. Please specify a different value for BANK_ID or ACCOUNT_ID or VIEW_ID or NAME." })),
                )
            }),
        )
    }

    #[tokio::test]
    async fn healthy_row_reaches_reported_in_one_tick() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-1").await;
        let backend = Arc::new(MockBackend::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let base = spawn_obp_stub(full_obp(tx)).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        let advanced = disp.process_due_once().await.unwrap();
        assert_eq!(advanced, 1);

        let rec = store.get("tr-1").await.unwrap().unwrap();
        assert_eq!(rec.status, status::REPORTED);
        assert_eq!(rec.obp_transaction_request_id.as_deref(), Some("obp-tr-1"));
        assert!(rec.promise_tx_id.is_some());
        assert_eq!(rec.promise_blockchain.as_deref(), Some("mock"));
        assert_eq!(backend.writes().len(), 1, "exactly one Promise written");

        // The reported evidence must satisfy the beneficiary's commit–reveal
        // check: SHA-256(salt ‖ preimage) == commitment, with the row's salt.
        let body = rx.recv().await.unwrap();
        assert_eq!(body["tx_hash"], rec.promise_tx_id.unwrap().as_str());
        assert_eq!(body["blockchain"], "mock");
        assert_eq!(body["salt"], rec.commitment_salt.as_str());
        assert!(PromiseRecord::verify_v1(
            body["preimage"].as_str().unwrap().as_bytes(),
            body["salt"].as_str().unwrap().as_bytes(),
            body["commitment"].as_str().unwrap(),
        ));
    }

    #[tokio::test]
    async fn missing_report_endpoint_leaves_promise_written_for_retry() {
        // OBP accepts the TR but has no report-back route (a 404 is
        // operational): the promise is written once, the row stays at
        // PROMISE_WRITTEN, and a later tick retries only the report.
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-nr").await;
        let backend = Arc::new(MockBackend::new());
        let base = spawn_obp_stub(accepting_obp()).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        disp.process_due_once().await.unwrap();
        let rec = store.get("tr-nr").await.unwrap().unwrap();
        assert_eq!(rec.status, status::PROMISE_WRITTEN);

        disp.process_due_once().await.unwrap();
        assert_eq!(
            backend.writes().len(),
            1,
            "the Promise must not be re-written on retry"
        );
        let rec = store.get("tr-nr").await.unwrap().unwrap();
        assert_eq!(rec.status, status::PROMISE_WRITTEN);
    }

    #[tokio::test]
    async fn evidence_conflict_marks_exception() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-c").await;
        let backend = Arc::new(MockBackend::new());
        let router = accepting_obp().route(
            REPORT_PATH,
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "code": 400, "message": "OBP-40053: Open Corridor promise evidence is already attached to this Transaction Request with different values." })),
                )
            }),
        );
        let base = spawn_obp_stub(router).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        disp.process_due_once().await.unwrap();

        let rec = store.get("tr-c").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert!(rec.exception_reason.unwrap().contains("OBP-40053"));
    }

    #[tokio::test]
    async fn resumes_a_promise_written_row_to_reported() {
        // Simulates a crash after the chain write but before the report-back.
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-rw").await;
        store
            .mark_submitted("tr-rw", Some("obp-tr-rw"))
            .await
            .unwrap();
        store
            .mark_promise_written("tr-rw", "txhash-live", "mock")
            .await
            .unwrap();
        let backend = Arc::new(MockBackend::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let base = spawn_obp_stub(full_obp(tx)).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        disp.process_due_once().await.unwrap();

        let rec = store.get("tr-rw").await.unwrap().unwrap();
        assert_eq!(rec.status, status::REPORTED);
        assert_eq!(
            backend.writes().len(),
            0,
            "the Promise must not be re-written"
        );
        let body = rx.recv().await.unwrap();
        assert_eq!(body["tx_hash"], "txhash-live", "reports the stored tx hash");
    }

    #[tokio::test]
    async fn promise_written_row_without_obp_tr_id_marks_exception() {
        // A row whose submit response never yielded OBP's TR id (or a legacy
        // pre-migration row) can never address the report-back endpoint.
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-noid").await;
        store.mark_submitted("tr-noid", None).await.unwrap();
        store
            .mark_promise_written("tr-noid", "txhash-x", "mock")
            .await
            .unwrap();
        let backend = Arc::new(MockBackend::new());
        // No HTTP call is made on this path; a refused port proves it.
        let disp = dispatcher(store.clone(), "http://127.0.0.1:1".into(), backend);

        disp.process_due_once().await.unwrap();

        let rec = store.get("tr-noid").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert!(rec
            .exception_reason
            .unwrap()
            .contains("transaction request id"));
    }

    #[tokio::test]
    async fn terminal_obp_rejection_marks_exception_and_writes_no_promise() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-2").await;
        let backend = Arc::new(MockBackend::new());
        let base = spawn_obp_stub(rejecting_obp()).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        disp.process_due_once().await.unwrap();

        let rec = store.get("tr-2").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert!(rec.exception_reason.unwrap().contains("OBP-30018"));
        assert!(!backend.written_kinds().contains(&"promise"), "no Promise on a rejected request");
    }

    #[tokio::test]
    async fn transport_failure_leaves_initiated_for_retry() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-3").await;
        let backend = Arc::new(MockBackend::new());
        // Point at a refused port so the OBP call is a transport error.
        let disp = dispatcher(store.clone(), "http://127.0.0.1:1".into(), backend.clone());

        let advanced = disp.process_due_once().await.unwrap();
        assert_eq!(advanced, 0, "nothing advanced");

        let rec = store.get("tr-3").await.unwrap().unwrap();
        assert_eq!(rec.status, status::INITIATED, "still pending retry");
        assert_eq!(rec.attempt_count, 1, "attempt was recorded for backoff");
        assert_eq!(backend.writes().len(), 0);
    }

    #[tokio::test]
    async fn resumes_a_submitted_row_to_promise_written() {
        // Simulates a crash after OBP submit but before the Promise write. The
        // TR must NOT be re-submitted; with OBP unreachable the row still
        // advances through the chain write, then parks at PROMISE_WRITTEN
        // because the report-back can't get through.
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-4").await;
        store
            .mark_submitted("tr-4", Some("obp-tr-4"))
            .await
            .unwrap();
        let backend = Arc::new(MockBackend::new());
        let disp = dispatcher(store.clone(), "http://127.0.0.1:1".into(), backend.clone());

        disp.process_due_once().await.unwrap();

        let rec = store.get("tr-4").await.unwrap().unwrap();
        assert_eq!(rec.status, status::PROMISE_WRITTEN);
        assert_eq!(backend.writes().len(), 1);
    }

    #[tokio::test]
    async fn commitment_is_deterministic_for_same_row() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-5").await;
        let row = store.get("tr-5").await.unwrap().unwrap();
        let backend = Arc::new(MockBackend::new());
        let disp = dispatcher(store.clone(), "http://127.0.0.1:1".into(), backend);

        let a = disp.build_commitment(&row);
        let b = disp.build_commitment(&row);
        assert_eq!(a.commitment, b.commitment);
        assert_eq!(a.schema, PromiseRecord::SCHEMA_V1);
        // The cleartext amount must not appear in the on-chain record.
        assert!(!a.commitment.contains("1500"));
    }

    #[tokio::test]
    async fn retryable_submit_failure_past_the_retry_limit_marks_exception_with_the_last_error() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-limit-1").await;
        let backend = Arc::new(MockBackend::new());
        let base = spawn_obp_stub(unavailable_obp()).await;
        let disp = dispatcher_with_retry_limit(store.clone(), base, backend.clone(), Duration::ZERO);

        let advanced = disp.process_due_once().await.unwrap();
        assert_eq!(advanced, 1, "giving up is a change of state");

        let rec = store.get("tr-limit-1").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        let reason = rec.exception_reason.unwrap();
        assert!(reason.contains("submitting the transaction request to OBP-API"), "{reason}");
        assert!(reason.contains("OBP-50000"), "the last error is recorded: {reason}");
        assert!(!backend.written_kinds().contains(&"promise"), "no Promise for a request OBP-API never accepted");

        assert_eq!(disp.process_due_once().await.unwrap(), 0, "an EXCEPTION row is not retried");
    }

    #[tokio::test]
    async fn retryable_submit_failure_within_the_retry_limit_stays_initiated() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-limit-2").await;
        let backend = Arc::new(MockBackend::new());
        let base = spawn_obp_stub(unavailable_obp()).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        assert_eq!(disp.process_due_once().await.unwrap(), 0);

        let rec = store.get("tr-limit-2").await.unwrap().unwrap();
        assert_eq!(rec.status, status::INITIATED, "still within the limit, so still retried");
        assert!(rec.exception_reason.is_none());
    }

    #[tokio::test]
    async fn retryable_report_failure_past_the_retry_limit_marks_exception() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-limit-3").await;
        let backend = Arc::new(MockBackend::new());
        // OBP accepts the TR but has no report-back route, so the report step
        // fails with a retryable 404.
        let base = spawn_obp_stub(accepting_obp()).await;
        let disp = dispatcher_with_retry_limit(store.clone(), base, backend.clone(), Duration::ZERO);

        disp.process_due_once().await.unwrap();

        let rec = store.get("tr-limit-3").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert!(
            rec.exception_reason.unwrap().contains("reporting the promise evidence to OBP-API"),
            "the step that kept failing is named"
        );
        assert_eq!(
            backend.written_kinds(),
            vec!["promise", "exception"],
            "the Promise was written before the report step gave up, then the Exception record"
        );
    }

    #[tokio::test]
    async fn not_found_refusal_with_a_business_code_marks_exception_on_the_first_attempt() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-refused").await;
        let backend = Arc::new(MockBackend::new());
        let base = spawn_obp_stub(counterparty_refused_obp()).await;
        // Well within the retry limit: the refusal is final, not retried.
        let disp = dispatcher(store.clone(), base, backend.clone());

        assert_eq!(disp.process_due_once().await.unwrap(), 1);

        let rec = store.get("tr-refused").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert_eq!(rec.attempt_count, 1);
        assert!(rec.exception_reason.unwrap().contains("OBP-30014"));
        assert!(!backend.written_kinds().contains(&"promise"));
    }

    /// A notary whose first `fail_times` writes fail, then behaves as the mock.
    struct FailingNotary {
        inner: MockBackend,
        remaining: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl BlockchainBackend for FailingNotary {
        async fn write_promise(&self, p: &PromiseRecord) -> obp_blockchain::Result<obp_blockchain::TxReference> {
            self.inner.write_promise(p).await
        }
        async fn write_settlement(
            &self,
            r: &obp_blockchain::SettlementRecord,
            salt: &[u8],
        ) -> obp_blockchain::Result<obp_blockchain::TxReference> {
            self.inner.write_settlement(r, salt).await
        }
        async fn write_exception(
            &self,
            e: &obp_blockchain::ExceptionRecord,
            salt: &[u8],
        ) -> obp_blockchain::Result<obp_blockchain::TxReference> {
            if self.remaining.fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            ).is_ok() {
                return Err(obp_blockchain::BlockchainError::Transport("ogmios unreachable".into()));
            }
            self.inner.write_exception(e, salt).await
        }
        async fn confirm(&self, r: &obp_blockchain::TxReference) -> obp_blockchain::Result<obp_blockchain::ConfirmationStatus> {
            self.inner.confirm(r).await
        }
    }

    /// The mock's tx id for an Exception record, recomputed from the outbox
    /// row alone: proves the record can be rebuilt and verified later.
    async fn expected_exception_tx_id(rec: &OutboxRecord) -> String {
        let record = obp_blockchain::ExceptionRecord {
            bank_id: rec.bank_id.clone(),
            transaction_request_id: rec.transaction_request_id.clone(),
            reason: rec.exception_reason.clone().unwrap(),
            raised_at: chrono::DateTime::parse_from_rfc3339(rec.exception_raised_at.as_deref().unwrap())
                .unwrap()
                .with_timezone(&Utc),
        };
        // The mock's tx id is a function of (kind, commitment) only.
        MockBackend::new()
            .write_exception(&record, rec.exception_salt.as_deref().unwrap().as_bytes())
            .await
            .unwrap()
            .tx_id
    }

    #[tokio::test]
    async fn terminal_obp_rejection_writes_an_exception_record() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-x1").await;
        let backend = Arc::new(MockBackend::new());
        let base = spawn_obp_stub(rejecting_obp()).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        disp.process_due_once().await.unwrap();

        let rec = store.get("tr-x1").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION);
        assert_eq!(backend.written_kinds(), vec!["exception"], "an Exception record, no Promise");
        assert_eq!(rec.exception_tx_id.as_deref(), Some(backend.writes()[0].tx_id.as_str()));
        assert_eq!(rec.exception_blockchain.as_deref(), Some("mock"));
        assert_eq!(rec.exception_tx_id.clone().unwrap(), expected_exception_tx_id(&rec).await);
    }

    #[tokio::test]
    async fn exception_record_write_failure_does_not_block_and_is_retried() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-x2").await;
        // The tick that marks the row also retries once, so two failures keep
        // the record off the chain until the next tick.
        let backend = Arc::new(FailingNotary {
            inner: MockBackend::new(),
            remaining: std::sync::atomic::AtomicUsize::new(2),
        });
        let base = spawn_obp_stub(rejecting_obp()).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        disp.process_due_once().await.unwrap();
        let rec = store.get("tr-x2").await.unwrap().unwrap();
        assert_eq!(rec.status, status::EXCEPTION, "the outcome does not wait for the chain");
        assert!(rec.exception_salt.is_some(), "the salt is kept for the retry");
        assert!(rec.exception_tx_id.is_none());

        disp.process_due_once().await.unwrap();
        let rec = store.get("tr-x2").await.unwrap().unwrap();
        assert_eq!(rec.exception_tx_id.clone().unwrap(), expected_exception_tx_id(&rec).await);
        assert!(store.list_exceptions_without_record(10).await.unwrap().is_empty());
    }
}

//! Background dispatcher — drains the outbox.
//!
//! The south-side handler only persists to the outbox and returns `202`. This
//! task does the external work, asynchronously, so the durability guarantee
//! holds: a `202` survives even if the process dies before any external call.
//!
//! Per outbox row, in lifecycle order:
//!   1. `INITIATED` → submit the OPEN_CORRIDOR Transaction Request to OBP-API
//!      (Interface B). Success → `SUBMITTED`. A terminal OBP rejection →
//!      `EXCEPTION`. A transport failure leaves the row `INITIATED` to retry.
//!   2. `SUBMITTED` → write the Cardano Promise *commitment* (Interface D).
//!      Success → `PROMISE_WRITTEN`. Any failure leaves the row `SUBMITTED`
//!      to retry.
//!
//! A healthy row walks both steps in a single tick. Retries are paced by the
//! `retry_interval`: [`OutboxStore::claim_due`] only returns rows whose last
//! attempt is older than the cutoff.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use obp_blockchain::{BlockchainBackend, PromiseRecord};
use tracing::{error, info, warn};

use crate::obp_client::ObpClient;
use crate::outbox::{status, OutboxRecord, OutboxStore};

pub struct DispatcherConfig {
    /// How often to scan the outbox.
    pub tick_interval: Duration,
    /// Minimum time between attempts on a single row.
    pub retry_interval: Duration,
    /// Maximum rows processed per scan.
    pub batch_size: i64,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            retry_interval: Duration::from_secs(15),
            batch_size: 50,
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
            self.store.record_attempt(&row.transaction_request_id).await?;
            if self.process_row(&row).await? {
                advanced += 1;
            }
        }
        Ok(advanced)
    }

    /// Advance a single row as far as it can go this tick. Returns whether its
    /// status changed.
    async fn process_row(&self, row: &OutboxRecord) -> Result<bool, crate::outbox::OutboxError> {
        let id = &row.transaction_request_id;

        // Step 1: submit to OBP-API if not already done.
        if row.status == status::INITIATED {
            match self
                .obp
                .submit_open_corridor(&row.bank_id, &row.account_id, &row.request_payload)
                .await
            {
                Ok(accepted) => {
                    info!(
                        transaction_request_id = %id,
                        obp_tr_id = ?accepted.obp_transaction_request_id,
                        "OBP-API accepted OPEN_CORRIDOR transaction request"
                    );
                    self.store.mark_submitted(id).await?;
                }
                Err(e) if e.is_retryable() => {
                    warn!(
                        transaction_request_id = %id,
                        attempt = row.attempt_count + 1,
                        error = %e,
                        "OBP-API submit failed (retryable) — leaving INITIATED for backoff"
                    );
                    return Ok(false);
                }
                Err(e) => {
                    warn!(
                        transaction_request_id = %id,
                        error = %e,
                        "OBP-API rejected the request (terminal) — marking EXCEPTION"
                    );
                    self.store.mark_exception(id, &e.to_string()).await?;
                    return Ok(true);
                }
            }
        }

        // Step 2: write the Cardano Promise commitment.
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
                Ok(true)
            }
            Err(e) => {
                warn!(
                    transaction_request_id = %id,
                    error = %e,
                    "Promise write failed — leaving SUBMITTED for retry"
                );
                Ok(false)
            }
        }
    }

    /// Compute the hash-commitment for this row's instruction. The preimage is a
    /// deterministic JSON object binding the identifiers to the original
    /// payload; the salt is the row's stored salt. No cleartext is retained in
    /// the returned record (see [`PromiseRecord`]).
    fn build_commitment(&self, row: &OutboxRecord) -> PromiseRecord {
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
        let canonical_bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        PromiseRecord::commit_v1(&canonical_bytes, row.commitment_salt.as_bytes(), Utc::now())
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

    fn accepting_obp() -> Router {
        Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/views/owner/transaction-request-types/OPEN_CORRIDOR/transaction-requests",
            post(|| async { Json(serde_json::json!({ "transaction_request_id": "obp-tr-1" })) }),
        )
    }

    fn rejecting_obp() -> Router {
        Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/views/owner/transaction-request-types/OPEN_CORRIDOR/transaction-requests",
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
    fn dispatcher(store: OutboxStore, obp_base: String, backend: Arc<dyn BlockchainBackend>) -> Dispatcher {
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
            },
        )
    }

    #[tokio::test]
    async fn healthy_row_reaches_promise_written_in_one_tick() {
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-1").await;
        let backend = Arc::new(MockBackend::new());
        let base = spawn_obp_stub(accepting_obp()).await;
        let disp = dispatcher(store.clone(), base, backend.clone());

        let advanced = disp.process_due_once().await.unwrap();
        assert_eq!(advanced, 1);

        let rec = store.get("tr-1").await.unwrap().unwrap();
        assert_eq!(rec.status, status::PROMISE_WRITTEN);
        assert!(rec.promise_tx_id.is_some());
        assert_eq!(rec.promise_blockchain.as_deref(), Some("mock"));
        assert_eq!(backend.writes().len(), 1, "exactly one Promise written");
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
        assert_eq!(backend.writes().len(), 0, "no Promise on a rejected request");
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
        // Simulates a crash after OBP submit but before the Promise write.
        let store = OutboxStore::connect_in_memory().await.unwrap();
        insert_row(&store, "tr-4").await;
        store.mark_submitted("tr-4").await.unwrap();
        let backend = Arc::new(MockBackend::new());
        // OBP must NOT be called again; point at a refused port to prove it.
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
}

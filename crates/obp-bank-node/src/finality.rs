//! Finality watcher — promotes `SUBMITTED` settlements to `FINAL`.
//!
//! Broadcast is not settlement. A value transfer counts as settled only once
//! it is buried under the configured confirmation depth, and a chain rollback
//! before that point can un-include it. This background task polls
//! [`SettlementBackend::confirm`] (backed by the chain-sync follower, which
//! reports real depth and reverts to `Pending` on rollback) for every
//! `SUBMITTED` row in the [`SettlementStore`] and:
//!
//! - records the latest observed depth (surfaced to OBP-API on redelivery),
//! - marks the row `FINAL` at `finality_depth`,
//! - marks it `ERROR` (non-retryable — the funds question needs a human) if
//!   the chain reports the tx rejected.

use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Timelike, Utc};
use obp_blockchain::settlement::SettlementBackend;
use obp_blockchain::{BlockchainBackend, ConfirmationStatus, SettlementRecord, TxReference};
use tracing::{info, warn};

use crate::outbox::{OutboxError, OutboxStore};
use crate::settlement_store::{SettlementRow, SettlementStore};

/// Rows examined per tick. Far above realistic in-flight settlement counts;
/// bounds a tick's work if something backs up.
const WORKLIST_LIMIT: i64 = 100;

/// How long after a settlement went FINAL its Settlement Reference record is
/// still retried on chain.
const RECORD_RETRY_WINDOW: chrono::Duration = chrono::Duration::hours(24);

pub struct FinalityWatcher {
    pub store: SettlementStore,
    pub backend: Arc<dyn SettlementBackend>,
    /// Writes the Settlement Reference record (Record 5) once a settlement is
    /// FINAL. Best-effort: a failed write is retried on later ticks.
    pub notary: Arc<dyn BlockchainBackend>,
    /// The promises a settlement covered, for the record.
    pub outbox: OutboxStore,
    pub bank_id: String,
    /// Depth at which a settlement is treated as final.
    pub finality_depth: u32,
    pub poll_interval: Duration,
}

impl FinalityWatcher {
    /// Poll forever. Errors are logged and retried next tick — the watcher
    /// must outlive transient store/chain failures.
    pub async fn run(self) {
        info!(
            finality_depth = self.finality_depth,
            poll_secs = self.poll_interval.as_secs(),
            "settlement finality watcher started"
        );
        loop {
            if let Err(e) = self.tick().await {
                warn!(error = %e, "finality watcher tick failed; retrying next interval");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// One pass over the `SUBMITTED` worklist. Public and side-effect-complete
    /// so tests drive it directly without the timer loop.
    pub async fn tick(&self) -> Result<(), OutboxError> {
        for row in self.store.list_submitted(WORKLIST_LIMIT).await? {
            let Some(tx) = tx_ref(&row) else {
                warn!(idempotency_key = %row.idempotency_key, "SUBMITTED row lacks tx reference; skipping");
                continue;
            };
            match self.backend.confirm(&tx).await {
                Ok(ConfirmationStatus::Confirmed { depth }) if depth >= self.finality_depth => {
                    info!(
                        idempotency_key = %row.idempotency_key,
                        tx_id = %tx.tx_id,
                        depth,
                        "settlement FINAL"
                    );
                    self.store.mark_final(&row.idempotency_key, depth).await?;
                    self.raise_settlement_record(&row.idempotency_key).await?;
                }
                Ok(ConfirmationStatus::Confirmed { depth }) => {
                    self.store.record_depth(&row.idempotency_key, depth).await?;
                }
                // Not (or no longer) on chain — a rollback resets the clock.
                // The follower re-detects inclusion on the new chain; depth
                // starts over from there.
                Ok(ConfirmationStatus::Pending) => {
                    if row.last_depth > 0 {
                        warn!(
                            idempotency_key = %row.idempotency_key,
                            tx_id = %tx.tx_id,
                            prior_depth = row.last_depth,
                            "settlement tx no longer confirmed (rollback?); depth reset"
                        );
                    }
                    self.store.record_depth(&row.idempotency_key, 0).await?;
                }
                Ok(ConfirmationStatus::Rejected) => {
                    warn!(
                        idempotency_key = %row.idempotency_key,
                        tx_id = %tx.tx_id,
                        "settlement tx rejected on chain — needs reconciliation"
                    );
                    self.store
                        .mark_error(&row.idempotency_key, "transaction rejected on chain", false)
                        .await?;
                }
                Err(e) => {
                    warn!(error = %e, tx_id = %tx.tx_id, "confirm failed; will retry next tick");
                }
            }
        }
        self.write_pending_settlement_records().await?;
        Ok(())
    }

    /// Raise the Settlement Reference record for a row that just went FINAL:
    /// store its salt and timestamp first, so the record can be rebuilt from
    /// the row and the outbox, then attempt the chain write.
    async fn raise_settlement_record(&self, idempotency_key: &str) -> Result<(), OutboxError> {
        let Some(row) = self.store.get(idempotency_key).await? else {
            return Ok(());
        };
        let salt = uuid::Uuid::new_v4().simple().to_string();
        // Whole seconds: the stored text must re-serialize to the same bytes
        // the commitment was computed over.
        let settled_at = Utc::now().with_nanosecond(0).unwrap_or_else(Utc::now);
        self.store
            .record_settlement_salt(idempotency_key, &salt, &settled_at.to_rfc3339_opts(SecondsFormat::Secs, true))
            .await?;
        let row = SettlementRow {
            record_salt: Some(salt),
            record_settled_at: Some(settled_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
            ..row
        };
        self.write_settlement_record(&row).await
    }

    /// Build the record from a FINAL row and write it; a failure is logged
    /// and left for the next tick.
    async fn write_settlement_record(&self, row: &SettlementRow) -> Result<(), OutboxError> {
        let (Some(salt), Some(settled_at)) = (row.record_salt.as_deref(), row.record_settled_at.as_deref()) else {
            return Ok(());
        };
        let Ok(settled_at) = chrono::DateTime::parse_from_rfc3339(settled_at) else {
            warn!(idempotency_key = %row.idempotency_key, "settlement record: unreadable record_settled_at; skipping");
            return Ok(());
        };
        let settlement_id = row.settlement_id.clone().unwrap_or_else(|| row.idempotency_key.clone());
        let record = SettlementRecord {
            bank_id: self.bank_id.clone(),
            snapshot_id: row.snapshot_id.clone().unwrap_or_else(|| settlement_id.clone()),
            net_amount: row.net_amount_minor.clone(),
            currency: row.currency.clone(),
            promise_tx_ids: self.outbox.promise_tx_ids_for_settlement(&settlement_id).await?,
            settled_at: settled_at.with_timezone(&Utc),
        };
        match self.notary.write_settlement(&record, salt.as_bytes()).await {
            Ok(tx) => {
                info!(idempotency_key = %row.idempotency_key, record_tx_id = %tx.tx_id, blockchain = %tx.chain, "Settlement Reference record written");
                self.store.record_settlement_tx(&row.idempotency_key, &tx.tx_id, &tx.chain).await
            }
            Err(e) => {
                warn!(idempotency_key = %row.idempotency_key, error = %e, "Settlement Reference record write failed; will retry");
                Ok(())
            }
        }
    }

    /// Retry the records that did not reach the chain when their settlements
    /// went FINAL, for [`RECORD_RETRY_WINDOW`] after that.
    async fn write_pending_settlement_records(&self) -> Result<(), OutboxError> {
        for row in self.store.list_final_without_record(WORKLIST_LIMIT).await? {
            if row.record_salt.is_none() {
                // Went FINAL before records existed: raise one now.
                self.raise_settlement_record(&row.idempotency_key).await?;
                continue;
            }
            let fresh = row
                .record_settled_at
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| Utc::now().signed_duration_since(t.with_timezone(&Utc)) < RECORD_RETRY_WINDOW)
                .unwrap_or(false);
            if fresh {
                self.write_settlement_record(&row).await?;
            }
        }
        Ok(())
    }
}

fn tx_ref(row: &SettlementRow) -> Option<TxReference> {
    Some(TxReference {
        chain: row.blockchain.clone()?,
        tx_id: row.tx_id.clone()?,
        submitted_at: chrono::DateTime::parse_from_rfc3339(&row.created_at)
            .ok()?
            .with_timezone(&chrono::Utc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settlement_store::{status, NewClaim};
    use obp_blockchain::mock::MockBackend;
    use obp_blockchain::settlement::{SettlementInstruction, SettlementOutcome};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A settlement backend whose `confirm` answers from a script.
    struct ScriptedConfirm {
        script: Mutex<VecDeque<ConfirmationStatus>>,
    }

    #[async_trait::async_trait]
    impl SettlementBackend for ScriptedConfirm {
        fn system(&self) -> &str {
            "scripted"
        }
        fn settles_from(&self) -> &str {
            "addr_test1me"
        }
        async fn settle(
            &self,
            _i: &SettlementInstruction,
        ) -> obp_blockchain::Result<SettlementOutcome> {
            unreachable!("watcher never settles")
        }
        async fn confirm(&self, _tx: &TxReference) -> obp_blockchain::Result<ConfirmationStatus> {
            Ok(self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("confirm called more times than scripted"))
        }
    }

    async fn watcher_with(
        script: Vec<ConfirmationStatus>,
        finality_depth: u32,
    ) -> (FinalityWatcher, SettlementStore) {
        watcher_with_notary(script, finality_depth, Arc::new(MockBackend::new())).await
    }

    async fn watcher_with_notary(
        script: Vec<ConfirmationStatus>,
        finality_depth: u32,
        notary: Arc<dyn BlockchainBackend>,
    ) -> (FinalityWatcher, SettlementStore) {
        let store = SettlementStore::connect_in_memory().await.unwrap();
        let watcher = FinalityWatcher {
            store: store.clone(),
            backend: Arc::new(ScriptedConfirm {
                script: Mutex::new(script.into()),
            }),
            notary,
            outbox: OutboxStore::connect_in_memory().await.unwrap(),
            bank_id: "rt.bank.a".into(),
            finality_depth,
            poll_interval: Duration::from_secs(3600), // unused: tests call tick()
        };
        (watcher, store)
    }

    /// A notary whose first `remaining` settlement writes fail.
    struct FailingNotary {
        inner: MockBackend,
        remaining: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl BlockchainBackend for FailingNotary {
        async fn write_promise(&self, p: &obp_blockchain::PromiseRecord) -> obp_blockchain::Result<TxReference> {
            self.inner.write_promise(p).await
        }
        async fn write_settlement(&self, r: &SettlementRecord, salt: &[u8]) -> obp_blockchain::Result<TxReference> {
            let failing = self
                .remaining
                .fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if failing {
                return Err(obp_blockchain::BlockchainError::Transport("ogmios unreachable".into()));
            }
            self.inner.write_settlement(r, salt).await
        }
        async fn write_exception(&self, e: &obp_blockchain::ExceptionRecord, salt: &[u8]) -> obp_blockchain::Result<TxReference> {
            self.inner.write_exception(e, salt).await
        }
        async fn confirm(&self, r: &TxReference) -> obp_blockchain::Result<ConfirmationStatus> {
            self.inner.confirm(r).await
        }
    }

    /// The mock's tx id for the record, rebuilt from the row and the outbox
    /// alone: proves the record is reproducible later.
    async fn expected_record_tx_id(w: &FinalityWatcher, row: &SettlementRow) -> String {
        let settlement_id = row.settlement_id.clone().unwrap();
        let record = SettlementRecord {
            bank_id: w.bank_id.clone(),
            snapshot_id: settlement_id.clone(),
            net_amount: row.net_amount_minor.clone(),
            currency: row.currency.clone(),
            promise_tx_ids: w.outbox.promise_tx_ids_for_settlement(&settlement_id).await.unwrap(),
            settled_at: chrono::DateTime::parse_from_rfc3339(row.record_settled_at.as_deref().unwrap())
                .unwrap()
                .with_timezone(&Utc),
        };
        MockBackend::new()
            .write_settlement(&record, row.record_salt.as_deref().unwrap().as_bytes())
            .await
            .unwrap()
            .tx_id
    }

    #[tokio::test]
    async fn reaching_finality_depth_writes_a_settlement_record() {
        let notary = Arc::new(MockBackend::new());
        let (w, store) = watcher_with_notary(vec![ConfirmationStatus::Confirmed { depth: 5 }], 5, notary.clone()).await;
        submitted_row(&store, "k1").await;
        // Two promises this settlement covered, one without a promise tx yet.
        for (id, tx) in [("tr-1", Some("promise-tx-1")), ("tr-2", Some("promise-tx-2")), ("tr-3", None)] {
            w.outbox
                .insert(crate::outbox::NewEntry {
                    transaction_request_id: id,
                    bank_id: "rt.bank.a",
                    account_id: "settlement-a",
                    request_payload: "{}",
                    commitment_salt: "salt",
                })
                .await
                .unwrap();
            let obp_id = format!("obp-{id}");
            w.outbox.mark_submitted(id, Some(&obp_id)).await.unwrap();
            if let Some(tx) = tx {
                w.outbox.mark_promise_written(id, tx, "mock").await.unwrap();
            }
            w.outbox.mark_settled(&[obp_id], "settle-1").await.unwrap();
        }

        w.tick().await.unwrap();

        let row = store.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::FINAL);
        assert_eq!(notary.written_kinds(), vec!["settlement"]);
        assert_eq!(row.record_blockchain.as_deref(), Some("mock"));
        assert_eq!(row.record_tx_id.clone().unwrap(), expected_record_tx_id(&w, &row).await);
        assert_eq!(
            w.outbox.promise_tx_ids_for_settlement("settle-1").await.unwrap(),
            vec!["promise-tx-1", "promise-tx-2"],
            "only rows with a promise on chain, in a stable order"
        );
        w.tick().await.unwrap();
        assert_eq!(notary.written_kinds().len(), 1, "written once");
    }

    #[tokio::test]
    async fn settlement_record_failure_leaves_final_and_retries_next_tick() {
        let notary = Arc::new(FailingNotary {
            inner: MockBackend::new(),
            // The tick that finalizes also retries once.
            remaining: std::sync::atomic::AtomicUsize::new(2),
        });
        let (w, store) = watcher_with_notary(vec![ConfirmationStatus::Confirmed { depth: 5 }], 5, notary.clone()).await;
        submitted_row(&store, "k1").await;

        w.tick().await.unwrap();
        let row = store.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::FINAL, "finality does not wait for the record");
        assert!(row.record_salt.is_some() && row.record_tx_id.is_none());

        w.tick().await.unwrap();
        let row = store.get("k1").await.unwrap().unwrap();
        assert_eq!(row.record_tx_id.clone().unwrap(), expected_record_tx_id(&w, &row).await);
        assert!(store.list_final_without_record(10).await.unwrap().is_empty());
    }

    async fn submitted_row(store: &SettlementStore, key: &str) {
        store
            .claim(NewClaim {
                idempotency_key: key,
                settlement_id: Some("settle-1"),
                snapshot_id: None,
                currency: "KES",
                net_amount_minor: 1_000,
                creditor_address: "addr_test1creditor",
                purpose: None,
            })
            .await
            .unwrap();
        store
            .mark_submitted(key, "tx-1", "cardano", "ADA", "10", None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn depth_below_threshold_records_but_does_not_finalize() {
        let (w, store) = watcher_with(vec![ConfirmationStatus::Confirmed { depth: 3 }], 5).await;
        submitted_row(&store, "k1").await;
        w.tick().await.unwrap();
        let row = store.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::SUBMITTED);
        assert_eq!(row.last_depth, 3);
    }

    #[tokio::test]
    async fn reaching_finality_depth_marks_final() {
        let (w, store) = watcher_with(
            vec![
                ConfirmationStatus::Confirmed { depth: 3 },
                ConfirmationStatus::Confirmed { depth: 5 },
            ],
            5,
        )
        .await;
        submitted_row(&store, "k1").await;
        w.tick().await.unwrap();
        w.tick().await.unwrap();
        let row = store.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::FINAL);
        assert_eq!(row.last_depth, 5);
        assert!(row.finalized_at.is_some());
    }

    #[tokio::test]
    async fn rollback_resets_depth_and_keeps_polling() {
        let (w, store) = watcher_with(
            vec![
                ConfirmationStatus::Confirmed { depth: 4 },
                ConfirmationStatus::Pending, // rolled back
                ConfirmationStatus::Confirmed { depth: 6 },
            ],
            5,
        )
        .await;
        submitted_row(&store, "k1").await;
        w.tick().await.unwrap();
        assert_eq!(store.get("k1").await.unwrap().unwrap().last_depth, 4);
        w.tick().await.unwrap();
        let row = store.get("k1").await.unwrap().unwrap();
        assert_eq!(
            row.status,
            status::SUBMITTED,
            "rollback keeps the row in play"
        );
        assert_eq!(row.last_depth, 0, "depth resets on rollback");
        w.tick().await.unwrap();
        assert_eq!(
            store.get("k1").await.unwrap().unwrap().status,
            status::FINAL
        );
    }

    #[tokio::test]
    async fn on_chain_rejection_is_a_sticky_error() {
        let (w, store) = watcher_with(vec![ConfirmationStatus::Rejected], 5).await;
        submitted_row(&store, "k1").await;
        w.tick().await.unwrap();
        let row = store.get("k1").await.unwrap().unwrap();
        assert_eq!(row.status, status::ERROR);
        assert!(!row.retryable, "funds ambiguity needs a human, not a retry");
    }

    #[tokio::test]
    async fn final_rows_leave_the_worklist() {
        let (w, store) = watcher_with(vec![ConfirmationStatus::Confirmed { depth: 9 }], 5).await;
        submitted_row(&store, "k1").await;
        w.tick().await.unwrap();
        // Second tick has an empty script; it must not call confirm again.
        w.tick().await.unwrap();
        assert_eq!(
            store.get("k1").await.unwrap().unwrap().status,
            status::FINAL
        );
    }
}

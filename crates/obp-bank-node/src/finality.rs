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

use obp_blockchain::settlement::SettlementBackend;
use obp_blockchain::{ConfirmationStatus, TxReference};
use tracing::{info, warn};

use crate::outbox::OutboxError;
use crate::settlement_store::{SettlementRow, SettlementStore};

/// Rows examined per tick. Far above realistic in-flight settlement counts;
/// bounds a tick's work if something backs up.
const WORKLIST_LIMIT: i64 = 100;

pub struct FinalityWatcher {
    pub store: SettlementStore,
    pub backend: Arc<dyn SettlementBackend>,
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
        let store = SettlementStore::connect_in_memory().await.unwrap();
        let watcher = FinalityWatcher {
            store: store.clone(),
            backend: Arc::new(ScriptedConfirm {
                script: Mutex::new(script.into()),
            }),
            finality_depth,
            poll_interval: Duration::from_secs(3600), // unused: tests call tick()
        };
        (watcher, store)
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

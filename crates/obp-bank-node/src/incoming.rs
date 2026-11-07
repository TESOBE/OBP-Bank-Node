//! Incoming settlement watcher — checks that settlements this bank receives as
//! creditor were really paid.
//!
//! OBP-API sends `obp_settlement_advice` when it nets the promises, before the
//! debtor bank's node has paid, so the advice alone means "netted on OBP-API's
//! ledger". This task follows each open incoming settlement:
//!
//! - asks OBP-API's settlement view for what the debtor's node reported paying
//!   (`settlement_tx_id`, `settlement_asset_amount`); until then it stays NETTED,
//! - asks the settlement backend what that transaction paid to this node's own
//!   address (the chain-sync follower records every such payment as it
//!   arrives, so it can be checked after the node has spent it),
//! - marks it RECEIVED once at least the reported amount has arrived at this
//!   node's finality depth, PAID while it is reported but not yet seen deep
//!   enough, and MISMATCH when less arrived than was reported, or when
//!   nothing has arrived within `paid_timeout` of the payment being reported
//!   (a MISMATCH is still re-checked, so a late arrival turns it RECEIVED).
//!
//! The check is against what the debtor reported paying, not an FX
//! conversion of the netted amount: the debtor converts at its own rate, so a
//! different rate here is not a mismatch.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use obp_blockchain::settlement::SettlementBackend;
use tracing::{info, warn};

use crate::obp_client::ObpClient;
use crate::settlement_store::{incoming_status, IncomingSettlement, IncomingUpdate, SettlementStore};

pub struct IncomingWatcher {
    pub store: SettlementStore,
    pub obp: Arc<ObpClient>,
    pub backend: Arc<dyn SettlementBackend>,
    pub bank_id: String,
    pub finality_depth: u32,
    pub poll_interval: Duration,
    /// How long a reported payment may stay unseen before it is a MISMATCH.
    pub paid_timeout: Duration,
}

/// Whether a payment reported at `paid_at` (RFC 3339) has been unseen for
/// longer than `timeout` as of `now`. An unparseable or absent `paid_at`
/// never times out: the row is just not that far yet.
fn paid_timed_out(paid_at: Option<&str>, now: DateTime<Utc>, timeout: Duration) -> bool {
    let Some(paid_at) = paid_at.and_then(|s| DateTime::parse_from_rfc3339(s).ok()) else {
        return false;
    };
    let elapsed = now.signed_duration_since(paid_at.with_timezone(&Utc));
    elapsed.to_std().map(|e| e >= timeout).unwrap_or(false)
}

impl IncomingWatcher {
    pub async fn run(self) {
        loop {
            match self.store.list_open_incoming().await {
                Ok(rows) => {
                    for row in rows {
                        self.check(&row).await;
                    }
                }
                Err(e) => warn!(error = %e, "incoming watcher: listing open settlements failed"),
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn check(&self, row: &IncomingSettlement) {
        let id = &row.settlement_id;
        let debtor = row.debtor_bank_id.as_deref().unwrap_or("the debtor bank");
        let view = match self.obp.get_settlement(&self.bank_id, id).await {
            Ok(v) => v.raw,
            Err(e) => {
                self.update(row, IncomingUpdate {
                    status: &row.status,
                    tx_id: None,
                    asset: None,
                    asset_amount: None,
                    received_amount: None,
                    depth: row.depth as u32,
                    detail: &format!("cannot read the settlement from OBP-API: {e}"),
                })
                .await;
                return;
            }
        };
        let str_of = |k: &str| view[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        let (tx_id, asset, asset_amount) = (
            str_of("settlement_tx_id"),
            str_of("settlement_asset"),
            str_of("settlement_asset_amount"),
        );
        let Some(tx_id) = tx_id else {
            let status = str_of("settlement_status").unwrap_or_else(|| "unknown".into());
            self.update(row, IncomingUpdate {
                status: incoming_status::NETTED,
                tx_id: None,
                asset: None,
                asset_amount: None,
                received_amount: None,
                depth: 0,
                detail: &format!("netted; waiting for {debtor} to pay (OBP-API reports {status})"),
            })
            .await;
            return;
        };
        let expected: Option<u64> = asset_amount.as_deref().and_then(|a| a.parse().ok());
        // Once a reported payment has gone unseen for the timeout, the row is
        // a MISMATCH until something arrives; a transient chain error must not
        // flip it back to PAID.
        let timed_out = paid_timed_out(row.paid_at.as_deref(), Utc::now(), self.paid_timeout);
        let unseen_status = if timed_out { incoming_status::MISMATCH } else { incoming_status::PAID };
        let base = IncomingUpdate {
            status: unseen_status,
            tx_id: Some(&tx_id),
            asset: asset.as_deref(),
            asset_amount: asset_amount.as_deref(),
            received_amount: None,
            depth: 0,
            detail: "",
        };
        let payment = match self.backend.incoming_payment(&tx_id).await {
            Ok(p) => p,
            Err(e) => {
                let detail = format!("{debtor} reports paying; cannot check the chain: {e}");
                self.update(row, IncomingUpdate { detail: &detail, ..base }).await;
                return;
            }
        };
        let Some(p) = payment else {
            let detail = if timed_out {
                let hours = self.paid_timeout.as_secs() / 3600;
                let since = row.paid_at.as_deref().unwrap_or("?");
                if row.status != incoming_status::MISMATCH {
                    warn!(settlement_id = %id, %tx_id, since, "incoming settlement reported paid but never seen; MISMATCH");
                }
                format!(
                    "{debtor} reported paying {tx_id} at {since}; nothing arrived at this node's \
                     address within {hours} h"
                )
            } else {
                format!("{debtor} reports paying; not seen arriving at this node's address yet")
            };
            self.update(row, IncomingUpdate { detail: &detail, ..base }).await;
            return;
        };
        let seen = IncomingUpdate { received_amount: Some(p.lovelace), depth: p.depth, ..base };
        match expected {
            Some(exp) if p.lovelace < exp => {
                let detail = format!(
                    "received {} lovelace, but {debtor} reports paying {exp}",
                    p.lovelace
                );
                self.update(row, IncomingUpdate { status: incoming_status::MISMATCH, detail: &detail, ..seen })
                    .await;
            }
            _ if p.depth >= self.finality_depth => {
                let detail = format!(
                    "received {} lovelace at depth {}/{}",
                    p.lovelace, p.depth, self.finality_depth
                );
                if row.status != incoming_status::RECEIVED {
                    info!(settlement_id = %id, tx_id = %tx_id, lovelace = p.lovelace, "incoming settlement received");
                }
                self.update(row, IncomingUpdate { status: incoming_status::RECEIVED, detail: &detail, ..seen })
                    .await;
            }
            _ => {
                let detail = format!(
                    "arrived ({} lovelace); waiting for depth {}/{}",
                    p.lovelace, p.depth, self.finality_depth
                );
                self.update(row, IncomingUpdate { detail: &detail, ..seen }).await;
            }
        }
    }

    async fn update(&self, row: &IncomingSettlement, u: IncomingUpdate<'_>) {
        if let Err(e) = self.store.update_incoming(&row.settlement_id, u).await {
            warn!(settlement_id = %row.settlement_id, error = %e, "incoming watcher: update failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obp_client::ObpAuth;
    use axum::{routing::get, Json, Router};
    use obp_blockchain::settlement::{IncomingPayment, SettlementInstruction, SettlementOutcome};
    use obp_blockchain::{ConfirmationStatus, TxReference};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn paid_timed_out_is_false_within_window_true_after() {
        let now = DateTime::parse_from_rfc3339("2026-11-07T12:00:00Z").unwrap().with_timezone(&Utc);
        let h = Duration::from_secs(3600);
        assert!(!paid_timed_out(None, now, h), "never reported paid");
        assert!(!paid_timed_out(Some("garbage"), now, h));
        assert!(!paid_timed_out(Some("2026-11-07T11:30:00Z"), now, h), "30 min in, 1 h window");
        assert!(paid_timed_out(Some("2026-11-07T11:00:00Z"), now, h), "exactly the window");
        assert!(paid_timed_out(Some("2026-11-06T12:00:00Z"), now, h));
    }

    /// A settlement backend whose chain answers are scripted, oldest first.
    struct ScriptedChain {
        answers: Mutex<VecDeque<obp_blockchain::Result<Option<IncomingPayment>>>>,
    }

    #[async_trait::async_trait]
    impl SettlementBackend for ScriptedChain {
        fn system(&self) -> &str {
            "scripted"
        }
        fn settles_from(&self) -> &str {
            "addr_me"
        }
        async fn settle(&self, _: &SettlementInstruction) -> obp_blockchain::Result<SettlementOutcome> {
            unreachable!("the incoming watcher never settles")
        }
        async fn confirm(&self, _: &TxReference) -> obp_blockchain::Result<ConfirmationStatus> {
            unreachable!("the incoming watcher never confirms")
        }
        async fn incoming_payment(&self, _: &str) -> obp_blockchain::Result<Option<IncomingPayment>> {
            self.answers.lock().unwrap().pop_front().unwrap_or(Ok(None))
        }
    }

    /// OBP-API's settlement view, saying the debtor paid `tx-paid`.
    async fn spawn_settlement_view() -> String {
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/open-corridor/settlements/:id",
            get(|| async {
                Json(serde_json::json!({
                    "settlement_id": "set-1",
                    "settlement_status": "SETTLED",
                    "settlement_tx_id": "tx-paid",
                    "settlement_asset": "ADA",
                    "settlement_asset_amount": "5000000"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    async fn watcher(
        answers: Vec<obp_blockchain::Result<Option<IncomingPayment>>>,
        paid_timeout: Duration,
    ) -> IncomingWatcher {
        let store = SettlementStore::connect_in_memory().await.unwrap();
        store
            .record_incoming("set-1", Some("rt.bank.a"), Some("KES"), Some("100.00"), &["tr-1".into()])
            .await
            .unwrap();
        let base = spawn_settlement_view().await;
        IncomingWatcher {
            store,
            obp: Arc::new(ObpClient::new(base, ObpAuth::None).unwrap()),
            backend: Arc::new(ScriptedChain { answers: Mutex::new(answers.into()) }),
            bank_id: "rt.bank.b".into(),
            finality_depth: 2,
            poll_interval: Duration::from_secs(1),
            paid_timeout,
        }
    }

    async fn tick(w: &IncomingWatcher) -> IncomingSettlement {
        let row = w.store.list_open_incoming().await.unwrap().remove(0);
        w.check(&row).await;
        w.store.list_incoming(10).await.unwrap().remove(0)
    }

    #[tokio::test]
    async fn paid_without_arrival_becomes_mismatch_after_timeout() {
        let w = watcher(vec![Ok(None), Ok(None)], Duration::ZERO).await;
        let row = tick(&w).await;
        assert_eq!(row.status, incoming_status::PAID, "first seen reported: PAID, clock starts");
        assert!(row.paid_at.is_some());
        let row = tick(&w).await;
        assert_eq!(row.status, incoming_status::MISMATCH);
        let detail = row.detail.unwrap_or_default();
        assert!(detail.contains("tx-paid") && detail.contains("nothing arrived"), "{detail}");
    }

    #[tokio::test]
    async fn late_arrival_turns_mismatch_into_received() {
        let deep = IncomingPayment { lovelace: 5_000_000, depth: 3 };
        let w = watcher(vec![Ok(None), Ok(None), Ok(Some(deep))], Duration::ZERO).await;
        tick(&w).await;
        assert_eq!(tick(&w).await.status, incoming_status::MISMATCH);
        let row = tick(&w).await;
        assert_eq!(row.status, incoming_status::RECEIVED);
        assert_eq!(row.received_amount.as_deref(), Some("5000000"));
    }

    #[tokio::test]
    async fn a_chain_error_does_not_undo_a_timeout_mismatch() {
        let w = watcher(
            vec![Ok(None), Ok(None), Err(obp_blockchain::BlockchainError::Transport("ogmios down".into()))],
            Duration::ZERO,
        )
        .await;
        tick(&w).await;
        tick(&w).await;
        let row = tick(&w).await;
        assert_eq!(row.status, incoming_status::MISMATCH);
        assert!(row.detail.unwrap_or_default().contains("cannot check the chain"));
    }

    #[tokio::test]
    async fn within_the_window_an_unseen_payment_stays_paid() {
        let w = watcher(vec![Ok(None), Ok(None)], Duration::from_secs(3600)).await;
        tick(&w).await;
        assert_eq!(tick(&w).await.status, incoming_status::PAID);
    }
}

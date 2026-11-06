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
//!   enough, and MISMATCH when less arrived than was reported.
//!
//! The check is against what the debtor reported paying, not an FX
//! conversion of the netted amount: the debtor converts at its own rate, so a
//! different rate here is not a mismatch.

use std::sync::Arc;
use std::time::Duration;

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
        let base = IncomingUpdate {
            status: incoming_status::PAID,
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
            let detail = format!("{debtor} reports paying; not seen arriving at this node's address yet");
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

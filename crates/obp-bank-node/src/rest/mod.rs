//! South-side REST API (Interface A).
//!
//! Mounts four endpoints under `/obp-bank-node/v5.1.0/` plus root `/health`.
//! URLs are intentionally short: one Bank Node serves exactly one bank, so
//! `bank_id`, `account_id`, and `view_id` are not on the URL — the Bank Node
//! knows them from config (see [`BankNodeState`]). The request body for
//! payment initiation still mirrors the OBP `OPEN_CORRIDOR_PROMISE` Transaction Request
//! shape so banks already familiar with OBP can reuse their validators.

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};

use crate::evidence::EvidenceStore;
use crate::obp_client::ObpClient;
use crate::outbox::OutboxStore;
use crate::routing::RoutingRegistry;
use crate::settlement_store::SettlementStore;

pub mod handlers;
pub mod types;

#[cfg(test)]
mod tests;

/// Shared state for the south-side handlers. Payment initiation only persists
/// to the outbox (the external work is the dispatcher's job), and the store
/// endpoints are read-only projections. The one deliberate exception is the
/// settle trigger, which calls OBP-API synchronously through `obp` — the
/// caller needs OBP's netting answer, and the value leg still runs
/// asynchronously (Interface C instruction → settlement backend).
#[derive(Clone)]
pub struct BankNodeState {
    pub outbox: OutboxStore,
    pub settlements: SettlementStore,
    pub evidence: EvidenceStore,
    pub obp: Arc<ObpClient>,
    /// Cached OBP-API routing-scheme registry for beneficiary-routing
    /// validation at initiation. Fail-open while unloaded.
    pub routing: RoutingRegistry,
    pub blockchain_label: &'static str,
    pub bank_id: String,
    pub account_id: String,
    /// Depth at which this node's finality watcher promotes settlements to
    /// FINAL — surfaced on the settlement read endpoints.
    pub finality_depth: u32,
}

pub fn build_router(state: BankNodeState) -> Router {
    use handlers::*;

    Router::new()
        .route(
            "/obp-bank-node/v5.1.0/transaction-requests",
            post(initiate_payment).get(list_transaction_requests),
        )
        .route(
            "/obp-bank-node/v5.1.0/transaction-requests/:transaction_request_id",
            get(get_transaction_request),
        )
        .route(
            "/obp-bank-node/v5.1.0/settlements",
            post(request_settlement).get(list_settlements),
        )
        .route(
            "/obp-bank-node/v5.1.0/settlements/:key",
            get(get_settlement),
        )
        .route(
            "/obp-bank-node/v5.1.0/settlements/:key/corridor",
            get(get_corridor_settlement),
        )
        .route("/obp-bank-node/v5.1.0/evidence", get(list_evidence))
        .route(
            "/obp-bank-node/v5.1.0/evidence/:transaction_request_id",
            get(get_evidence),
        )
        .route("/obp-bank-node/v5.1.0/health", get(root_health))
        .route("/health", get(root_health))
        .with_state(state)
}

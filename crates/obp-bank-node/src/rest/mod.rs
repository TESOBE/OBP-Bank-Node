//! South-side REST API (Interface A).
//!
//! Mounts four endpoints under `/obp-bank-node/v5.1.0/` plus root `/health`.
//! URLs are intentionally short: one Bank Node serves exactly one bank, so
//! `bank_id`, `account_id`, and `view_id` are not on the URL — the Bank Node
//! knows them from config (see [`BankNodeState`]). The request body for
//! payment initiation still mirrors the OBP `OPEN_CORRIDOR` Transaction Request
//! shape so banks already familiar with OBP can reuse their validators.

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use obp_blockchain::BlockchainBackend;

pub mod handlers;
pub mod types;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct BankNodeState {
    pub backend: Arc<dyn BlockchainBackend>,
    pub blockchain_label: &'static str,
    pub bank_id: String,
    pub account_id: String,
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
        .route("/obp-bank-node/v5.1.0/health", get(root_health))
        .route("/health", get(root_health))
        .with_state(state)
}

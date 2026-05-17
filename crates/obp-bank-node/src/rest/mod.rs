//! South-side REST API (Interface A).
//!
//! Mounts the OBP-API-shaped endpoints under the `/obp-bank-node/v5.1.0/`
//! prefix, plus the root `/health` endpoint.
//!
//! Only `v5.1.0` is supported. Older or newer version prefixes can be added
//! back if a deployer actually requires them — for the current state of the
//! project it's just route duplication.

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use obp_blockchain::BlockchainConnector;

pub mod handlers;
pub mod types;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct BankNodeState {
    pub connector: Arc<dyn BlockchainConnector>,
    pub blockchain_label: &'static str,
}

pub fn build_router(state: BankNodeState) -> Router {
    use handlers::*;

    Router::new()
        .route(
            "/obp-bank-node/v5.1.0/banks/:bank_id/accounts/:account_id/views/:view_id\
             /transaction-request-types/SIMPLE/transaction-requests",
            post(initiate_payment),
        )
        .route(
            "/obp-bank-node/v5.1.0/banks/:bank_id/accounts/:account_id/views/:view_id\
             /transaction-requests/:transaction_request_id",
            get(get_transaction_request),
        )
        .route(
            "/obp-bank-node/v5.1.0/banks/:bank_id/accounts/:account_id/views/:view_id\
             /transaction-requests",
            get(list_transaction_requests),
        )
        .route("/obp-bank-node/v5.1.0/health", get(root_health))
        .route("/health", get(root_health))
        .with_state(state)
}

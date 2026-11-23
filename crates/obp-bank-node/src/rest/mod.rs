// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! South-side REST API (Interface A).
//!
//! Mounts the south-side endpoints under `/obp-bank-node/v5.1.0/` plus root `/alive`, `/health`, `/ready` and `/status` (in OBP-API's format, see [`status_page`]).
//!
//! A Bank Node serves one bank, which may be on several OBP hosts, with a different `bank_id` on each. So the URLs name the OBP host and the node's bank on it, as OBP-API names a bank: `/obp-bank-node/v5.1.0/hosts/HOST/banks/BANK_ID/...`. Everything after `/hosts/HOST` reads as it would on that OBP-API. HOST is the host (and port) of that OBP-API's base URL, for example `localhost:8081`. The request body for payment initiation still mirrors the OBP `OPEN_CORRIDOR_PROMISE` Transaction Request shape, so banks already familiar with OBP can reuse their validators.

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
pub mod status_page;
pub mod types;

#[cfg(test)]
mod tests;

/// One OBP host the node is on, and the node's bank there.
#[derive(Clone)]
pub struct HostState {
    /// The OBP host: the host (and port) of its OBP-API's base URL, as in the URLs.
    pub host: String,
    /// That OBP-API's base URL, checked by the root `/status` page.
    pub obp_api_url: String,
    /// The OBP-API client for this OBP host, logged in as this node's Consumer there.
    pub obp: Arc<ObpClient>,
    /// Cached OBP-API routing-scheme registry for beneficiary-routing validation at initiation. Fail-open while unloaded.
    pub routing: RoutingRegistry,
    /// This node's bank on this OBP host.
    pub bank_id: String,
    /// The single-currency shorthand corridor account on this OBP host.
    pub account_id: String,
    /// Currency (uppercase ISO 4217) → corridor account id. Empty = the single-currency shorthand: every currency sends from `account_id`.
    pub corridor_accounts: std::collections::HashMap<String, String>,
    /// Interface C consumer state for this OBP host, surfaced on `/health` so a node that cannot receive AMQP messages is visible instead of silently green.
    pub interface_c: crate::interface_c::consumer::ConsumerStatus,
}

impl HostState {
    /// The corridor account that sends `currency` (matched case-insensitively). `None` means the currency has no configured account and the request must be rejected (`OBP-BANK-NODE-ROUTING-001`) — a non-empty map never falls back to `account_id`.
    pub fn corridor_account_for(&self, currency: &str) -> Option<&str> {
        // No per-currency accounts: the shorthand account sends everything.
        if self.corridor_accounts.is_empty() {
            // The shorthand account.
            Some(&self.account_id)
        } else {
            // The account configured for this currency, if any.
            self.corridor_accounts
                // Keys are stored in upper case.
                .get(currency.trim().to_uppercase().as_str())
                // Borrow the account id.
                .map(String::as_str)
        }
    }
}

/// Shared state for the south-side handlers. Payment initiation only persists to the outbox (the external work is the dispatcher's job), and the store endpoints are read-only projections. The one deliberate exception is the settle trigger, which calls OBP-API synchronously through the OBP host's client — the caller needs OBP's netting answer, and the value leg still runs asynchronously (Interface C instruction → settlement backend).
#[derive(Clone)]
pub struct BankNodeState {
    /// The node's name, from its config, shown on its status pages.
    pub name: String,
    pub outbox: OutboxStore,
    pub settlements: SettlementStore,
    pub evidence: EvidenceStore,
    pub blockchain_label: &'static str,
    /// The OBP hosts the node is on, in config order.
    pub hosts: Arc<Vec<HostState>>,
    /// Depth at which this node's finality watcher promotes settlements to FINAL — surfaced on the settlement read endpoints.
    pub finality_depth: u32,
    /// The chain's health URL (Ogmios's `/health` for Cardano; `None` for the mock), checked by the root `/status` page.
    pub chain_health_url: Option<String>,
    /// Identifies this run of the node on `/status`; new on every start.
    pub instance_id: String,
    pub started_at: std::time::Instant,
}

impl BankNodeState {
    /// The OBP host with this name, if the node is on it.
    pub fn host(&self, obp_host: &str) -> Option<&HostState> {
        // Look through the configured OBP hosts.
        self.hosts.iter().find(|host_state| host_state.host == obp_host)
    }
}

pub fn build_router(state: BankNodeState) -> Router {
    use handlers::*;

    // Everything about this node's bank on one OBP host starts here.
    const BANK: &str = "/obp-bank-node/v5.1.0/hosts/:obp_host/banks/:bank_id";

    Router::new()
        // The OBP hosts the node is on, with its bank on each.
        .route("/obp-bank-node/v5.1.0/hosts", get(list_hosts))
        // The banks on one OBP host, relayed from that OBP-API's getBanks, as OBP-API's own GET /banks.
        .route("/obp-bank-node/v5.1.0/hosts/:obp_host/banks", get(list_banks))
        // Send a payment from this node's bank on the OBP host, and list the payments it sent.
        .route(&format!("{BANK}/transaction-requests"), post(initiate_payment).get(list_transaction_requests))
        // One payment.
        .route(&format!("{BANK}/transaction-requests/:transaction_request_id"), get(get_transaction_request))
        // Settle with another bank on the OBP host, and list this bank's settlements there.
        .route(&format!("{BANK}/settlements"), post(request_settlement).get(list_settlements))
        // One settlement.
        .route(&format!("{BANK}/settlements/:key"), get(get_settlement))
        // One settlement as OBP-API sees it.
        .route(&format!("{BANK}/settlements/:key/corridor"), get(get_corridor_settlement))
        // Settlements this bank receives as creditor.
        .route(&format!("{BANK}/incoming-settlements"), get(list_incoming_settlements))
        // Credits this bank received.
        .route(&format!("{BANK}/evidence"), get(list_evidence))
        // One received credit.
        .route(&format!("{BANK}/evidence/:transaction_request_id"), get(get_evidence))
        // The node itself, across its OBP hosts.
        .route("/obp-bank-node/v5.1.0/health", get(root_health))
        .route("/obp-bank-node/v5.1.0/status", get(root_status))
        // Same format as OBP-API's: see status_page.
        .route("/alive", get(status_page::alive))
        .route("/health", get(status_page::health))
        .route("/ready", get(status_page::ready))
        .route("/status", get(status_page::status))
        .with_state(state)
}

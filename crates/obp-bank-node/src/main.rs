// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! OBP Bank Node — main binary.
//!
//! Wires together config, the blockchain backend, and the south-side REST
//! router. AMQP consumer, outbox, delivery modes, and OBP API client will
//! land in dedicated modules over subsequent commits.

mod cbs;
mod dispatcher;
mod evidence;
mod finality;
mod fx;
mod incoming;
mod interface_c;
mod obp_client;
mod outbox;
mod rest;
mod routing;
mod settlement_store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use obp_blockchain::cardano::{CardanoAdaSettlement, CardanoConfig};
use obp_blockchain::mock::MockBackend;
use obp_blockchain::settlement::{FxSource, SettlementBackend, StubFxSource};
use obp_blockchain::BlockchainBackend;

use crate::cbs::CbsClient;
use crate::dispatcher::{Dispatcher, DispatcherConfig};
use crate::evidence::EvidenceStore;
use crate::finality::FinalityWatcher;
use crate::obp_client::{ObpAuth, ObpClient};
use crate::outbox::OutboxStore;
use crate::rest::{build_router, BankNodeState, HostState};
use crate::settlement_store::SettlementStore;

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    /// The node's name, shown on its status pages so people can tell nodes apart, for example `node-a`. The same name the Bank Node UI's config gives the node is a good choice.
    name: String,
    server: ServerConfig,
    /// The OBP hosts the node is on, each with the node's bank there. A bank on one OBP host has a list of one; a hub bank lists several.
    hosts: Vec<HostConfig>,
    blockchain: BlockchainConfig,
    outbox: OutboxConfig,
    cbs_delivery: CbsDeliveryConfig,
    evidence: EvidenceConfig,
    settlement: SettlementConfig,
    /// Shared secret sent as the bearer token on the A2 CBS webhook.
    #[serde(default)]
    local_secret: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            // A name for a node started without a config file.
            name: "obp-bank-node".into(),
            server: ServerConfig {
                bind: "0.0.0.0:8088".into(),
            },
            // One placeholder OBP host, so the binary boots without a config file.
            hosts: vec![HostConfig::default()],
            blockchain: BlockchainConfig {
                kind: BlockchainKind::Mock,
                cardano: CardanoConfig::default(),
            },
            outbox: OutboxConfig::default(),
            cbs_delivery: CbsDeliveryConfig::default(),
            evidence: EvidenceConfig::default(),
            settlement: SettlementConfig::default(),
            local_secret: None,
        }
    }
}

/// One OBP host the node is on: how to reach its OBP-API (Interface B), its RabbitMQ (Interface C), and the node's bank there. The default is a placeholder, so the binary boots without a config file.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
struct HostConfig {
    /// How to reach this OBP host's OBP-API. Its base URL also names the OBP host.
    obp_api: ObpApiConfig,
    /// The RabbitMQ this OBP host's OBP-API sends messages to the node through. Off unless set.
    #[serde(default)]
    rabbitmq: RabbitMqConfig,
    /// The node's bank on this OBP host.
    bank: BankConfig,
}

/// Settlement (value-leg) configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct SettlementConfig {
    /// Which settle-time FX source to use. `coingecko` (default) queries the
    /// live CoinGecko price; `stub` uses `ada_rate_minor_per_whole_ada`
    /// (tests / offline development only).
    #[serde(default)]
    fx_source: FxSourceKind,
    /// CoinGecko base URL — override for tests or a proxy.
    #[serde(default = "default_fx_base_url")]
    fx_base_url: String,
    #[serde(default = "default_fx_timeout_secs")]
    fx_timeout_secs: u64,
    /// Cross-rate crypto-leg sources (`coingecko` / `api3` / `pyth`), tried
    /// in order until one answers. A single string or a list; the default
    /// `[pyth, coingecko]` prefers Pyth and falls back. Naming legs pins the
    /// order exactly — no implicit extras.
    #[serde(default = "default_fx_crypto_legs")]
    fx_crypto_leg: OneOrMany<FxCryptoLeg>,
    /// Cross-rate fiat-leg sources (`er-api` / `pyth`), tried in order. The
    /// default `[pyth, er-api]` prefers Pyth and falls back — necessary while
    /// Pyth's corridor fiat feeds are mostly `coming_soon` (see
    /// [`FxFiatLeg`]).
    #[serde(default = "default_fx_fiat_legs")]
    fx_fiat_leg: OneOrMany<FxFiatLeg>,
    /// Cross-rate er-api fiat leg base URL (USD table with the corridor fiats).
    #[serde(default = "default_fx_fiat_base_url")]
    fx_fiat_base_url: String,
    /// Pyth Hermes base URL for the `pyth` legs.
    #[serde(default = "default_fx_hermes_base_url")]
    fx_hermes_base_url: String,
    /// Refuse a Pyth price published longer ago than this, seconds. Pyth FX
    /// feeds pause outside forex market hours, so the default spans a
    /// weekend.
    #[serde(default = "default_fx_pyth_max_stale_secs")]
    fx_pyth_max_stale_secs: u64,
    /// EVM JSON-RPC endpoint for the API3 leg.
    #[serde(default)]
    fx_api3_rpc_url: String,
    /// `Api3ReaderProxyV1` address of the ADA/USD feed (API3 Market).
    #[serde(default)]
    fx_api3_proxy_address: String,
    /// Stub-source rate: minor units of the book currency per 1 whole ADA
    /// (e.g. 3542 = 1 ADA ≈ 35.42 KES). `u64` because figment's value model
    /// cannot round-trip `u128`; widened at the `StubFxSource` boundary.
    ada_rate_minor_per_whole_ada: u64,
    /// Confirmation depth at which a settlement transfer counts as FINAL.
    /// Broadcast (`SUBMITTED`) is not settlement — the finality watcher
    /// promotes rows at this depth. 15 blocks ≈ 5 minutes on Cardano.
    #[serde(default = "default_finality_depth")]
    finality_depth: u32,
    /// Finality watcher poll interval, seconds.
    #[serde(default = "default_finality_poll_secs")]
    finality_poll_secs: u64,
    /// How often the incoming-settlement watcher checks what this bank is owed.
    #[serde(default = "default_incoming_poll_secs")]
    incoming_poll_secs: u64,
    /// How long a settlement the debtor reports paying may stay unseen at this
    /// node's address before it is marked MISMATCH for an operator.
    #[serde(default = "default_incoming_timeout_secs")]
    incoming_timeout_secs: u64,
    /// Durable settlement idempotency/finality store (SQLite).
    #[serde(default = "default_settlement_store_path")]
    store_path: PathBuf,
}

fn default_finality_depth() -> u32 {
    15
}
fn default_finality_poll_secs() -> u64 {
    30
}

fn default_incoming_poll_secs() -> u64 {
    20
}

fn default_incoming_timeout_secs() -> u64 {
    24 * 60 * 60
}
fn default_settlement_store_path() -> PathBuf {
    PathBuf::from("./outbox/settlements.db")
}
fn default_fx_base_url() -> String {
    "https://api.coingecko.com".to_string()
}
fn default_fx_fiat_base_url() -> String {
    "https://open.er-api.com".to_string()
}
fn default_fx_hermes_base_url() -> String {
    "https://hermes.pyth.network".to_string()
}
fn default_fx_pyth_max_stale_secs() -> u64 {
    259_200 // 72h: Friday close → Monday open
}
fn default_fx_crypto_legs() -> OneOrMany<FxCryptoLeg> {
    OneOrMany::Many(vec![FxCryptoLeg::Pyth, FxCryptoLeg::Coingecko])
}
fn default_fx_fiat_legs() -> OneOrMany<FxFiatLeg> {
    OneOrMany::Many(vec![FxFiatLeg::Pyth, FxFiatLeg::ErApi])
}
fn default_fx_timeout_secs() -> u64 {
    10
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum FxSourceKind {
    /// Direct asset/fiat quote — only for fiats CoinGecko's `vs_currencies`
    /// still carries (KES is not among them since 2026-07).
    Coingecko,
    /// asset/USD × USD/fiat. The crypto leg is `fx_crypto_leg`; the fiat leg
    /// is `fx_fiat_base_url` (open.er-api.com). Default: the corridor fiats
    /// need it and it degrades to nothing when CoinGecko carries the fiat.
    #[default]
    Cross,
    Stub,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum FxCryptoLeg {
    Coingecko,
    /// API3 dAPI via `Api3ReaderProxyV1.read()` — needs `fx_api3_rpc_url` and
    /// `fx_api3_proxy_address` (from the API3 Market, per feed and chain).
    Api3,
    /// Pyth `Crypto.ADA/USD` via the Hermes endpoint (`fx_hermes_base_url`).
    /// Preferred by default: publishes continuously, keyless, and is the feed
    /// the Pyth Cardano integration serves on-chain.
    #[default]
    Pyth,
}

/// A config value that is either one `T` or a preference-ordered list of `T`.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T: Clone> OneOrMany<T> {
    fn to_vec(&self) -> Vec<T> {
        match self {
            OneOrMany::One(t) => vec![t.clone()],
            OneOrMany::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
enum FxFiatLeg {
    /// open.er-api.com's daily USD table (`fx_fiat_base_url`).
    #[default]
    #[serde(rename = "er-api")]
    ErApi,
    /// Pyth `FX.USD/*` via Hermes. Only for fiats whose feed actually
    /// publishes — as of 2026-08 that is ZAR alone of the corridor set; the
    /// others are listed `coming_soon` and are refused at quote time.
    #[serde(rename = "pyth")]
    Pyth,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        SettlementConfig {
            fx_source: FxSourceKind::default(),
            fx_base_url: default_fx_base_url(),
            fx_timeout_secs: default_fx_timeout_secs(),
            fx_crypto_leg: default_fx_crypto_legs(),
            fx_fiat_leg: default_fx_fiat_legs(),
            fx_fiat_base_url: default_fx_fiat_base_url(),
            fx_hermes_base_url: default_fx_hermes_base_url(),
            fx_pyth_max_stale_secs: default_fx_pyth_max_stale_secs(),
            fx_api3_rpc_url: String::new(),
            fx_api3_proxy_address: String::new(),
            ada_rate_minor_per_whole_ada: 3542,
            finality_depth: default_finality_depth(),
            finality_poll_secs: default_finality_poll_secs(),
            incoming_poll_secs: default_incoming_poll_secs(),
            incoming_timeout_secs: default_incoming_timeout_secs(),
            store_path: default_settlement_store_path(),
        }
    }
}

/// Interface C — inbound RabbitMQ connection. `enabled` defaults to false so the
/// node boots (and tests run) without a broker; set it true for the corridor.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct RabbitMqConfig {
    #[serde(default)]
    enabled: bool,
    host: String,
    port: u16,
    username: String,
    password: String,
    /// The bank's vhost, e.g. `/bank.ke.01.kcs` (per-bank isolation).
    virtual_host: String,
    request_queue: String,
}

impl Default for RabbitMqConfig {
    fn default() -> Self {
        RabbitMqConfig {
            enabled: false,
            host: "localhost".into(),
            port: 5672,
            username: "guest".into(),
            password: "guest".into(),
            virtual_host: "/".into(),
            request_queue: "obp_rpc_queue".into(),
        }
    }
}

/// Interface A2 — how credits are delivered to the bank's CBS. Only the
/// `webhook_obp` mode is wired today.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct CbsDeliveryConfig {
    #[serde(default)]
    mode: String,
    webhook: CbsWebhookConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct CbsWebhookConfig {
    url: String,
    #[serde(default = "default_cbs_timeout")]
    timeout_seconds: u64,
}

fn default_cbs_timeout() -> u64 {
    30
}

impl Default for CbsDeliveryConfig {
    fn default() -> Self {
        CbsDeliveryConfig {
            mode: "webhook_obp".into(),
            webhook: CbsWebhookConfig {
                url: "http://localhost:9000/credit-notifications".into(),
                timeout_seconds: 30,
            },
        }
    }
}

/// Where the beneficiary-side evidence store (received credit notifications +
/// salts) lives.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct EvidenceConfig {
    path: PathBuf,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        EvidenceConfig {
            path: PathBuf::from("./outbox/evidence.db"),
        }
    }
}

/// Interface B — how to reach one OBP host's OBP-API. The Bank Node logs in as its own Consumer there: OAuth2 client credentials (`token_url`, `client_id_env` and `client_secret_env`) or a pre-obtained DirectLogin token (`direct_login_token_env`). Secrets are never written in the config file: these settings name the environment variables that hold them. With none set the client runs unauthenticated, which is fine against a local or mock OBP-API.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct ObpApiConfig {
    /// OBP-API's base URL. Its host (with the port, when the URL names one) is how the Bank Node and its apps identify this OBP host.
    base_url: String,
    /// OAuth2 token endpoint for the client-credentials grant.
    #[serde(default)]
    token_url: String,
    /// The environment variable holding the node's OAuth2 client id (its Consumer key) on this OBP host.
    #[serde(default)]
    client_id_env: String,
    /// The environment variable holding the node's OAuth2 client secret on this OBP host.
    #[serde(default)]
    client_secret_env: String,
    #[serde(default)]
    scope: Option<String>,
    /// Alternative to client credentials: the environment variable holding a pre-obtained DirectLogin token.
    #[serde(default)]
    direct_login_token_env: String,
    /// How often to refresh the cached routing-scheme registry used for A1.1
    /// beneficiary-routing validation. `0` disables the refresher entirely —
    /// the registry then never loads and validation stays off (fail-open).
    #[serde(default = "default_routing_refresh_secs")]
    routing_refresh_secs: u64,
}

/// The OBP host the node is on: the host of `obp_api.base_url`, with the port
/// when the URL names one (`localhost:8081`, `apisandbox.openbankproject.com`).
/// Falls back to the base URL itself when it cannot be parsed.
fn obp_host(obp_api: &ObpApiConfig) -> String {
    // Parse the base URL; a URL that does not parse is reported as written.
    let Ok(url) = reqwest::Url::parse(&obp_api.base_url) else {
        // Use the base URL itself.
        return obp_api.base_url.clone();
    };
    // The host part, or the whole URL when it has none.
    let Some(host) = url.host_str() else {
        // Use the base URL itself.
        return obp_api.base_url.clone();
    };
    // `port()` is only set when the URL names a port other than the scheme's default.
    match url.port() {
        // Host and port, as in `localhost:8081`.
        Some(port) => format!("{host}:{port}"),
        // The host alone, as in `apisandbox.openbankproject.com`.
        None => host.to_string(),
    }
}

fn default_routing_refresh_secs() -> u64 {
    300
}

impl Default for ObpApiConfig {
    fn default() -> Self {
        ObpApiConfig {
            base_url: "http://localhost:8080".into(),
            token_url: String::new(),
            // No client credentials.
            client_id_env: String::new(),
            // No client secret.
            client_secret_env: String::new(),
            scope: None,
            // No DirectLogin token.
            direct_login_token_env: String::new(),
            routing_refresh_secs: default_routing_refresh_secs(),
        }
    }
}

/// Durable outbox location.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct OutboxConfig {
    path: PathBuf,
    /// Finished rows (EXCEPTION, or REPORTED and settled) older than this are
    /// deleted once an hour. 0 (the default) keeps everything: a row holds the
    /// commitment salt and the full payment request with the originator's
    /// details, the bank's own record of what it committed, so how long to keep
    /// it is the bank's decision (record-keeping rules usually run to years).
    #[serde(default = "default_outbox_retention_days")]
    retention_days: u32,
}

fn default_outbox_retention_days() -> u32 {
    0
}

impl Default for OutboxConfig {
    fn default() -> Self {
        OutboxConfig {
            path: PathBuf::from("./outbox/obp-bank-node.db"),
            retention_days: default_outbox_retention_days(),
        }
    }
}

/// Once an hour, drop outbox rows that are finished and past retention.
async fn run_outbox_retention(outbox: OutboxStore, retention_days: u32) {
    let keep = chrono::Duration::days(i64::from(retention_days));
    loop {
        let cutoff = (chrono::Utc::now() - keep).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match outbox.purge_finished_before(&cutoff).await {
            Ok(0) => {}
            Ok(n) => info!(rows = n, cutoff = %cutoff, "outbox retention: deleted finished rows"),
            Err(e) => warn!(error = %e, "outbox retention failed; will retry in an hour"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ServerConfig {
    bind: String,
}

/// The bank this Bank Node serves, as it is on one OBP host: each entry of `hosts:` has its own `bank:` block, because a bank has a different `bank_id` and corridor accounts on each OBP host. Defaults are placeholders so the binary boots without a config file.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct BankConfig {
    bank_id: String,
    /// Corridor account to debit when `corridor_accounts` is empty — the
    /// single-currency shorthand. Also what `/health` reports as `account_id`.
    account_id: String,
    /// Currency (ISO 4217) → corridor account id, one account per sending
    /// currency (like nostros). When non-empty this map is authoritative:
    /// a currency without an entry is rejected at initiation with
    /// `422 OBP-BANK-NODE-ROUTING-001` (there is no fallback to `account_id`,
    /// so a misdirected booking cannot happen silently).
    #[serde(default)]
    corridor_accounts: std::collections::HashMap<String, String>,
    /// The account view the node creates Transaction Requests through. With
    /// client credentials (the node as a Platform App) this is the view its
    /// Consumer has been given access to on each corridor account (Consumer
    /// Account Access in OBP-API): a view with only the permission to create
    /// Transaction Requests, never `owner`. With a DirectLogin service user it
    /// is a view that user holds, `owner` by default.
    #[serde(default = "default_view_id")]
    view_id: String,
}

fn default_view_id() -> String {
    "owner".into()
}

impl Default for BankConfig {
    fn default() -> Self {
        BankConfig {
            bank_id: "PLACEHOLDER-bank-id".into(),
            account_id: "PLACEHOLDER-account-id".into(),
            corridor_accounts: Default::default(),
            view_id: default_view_id(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct BlockchainConfig {
    #[serde(rename = "type")]
    kind: BlockchainKind,
    /// Optional overrides; the defaults match the bundled Cardano container
    /// and the keys generated at installation.
    #[serde(default)]
    cardano: CardanoConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum BlockchainKind {
    Mock,
    Cardano,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = load_config()?;
    // The settlement store is opened first: the chain follower resumes from
    // the cursor and payments it keeps (see FollowerStore).
    let settlement_store = SettlementStore::connect(&config.settlement.store_path)
        .await
        .with_context(|| {
            format!(
                "failed to open settlement store at {}",
                config.settlement.store_path.display()
            )
        })?;
    let (backend, settlement) = build_backend(&config, &settlement_store).await?;
    let blockchain_label: &'static str = match config.blockchain.kind {
        BlockchainKind::Mock => "mock",
        BlockchainKind::Cardano => "cardano",
    };

    info!(
        bind = %config.server.bind,
        // Each OBP host and the node's bank there.
        hosts = %config
            .hosts
            .iter()
            .map(|host_config| format!("{} (bank {})", obp_host(&host_config.obp_api), host_config.bank.bank_id))
            .collect::<Vec<_>>()
            .join(", "),
        blockchain = blockchain_label,
        outbox = %config.outbox.path.display(),
        version = env!("CARGO_PKG_VERSION"),
        "OBP Bank Node starting"
    );

    let outbox = OutboxStore::connect(&config.outbox.path)
        .await
        .with_context(|| format!("failed to open outbox at {}", config.outbox.path.display()))?;

    // The settlement and evidence stores are opened unconditionally — the
    // south-side read API serves them even when no settlement rail or
    // Interface C consumer is running (e.g. mock mode, consumer off).
    let evidence = EvidenceStore::connect(&config.evidence.path)
        .await
        .with_context(|| {
            format!(
                "failed to open evidence store at {}",
                config.evidence.path.display()
            )
        })?;

    // The finality watcher writes Settlement Reference records with the same notary.
    let backend_for_records = Arc::clone(&backend);

    // Settlement finality: whenever a settlement rail exists, start the
    // finality watcher — even with the Interface C consumer off, previously
    // SUBMITTED settlements must still be promoted to FINAL (or flagged) as
    // the chain advances. One watcher serves every OBP host: settlements are
    // paid from the one wallet, and each row says which bank paid it.
    if let Some(settlement_backend) = &settlement {
        // The shared watcher.
        let watcher = FinalityWatcher {
            store: settlement_store.clone(),
            backend: Arc::clone(settlement_backend),
            notary: Arc::clone(&backend_for_records),
            outbox: outbox.clone(),
            finality_depth: config.settlement.finality_depth,
            poll_interval: std::time::Duration::from_secs(config.settlement.finality_poll_secs),
        };
        // Run it in the background.
        tokio::spawn(watcher.run());
    }

    // CBS delivery is shared by every OBP host: credits all go to the bank's one core banking system.
    let cbs = CbsClient::new(
        config.cbs_delivery.webhook.url.clone(),
        config.local_secret.clone(),
        config.cbs_delivery.webhook.timeout_seconds,
    )
    .context("failed to build CBS client")?;

    // What each OBP host needs: its own OBP-API client, routing schemes, Interface C consumer and incoming-settlement watcher.
    let mut host_states: Vec<HostState> = Vec::new();
    // Every OBP host the node is on, for a hub's routers to carry a payment on to another of them. Filled in once all are set up.
    let forward_targets: interface_c::router::ForwardTargets = Arc::new(std::sync::OnceLock::new());
    // The entries, collected as each OBP host is set up.
    let mut forward_target_list: Vec<interface_c::router::ForwardTarget> = Vec::new();
    // The dispatcher's clients, keyed by OBP host.
    let mut obp_clients: std::collections::HashMap<String, Arc<ObpClient>> = std::collections::HashMap::new();
    // Set up each OBP host in turn.
    for host_config in &config.hosts {
        // The OBP host's name: the host (and port) of its OBP-API's base URL.
        let host_name = obp_host(&host_config.obp_api);
        // This OBP host's OBP-API client, logged in as the node's Consumer there.
        let obp = Arc::new(
            ObpClient::new(
                // This OBP-API's base URL.
                host_config.obp_api.base_url.clone(),
                // The node's login there, from the environment variables the config names.
                build_obp_auth(&host_config.obp_api, &host_name)?,
            )
            // Name the OBP host in the error.
            .with_context(|| format!("failed to build the OBP-API client for OBP host {host_name}"))?
            // The view the node creates Transaction Requests through there.
            .with_view_id(host_config.bank.view_id.clone()),
        );

        // As a Platform App (client credentials), tell this OBP-API which Scopes the node needs, so its administrators can see which are missing.
        if obp.uses_client_credentials() {
            // In the background: OBP-API may not be up yet.
            tokio::spawn(declare_platform_app(
                Arc::clone(&obp),
                host_config.bank.bank_id.clone(),
            ));
        }

        // Settlements this bank receives as creditor on this OBP host: checked against what actually arrives at its own address.
        if let Some(settlement_backend) = &settlement {
            // One watcher per OBP host, asking that OBP host's OBP-API.
            tokio::spawn(
                incoming::IncomingWatcher {
                    store: settlement_store.clone(),
                    obp: Arc::clone(&obp),
                    backend: Arc::clone(settlement_backend),
                    // The OBP host this watcher serves.
                    obp_host: host_name.clone(),
                    bank_id: host_config.bank.bank_id.clone(),
                    finality_depth: config.settlement.finality_depth,
                    poll_interval: std::time::Duration::from_secs(config.settlement.incoming_poll_secs),
                    paid_timeout: std::time::Duration::from_secs(config.settlement.incoming_timeout_secs),
                }
                .run(),
            );
        }

        // Interface C — inbound from this OBP host's OBP-API over RabbitMQ. Off by default; when on,
        // it consumes credit notifications (capturing the salt as evidence) and
        // delivers credits to the bank's CBS.
        let interface_c_status = interface_c::consumer::ConsumerStatus::new();
        if host_config.rabbitmq.enabled {
            // The value leg, when the node has a settlement rail.
            let settlement_service = settlement.as_ref().map(|settlement_backend| {
                // The rail and the store it records progress in.
                interface_c::router::SettlementService {
                    backend: Arc::clone(settlement_backend),
                    store: settlement_store.clone(),
                    finality_depth: config.settlement.finality_depth,
                }
            });
            // This OBP host's message router: what it writes is recorded as this OBP host's.
            let c_router = Arc::new(interface_c::Router::new(
                host_name.clone(),
                host_config.bank.bank_id.clone(),
                host_config.bank.account_id.clone(),
                evidence.clone(),
                cbs.clone(),
                outbox.clone(),
                settlement_service,
            )
            // The node's OBP hosts, so a credit can be carried on to another of them.
            .with_forward_targets(Arc::clone(&forward_targets)));
            let consumer_cfg = interface_c::consumer::ConsumerConfig {
                uri: build_amqp_uri(&host_config.rabbitmq),
                queue: host_config.rabbitmq.request_queue.clone(),
                consumer_tag: format!("obp-bank-node.{}", host_config.bank.bank_id),
            };
            info!(
                obp_host = %host_name,
                vhost = %host_config.rabbitmq.virtual_host,
                queue = %host_config.rabbitmq.request_queue,
                cbs = %config.cbs_delivery.webhook.url,
                "Interface C enabled — starting RabbitMQ consumer"
            );
            // Supervised: reconnects with backoff forever. A consumer that stayed
            // dead after one failure left the node deaf while HTTP looked healthy.
            tokio::spawn(interface_c::consumer::run_supervised(
                consumer_cfg,
                c_router,
                interface_c_status.clone(),
            ));
        } else {
            interface_c_status.set_disabled();
            info!(obp_host = %host_name, "rabbitmq.enabled=false — Interface C consumer not started for this OBP host");
        }

        // Routing-scheme registry: cached from this OBP host's OBP-API for synchronous A1.1
        // beneficiary-routing validation. Fail-open until first load.
        let routing = routing::RoutingRegistry::default();
        if host_config.obp_api.routing_refresh_secs > 0 {
            routing::spawn_refresher(
                routing.clone(),
                Arc::clone(&obp),
                host_config.obp_api.routing_refresh_secs,
            );
        } else {
            info!(obp_host = %host_name, "obp_api.routing_refresh_secs=0 — routing-scheme validation disabled for this OBP host");
        }

        // The dispatcher sends this OBP host's rows with this client.
        obp_clients.insert(host_name.clone(), Arc::clone(&obp));
        // What the REST API needs about this OBP host.
        // This OBP host as a place a hub can carry a payment on to.
        forward_target_list.push(interface_c::router::ForwardTarget {
            // The OBP host.
            obp_host: host_name.clone(),
            // The node's bank there.
            bank_id: host_config.bank.bank_id.clone(),
            // Its shorthand corridor account.
            account_id: host_config.bank.account_id.clone(),
            // Its corridor accounts, keyed by upper-case currency.
            corridor_accounts: host_config
                .bank
                .corridor_accounts
                .iter()
                .map(|(currency, account)| (currency.to_uppercase(), account.clone()))
                .collect(),
            // Its OBP-API client.
            obp: Arc::clone(&obp),
        });
        host_states.push(HostState {
            host: host_name,
            obp_api_url: host_config.obp_api.base_url.clone(),
            obp,
            routing,
            bank_id: host_config.bank.bank_id.clone(),
            account_id: host_config.bank.account_id.clone(),
            // Keys uppercased once here so lookup is case-insensitive on the
            // currency without re-normalizing per request.
            corridor_accounts: host_config
                .bank
                .corridor_accounts
                .iter()
                .map(|(currency, account)| (currency.to_uppercase(), account.clone()))
                .collect(),
            interface_c: interface_c_status,
        });
    }

    // Every OBP host is set up: give the routers the full list. Set only here, so this cannot fail.
    let _ = forward_targets.set(forward_target_list);

    // The dispatcher owns the backend and drains the outbox asynchronously, sending each row to its own OBP host.
    let dispatcher = Dispatcher::new(
        outbox.clone(),
        obp_clients,
        backend,
        blockchain_label,
        DispatcherConfig::default(),
    );
    tokio::spawn(dispatcher.run());
    if config.outbox.retention_days > 0 {
        tokio::spawn(run_outbox_retention(outbox.clone(), config.outbox.retention_days));
    }

    let state = BankNodeState {
        // The node's name, from its config.
        name: config.name.clone(),
        outbox,
        settlements: settlement_store,
        evidence,
        blockchain_label,
        // Every OBP host the node is on.
        hosts: Arc::new(host_states),
        finality_depth: config.settlement.finality_depth,
        chain_health_url: match config.blockchain.kind {
            BlockchainKind::Mock => None,
            BlockchainKind::Cardano => Some(ogmios_health_url(&config.blockchain.cardano.ogmios_url)),
        },
        instance_id: uuid::Uuid::new_v4().to_string(),
        started_at: std::time::Instant::now(),
    };
    let app = build_router(state);

    let addr: SocketAddr = config
        .server
        .bind
        .parse()
        .with_context(|| format!("invalid server.bind: {}", config.server.bind))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    info!(%addr, "listening");
    axum::serve(listener, app)
        .await
        .context("axum::serve failed")?;
    Ok(())
}

fn load_config() -> anyhow::Result<Config> {
    let path = std::env::var("OBP_BANK_NODE_CONFIG")
        .unwrap_or_else(|_| "./obp-bank-node-config.yaml".to_string());

    let mut fig = Figment::from(Serialized::defaults(Config::default()));
    let mut yaml_cardano_network: Option<String> = None;
    if std::path::Path::new(&path).exists() {
        info!(config_path = %path, "loading YAML overrides");
        yaml_cardano_network = Figment::from(Yaml::file(&path))
            .extract_inner::<String>("blockchain.cardano.network")
            .ok();
        fig = fig.merge(Yaml::file(&path));
    } else {
        warn!(config_path = %path, "config file not found, using defaults");
    }
    fig = fig.merge(Env::prefixed("OBP_BN_").split("__"));

    // Per-network images bake OBP_BN_BLOCKCHAIN__CARDANO__NETWORK, which would
    // silently override a conflicting YAML value. Fail loudly instead: the
    // operator wrote a setting this build cannot honor.
    if let (Ok(env_network), Some(yaml_network)) = (
        std::env::var("OBP_BN_BLOCKCHAIN__CARDANO__NETWORK"),
        yaml_cardano_network,
    ) {
        if env_network != yaml_network {
            anyhow::bail!(
                "this build is fixed to Cardano network '{env_network}' \
                 (OBP_BN_BLOCKCHAIN__CARDANO__NETWORK), but {path} sets \
                 `blockchain.cardano.network: \"{yaml_network}\"` — remove the \
                 YAML setting or use the '{yaml_network}' image"
            );
        }
    }

    let config: Config = fig.extract().context("failed to parse config")?;
    validate_config(&config)?;
    Ok(config)
}

/// The A2 delivery modes this build can run. The spec also names
/// `webhook_iso20022`, `database` and `file`; a config asking for one of
/// those must not quietly behave as `webhook_obp`.
const CBS_DELIVERY_MODES: &[&str] = &["webhook_obp"];

/// Refuse a config the node would otherwise misread at runtime.
fn validate_config(cfg: &Config) -> anyhow::Result<()> {
    // The node must be on at least one OBP host.
    if cfg.hosts.is_empty() {
        // Nothing to connect to.
        anyhow::bail!("hosts is empty: list at least one OBP host the node is on");
    }
    // The OBP hosts seen so far, to find one listed twice.
    let mut seen_hosts = std::collections::HashSet::new();
    // Each OBP host in turn.
    for host_config in &cfg.hosts {
        // The OBP host's name.
        let host_name = obp_host(&host_config.obp_api);
        // The URLs and the stores tell OBP hosts apart by name, so each may appear only once.
        if !seen_hosts.insert(host_name.clone()) {
            // Name the OBP host that is listed twice.
            anyhow::bail!("OBP host {host_name} is listed more than once under hosts: each OBP host may appear only once");
        }
        // Client credentials need both a client id and a client secret.
        if !host_config.obp_api.client_id_env.trim().is_empty() && host_config.obp_api.client_secret_env.trim().is_empty() {
            // Say which setting is missing.
            anyhow::bail!("OBP host {host_name}: obp_api.client_id_env is set but obp_api.client_secret_env is not");
        }
    }
    let mode = cfg.cbs_delivery.mode.trim();
    if !mode.is_empty() && !CBS_DELIVERY_MODES.contains(&mode) {
        anyhow::bail!(
            "cbs_delivery.mode \"{mode}\" is not implemented in this build; supported: {} \
             (planned: webhook_iso20022, database, file)",
            CBS_DELIVERY_MODES.join(", ")
        );
    }
    // Interface C on at any OBP host means credits arrive, and they must go somewhere.
    if cfg.hosts.iter().any(|host_config| host_config.rabbitmq.enabled) && cfg.cbs_delivery.webhook.url.trim().is_empty() {
        anyhow::bail!(
            "cbs_delivery.webhook.url is empty, but rabbitmq.enabled is true for an OBP host: credit \
             notifications would have nowhere to go"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_retention_defaults_to_keeping_everything_and_can_be_set() {
        assert_eq!(Config::default().outbox.retention_days, 0);
        let cfg: OutboxConfig = serde_json::from_str(r#"{"path":"x.db"}"#).unwrap();
        assert_eq!(cfg.retention_days, 0);
        let cfg: OutboxConfig = serde_json::from_str(r#"{"path":"x.db","retention_days":90}"#).unwrap();
        assert_eq!(cfg.retention_days, 90);
    }

    // The OBP host is the host of OBP-API's base URL, with the port only when the URL names one.
    #[test]
    fn obp_host_is_the_host_and_port_of_the_base_url() {
        // A URL with a port keeps it.
        let mut obp_api = ObpApiConfig { base_url: "http://localhost:8081".into(), ..ObpApiConfig::default() };
        // The host and port.
        assert_eq!(obp_host(&obp_api), "localhost:8081");
        // A URL with the scheme's default port, and a path.
        obp_api.base_url = "https://apisandbox.openbankproject.com/".into();
        // The host alone.
        assert_eq!(obp_host(&obp_api), "apisandbox.openbankproject.com");
        // A base URL that does not parse.
        obp_api.base_url = "not a url".into();
        // Is reported as written.
        assert_eq!(obp_host(&obp_api), "not a url");
    }

    // An empty hosts list is refused.
    #[test]
    fn no_hosts_is_rejected_at_startup() {
        // The default config, with its one OBP host taken away.
        let mut cfg = Config::default();
        // No OBP hosts.
        cfg.hosts.clear();
        // Refused, naming the setting.
        let err = validate_config(&cfg).unwrap_err().to_string();
        // The message names hosts.
        assert!(err.contains("hosts is empty"), "{err}");
    }

    // The same OBP host twice is refused: URLs and stores tell OBP hosts apart by name.
    #[test]
    fn the_same_obp_host_twice_is_rejected_at_startup() {
        // The default config has one OBP host on localhost:8080.
        let mut cfg = Config::default();
        // A second entry on the same OBP-API, written slightly differently.
        let mut second = HostConfig::default();
        // Same host and port, with a trailing slash.
        second.obp_api.base_url = "http://localhost:8080/".into();
        // Add it.
        cfg.hosts.push(second);
        // Refused.
        let err = validate_config(&cfg).unwrap_err().to_string();
        // The message names the OBP host.
        assert!(err.contains("localhost:8080"), "{err}");
    }

    // Two different OBP hosts are fine.
    #[test]
    fn two_different_obp_hosts_validate() {
        // The default config has one OBP host on localhost:8080.
        let mut cfg = Config::default();
        // A second OBP host on another port.
        let mut second = HostConfig::default();
        // Another OBP-API.
        second.obp_api.base_url = "http://localhost:8081".into();
        // Add it.
        cfg.hosts.push(second);
        // Accepted.
        validate_config(&cfg).unwrap();
    }

    // A client id without a client secret is refused.
    #[test]
    fn client_id_without_client_secret_is_rejected_at_startup() {
        // The default config.
        let mut cfg = Config::default();
        // A client id variable, but no secret variable.
        cfg.hosts[0].obp_api.client_id_env = "SOME_CLIENT_ID".into();
        // Refused.
        let err = validate_config(&cfg).unwrap_err().to_string();
        // The message names the missing setting.
        assert!(err.contains("client_secret_env"), "{err}");
    }

    // A credential setting naming a variable that is not set stops the node, without showing any value.
    #[test]
    fn a_missing_credential_variable_stops_the_node() {
        // An OBP-API config naming variables nobody sets.
        let obp_api = ObpApiConfig {
            // A client id variable that is not set.
            client_id_env: "OBP_BN_TEST_UNSET_CLIENT_ID_7F3A".into(),
            // And a secret variable that is not set either.
            client_secret_env: "OBP_BN_TEST_UNSET_CLIENT_SECRET_7F3A".into(),
            // Everything else as default.
            ..ObpApiConfig::default()
        };
        // Building the login fails.
        // (ObpAuth holds secrets, so it is not printable; take the error side only.)
        let err = build_obp_auth(&obp_api, "localhost:8081").err().expect("a missing variable must stop the node").to_string();
        // The message names the OBP host and the variable.
        assert!(err.contains("localhost:8081") && err.contains("OBP_BN_TEST_UNSET_CLIENT_ID_7F3A"), "{err}");
    }

    // Credentials are read from the variables the config names.
    #[test]
    fn credentials_come_from_the_named_variables() {
        // Set a client id variable for this test only (the name is unique to it).
        std::env::set_var("OBP_BN_TEST_CLIENT_ID_9C21", "the-client-id");
        // And its secret.
        std::env::set_var("OBP_BN_TEST_CLIENT_SECRET_9C21", "the-client-secret");
        // An OBP-API config naming them.
        let obp_api = ObpApiConfig {
            // The token endpoint.
            token_url: "http://localhost:9001/obp-oidc/token".into(),
            // The client id variable.
            client_id_env: "OBP_BN_TEST_CLIENT_ID_9C21".into(),
            // The client secret variable.
            client_secret_env: "OBP_BN_TEST_CLIENT_SECRET_9C21".into(),
            // Everything else as default.
            ..ObpApiConfig::default()
        };
        // Build the login.
        let auth = build_obp_auth(&obp_api, "localhost:8081").unwrap();
        // Client credentials, with the values from the environment.
        match auth {
            // The expected kind of login.
            ObpAuth::ClientCredentials { client_id, client_secret, token_url, .. } => {
                // The id came from its variable.
                assert_eq!(client_id, "the-client-id");
                // The secret came from its variable.
                assert_eq!(client_secret, "the-client-secret");
                // The token URL came from the config.
                assert_eq!(token_url, "http://localhost:9001/obp-oidc/token");
            }
            // Any other kind is wrong.
            _ => panic!("expected client credentials"),
        }
    }

    #[test]
    fn default_config_validates() {
        validate_config(&Config::default()).unwrap();
    }

    #[test]
    fn unsupported_cbs_delivery_mode_is_rejected_at_startup() {
        let mut cfg = Config::default();
        cfg.cbs_delivery.mode = "database".into();
        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("cbs_delivery.mode"), "{err}");
        assert!(err.contains("\"database\""), "{err}");
        assert!(err.contains("webhook_obp"), "{err}");
    }

    #[test]
    fn empty_cbs_delivery_mode_means_the_default() {
        let mut cfg = Config::default();
        cfg.cbs_delivery.mode = String::new();
        validate_config(&cfg).unwrap();
    }

    #[test]
    fn empty_webhook_url_is_rejected_when_interface_c_is_on() {
        let mut cfg = Config::default();
        // Interface C on at the one OBP host.
        cfg.hosts[0].rabbitmq.enabled = true;
        cfg.cbs_delivery.webhook.url = " ".into();
        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("cbs_delivery.webhook.url"), "{err}");
    }
}

/// Build the AMQP URI for the bank's vhost. The vhost path is percent-encoded
/// (`/` → `%2f`), so `/bank.ke.01.kcs` becomes `%2fbank.ke.01.kcs`.
///
/// Note: username/password are not percent-encoded here — fine for the PoC's
/// simple credentials; revisit if creds contain reserved characters.
fn build_amqp_uri(cfg: &RabbitMqConfig) -> String {
    let vhost = cfg.virtual_host.replace('/', "%2f");
    format!(
        "amqp://{}:{}@{}:{}/{}",
        cfg.username, cfg.password, cfg.host, cfg.port, vhost
    )
}

/// Declare the node's Scopes as a Platform App, retrying while OBP-API can't
/// be reached. A refusal (e.g. the Consumer is not yet marked as a Platform
/// App) is logged once with what to do; the node runs on regardless, and its
/// calls fail with OBP-API's missing-Scope error until the Scopes are granted.
async fn declare_platform_app(obp: Arc<ObpClient>, bank_id: String) {
    use crate::obp_client::RequiredScope;
    let scopes = [
        RequiredScope {
            role_name: "CanAttachOpenCorridorPromise",
            bank_id: &bank_id,
            needed_for: "Reporting each payment's on-chain promise (transaction hash, commitment and salt) back to OBP-API, so the receiving bank can verify it.",
            optional: false,
        },
        RequiredScope {
            role_name: "CanSettleOpenCorridor",
            bank_id: &bank_id,
            needed_for: "Settling the net position with another bank when an operator asks the node to (POST /obp-bank-node/v5.1.0/settlements).",
            optional: false,
        },
    ];
    let mut delay = std::time::Duration::from_secs(5);
    for attempt in 1..=12 {
        match obp
            .declare_platform_app(env!("CARGO_PKG_VERSION"), &scopes)
            .await
        {
            Ok(()) => {
                info!(%bank_id, "declared the node's Scopes to OBP-API as a Platform App");
                return;
            }
            Err(e) if e.is_retryable() => {
                warn!(attempt, error = %e, "could not declare the node's Scopes to OBP-API yet; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(300));
            }
            Err(e) => {
                warn!(error = %e, "OBP-API refused the node's Platform App declaration. An administrator marks the node's Consumer as a Platform App (POST /obp/v7.0.0/management/platform-apps) and grants it the Scopes CanAttachOpenCorridorPromise and CanSettleOpenCorridor at this bank");
                return;
            }
        }
    }
    warn!("gave up declaring the node's Scopes to OBP-API; it is retried on the next start");
}

/// The value of the environment variable a config setting names. `Ok(None)` when the setting is empty (not configured). A named variable that is missing or empty stops the node, saying which setting named it.
fn value_from_env(host_name: &str, setting: &str, variable: &str) -> anyhow::Result<Option<String>> {
    // An empty setting means the node does not use this credential.
    if variable.trim().is_empty() {
        // Nothing to read.
        return Ok(None);
    }
    // Read the variable the setting names.
    match std::env::var(variable.trim()) {
        // Set and not empty: use it.
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        // Missing or empty: the node cannot log in as configured. The value itself is never shown.
        _ => anyhow::bail!(
            "OBP host {host_name}: obp_api.{setting} names the environment variable {}, which is not set or is empty",
            variable.trim()
        ),
    }
}

/// Choose the OBP-API auth scheme for one OBP host from its config. Prefer OAuth2 client credentials when `client_id_env` is set, fall back to a DirectLogin token (`direct_login_token_env`), and otherwise run unauthenticated (local or mock OBP-API).
fn build_obp_auth(cfg: &ObpApiConfig, host_name: &str) -> anyhow::Result<ObpAuth> {
    // Client credentials, when configured.
    if let Some(client_id) = value_from_env(host_name, "client_id_env", &cfg.client_id_env)? {
        // The secret is required with the id (validate_config checks the setting; this checks the variable).
        let client_secret = value_from_env(host_name, "client_secret_env", &cfg.client_secret_env)?
            // A missing secret setting is caught by validate_config; say so plainly here too.
            .ok_or_else(|| anyhow::anyhow!("OBP host {host_name}: obp_api.client_secret_env is not set"))?;
        // Log in with OAuth2 client credentials.
        return Ok(ObpAuth::ClientCredentials {
            token_url: cfg.token_url.clone(),
            client_id,
            client_secret,
            scope: cfg.scope.clone(),
        });
    }
    // A DirectLogin token, when configured.
    if let Some(token) = value_from_env(host_name, "direct_login_token_env", &cfg.direct_login_token_env)? {
        // Log in with DirectLogin.
        return Ok(ObpAuth::DirectLogin { token });
    }
    // No credentials: fine only against a local or mock OBP-API.
    warn!(obp_host = %host_name, "no OBP-API credentials configured for this OBP host — its OBP client will send unauthenticated requests");
    // Unauthenticated.
    Ok(ObpAuth::None)
}

/// Build the notary backend and, when on Cardano, the matching ADA settlement
/// backend (sharing the same wallet + Ogmios + submission lock). Mock mode has
/// no settlement rail, so its settlement backend is `None`.
type Backends = (
    Arc<dyn BlockchainBackend>,
    Option<Arc<dyn SettlementBackend>>,
);

/// Ogmios serves `/health` over HTTP on the port of its WebSocket.
fn ogmios_health_url(ogmios_url: &str) -> String {
    let base = ogmios_url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    format!("{}/health", base.trim_end_matches('/'))
}

async fn build_backend(config: &Config, follower_store: &SettlementStore) -> anyhow::Result<Backends> {
    match config.blockchain.kind {
        BlockchainKind::Mock => Ok((Arc::new(MockBackend::new()), None)),
        BlockchainKind::Cardano => {
            let c = obp_blockchain::cardano::CardanoBackend::with_store(
                config.blockchain.cardano.clone(),
                Some(Arc::new(follower_store.clone())),
            )
            .await
            .context("failed to construct CardanoBackend")?;
            // Settlement shares the backend's wallet/Ogmios/submit-lock.
            let fx: Arc<dyn FxSource> = match config.settlement.fx_source {
                FxSourceKind::Coingecko => {
                    info!(base_url = %config.settlement.fx_base_url, "FX source: CoinGecko");
                    Arc::new(
                        crate::fx::CoinGeckoFxSource::new(
                            config.settlement.fx_base_url.clone(),
                            config.settlement.fx_timeout_secs,
                        )
                        .context("failed to build CoinGecko FX source")?,
                    )
                }
                FxSourceKind::Cross => {
                    let crypto_legs = config.settlement.fx_crypto_leg.to_vec();
                    let fiat_legs = config.settlement.fx_fiat_leg.to_vec();
                    let crypto = crypto_legs
                        .iter()
                        .map(|leg| {
                            Ok(match leg {
                                FxCryptoLeg::Coingecko => crate::fx::CryptoUsdLeg::CoinGecko {
                                    base_url: config.settlement.fx_base_url.clone(),
                                },
                                FxCryptoLeg::Api3 => {
                                    anyhow::ensure!(
                                        !config.settlement.fx_api3_rpc_url.trim().is_empty()
                                            && !config
                                                .settlement
                                                .fx_api3_proxy_address
                                                .trim()
                                                .is_empty(),
                                        "fx_crypto_leg api3 needs fx_api3_rpc_url and fx_api3_proxy_address"
                                    );
                                    crate::fx::CryptoUsdLeg::Api3 {
                                        rpc_url: config.settlement.fx_api3_rpc_url.clone(),
                                        proxy_address: config
                                            .settlement
                                            .fx_api3_proxy_address
                                            .clone(),
                                    }
                                }
                                FxCryptoLeg::Pyth => crate::fx::CryptoUsdLeg::Pyth {
                                    hermes_url: config.settlement.fx_hermes_base_url.clone(),
                                },
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    let fiat = fiat_legs
                        .iter()
                        .map(|leg| match leg {
                            FxFiatLeg::ErApi => crate::fx::FiatUsdLeg::ErApi {
                                base_url: config.settlement.fx_fiat_base_url.clone(),
                            },
                            FxFiatLeg::Pyth => crate::fx::FiatUsdLeg::Pyth {
                                hermes_url: config.settlement.fx_hermes_base_url.clone(),
                            },
                        })
                        .collect::<Vec<_>>();
                    info!(
                        crypto_legs = ?crypto_legs,
                        fiat_legs = ?fiat_legs,
                        "FX source: cross rate (asset/USD × USD/fiat), sources tried in order"
                    );
                    Arc::new(
                        crate::fx::CrossRateFxSource::new(
                            crypto,
                            fiat,
                            config.settlement.fx_timeout_secs,
                            config.settlement.fx_pyth_max_stale_secs,
                        )
                        .context("failed to build cross-rate FX source")?,
                    )
                }
                FxSourceKind::Stub => {
                    warn!(
                        rate = config.settlement.ada_rate_minor_per_whole_ada,
                        "FX source: STUB fixed rate — tests/offline development only"
                    );
                    Arc::new(StubFxSource::new(
                        config.settlement.ada_rate_minor_per_whole_ada.into(),
                    ))
                }
            };
            let settlement: Arc<dyn SettlementBackend> =
                Arc::new(CardanoAdaSettlement::from_backend(&c, fx));
            let backend: Arc<dyn BlockchainBackend> = Arc::new(c);
            Ok((backend, Some(settlement)))
        }
    }
}

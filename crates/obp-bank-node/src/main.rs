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
mod interface_c;
mod obp_client;
mod outbox;
mod rest;
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
use crate::rest::{build_router, BankNodeState};
use crate::settlement_store::SettlementStore;

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    server: ServerConfig,
    bank: BankConfig,
    blockchain: BlockchainConfig,
    obp_api: ObpApiConfig,
    outbox: OutboxConfig,
    rabbitmq: RabbitMqConfig,
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
            server: ServerConfig {
                bind: "0.0.0.0:8088".into(),
            },
            bank: BankConfig::default(),
            blockchain: BlockchainConfig {
                kind: BlockchainKind::Mock,
                cardano: CardanoConfig::default(),
            },
            obp_api: ObpApiConfig::default(),
            outbox: OutboxConfig::default(),
            rabbitmq: RabbitMqConfig::default(),
            cbs_delivery: CbsDeliveryConfig::default(),
            evidence: EvidenceConfig::default(),
            settlement: SettlementConfig::default(),
            local_secret: None,
        }
    }
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
fn default_settlement_store_path() -> PathBuf {
    PathBuf::from("./outbox/settlements.db")
}
fn default_fx_base_url() -> String {
    "https://api.coingecko.com".to_string()
}
fn default_fx_timeout_secs() -> u64 {
    10
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum FxSourceKind {
    #[default]
    Coingecko,
    Stub,
}

impl Default for SettlementConfig {
    fn default() -> Self {
        SettlementConfig {
            fx_source: FxSourceKind::default(),
            fx_base_url: default_fx_base_url(),
            fx_timeout_secs: default_fx_timeout_secs(),
            ada_rate_minor_per_whole_ada: 3542,
            finality_depth: default_finality_depth(),
            finality_poll_secs: default_finality_poll_secs(),
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

/// Interface B — how to reach OBP-API. The Bank Node authenticates as a
/// configured service: OAuth2 client-credentials (set `token_url` + `client_id`
/// + `client_secret`) or a pre-obtained DirectLogin token. With all of them
/// empty the client runs unauthenticated, which is fine against a local/mock
/// OBP-API.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct ObpApiConfig {
    base_url: String,
    /// OAuth2 token endpoint for the client-credentials grant.
    #[serde(default)]
    token_url: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    scope: Option<String>,
    /// Alternative to client-credentials: a pre-obtained DirectLogin token.
    #[serde(default)]
    direct_login_token: String,
}

impl Default for ObpApiConfig {
    fn default() -> Self {
        ObpApiConfig {
            base_url: "http://localhost:8080".into(),
            token_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            scope: None,
            direct_login_token: String::new(),
        }
    }
}

/// Durable outbox location.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct OutboxConfig {
    path: PathBuf,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        OutboxConfig {
            path: PathBuf::from("./outbox/obp-bank-node.db"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ServerConfig {
    bind: String,
}

/// The single bank this Bank Node serves. Sourced from the `bank:` block in
/// `obp-bank-node-config.yaml`; defaults are placeholders so the binary boots
/// without a config file.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct BankConfig {
    bank_id: String,
    /// Settlement account to debit. Future: support a map keyed by currency
    /// for multi-currency settlement.
    account_id: String,
}

impl Default for BankConfig {
    fn default() -> Self {
        BankConfig {
            bank_id: "PLACEHOLDER-bank-id".into(),
            account_id: "PLACEHOLDER-account-id".into(),
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
    let (backend, settlement) = build_backend(&config).await?;
    let blockchain_label: &'static str = match config.blockchain.kind {
        BlockchainKind::Mock => "mock",
        BlockchainKind::Cardano => "cardano",
    };

    info!(
        bind = %config.server.bind,
        bank_id = %config.bank.bank_id,
        account_id = %config.bank.account_id,
        blockchain = blockchain_label,
        outbox = %config.outbox.path.display(),
        obp_api = %config.obp_api.base_url,
        version = env!("CARGO_PKG_VERSION"),
        "OBP Bank Node starting"
    );

    let outbox = OutboxStore::connect(&config.outbox.path)
        .await
        .with_context(|| format!("failed to open outbox at {}", config.outbox.path.display()))?;

    let obp = Arc::new(
        ObpClient::new(config.obp_api.base_url.clone(), build_obp_auth(&config.obp_api))
            .context("failed to build OBP API client")?,
    );

    // The dispatcher owns the backend and drains the outbox asynchronously.
    let dispatcher = Dispatcher::new(
        outbox.clone(),
        Arc::clone(&obp),
        backend,
        blockchain_label,
        DispatcherConfig::default(),
    );
    tokio::spawn(dispatcher.run());

    // The settlement and evidence stores are opened unconditionally — the
    // south-side read API serves them even when no settlement rail or
    // Interface C consumer is running (e.g. mock mode, consumer off).
    let settlement_store =
        SettlementStore::connect(&config.settlement.store_path).await.with_context(|| {
            format!(
                "failed to open settlement store at {}",
                config.settlement.store_path.display()
            )
        })?;
    let evidence = EvidenceStore::connect(&config.evidence.path).await.with_context(|| {
        format!("failed to open evidence store at {}", config.evidence.path.display())
    })?;

    // Settlement finality: whenever a settlement rail exists, start the
    // finality watcher — even with the Interface C consumer off, previously
    // SUBMITTED settlements must still be promoted to FINAL (or flagged) as
    // the chain advances.
    let settlement_parts = match &settlement {
        Some(backend) => {
            let watcher = FinalityWatcher {
                store: settlement_store.clone(),
                backend: Arc::clone(backend),
                finality_depth: config.settlement.finality_depth,
                poll_interval: std::time::Duration::from_secs(config.settlement.finality_poll_secs),
            };
            tokio::spawn(watcher.run());
            Some((Arc::clone(backend), settlement_store.clone()))
        }
        None => None,
    };

    // Interface C — inbound from OBP-API over RabbitMQ. Off by default; when on,
    // it consumes credit notifications (capturing the salt as evidence) and
    // delivers credits to the bank's CBS.
    if config.rabbitmq.enabled {
        let cbs = CbsClient::new(
            config.cbs_delivery.webhook.url.clone(),
            config.local_secret.clone(),
            config.cbs_delivery.webhook.timeout_seconds,
        )
        .context("failed to build CBS client")?;
        let settlement_service =
            settlement_parts.map(|(backend, store)| interface_c::router::SettlementService {
                backend,
                store,
                finality_depth: config.settlement.finality_depth,
            });
        let c_router = Arc::new(interface_c::Router::new(
            config.bank.bank_id.clone(),
            evidence.clone(),
            cbs,
            settlement_service,
        ));
        let consumer_cfg = interface_c::consumer::ConsumerConfig {
            uri: build_amqp_uri(&config.rabbitmq),
            queue: config.rabbitmq.request_queue.clone(),
            consumer_tag: format!("obp-bank-node.{}", config.bank.bank_id),
        };
        info!(
            vhost = %config.rabbitmq.virtual_host,
            queue = %config.rabbitmq.request_queue,
            cbs = %config.cbs_delivery.webhook.url,
            "Interface C enabled — starting RabbitMQ consumer"
        );
        tokio::spawn(async move {
            if let Err(e) = interface_c::consumer::run(consumer_cfg, c_router).await {
                tracing::error!(error = %e, "Interface C consumer exited with error");
            }
        });
    } else {
        info!("rabbitmq.enabled=false — Interface C consumer not started");
    }

    let state = BankNodeState {
        outbox,
        settlements: settlement_store,
        evidence,
        obp,
        blockchain_label,
        bank_id: config.bank.bank_id.clone(),
        account_id: config.bank.account_id.clone(),
        finality_depth: config.settlement.finality_depth,
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
    axum::serve(listener, app).await.context("axum::serve failed")?;
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

    fig.extract().context("failed to parse config")
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

/// Choose the OBP-API auth scheme from config. Prefer OAuth2 client-credentials
/// when a `client_id` is set, fall back to a DirectLogin token, and otherwise
/// run unauthenticated (local/mock OBP-API).
fn build_obp_auth(cfg: &ObpApiConfig) -> ObpAuth {
    if !cfg.client_id.is_empty() {
        ObpAuth::ClientCredentials {
            token_url: cfg.token_url.clone(),
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
            scope: cfg.scope.clone(),
        }
    } else if !cfg.direct_login_token.is_empty() {
        ObpAuth::DirectLogin {
            token: cfg.direct_login_token.clone(),
        }
    } else {
        warn!("no OBP-API credentials configured — OBP client will send unauthenticated requests");
        ObpAuth::None
    }
}

/// Build the notary backend and, when on Cardano, the matching ADA settlement
/// backend (sharing the same wallet + Ogmios + submission lock). Mock mode has
/// no settlement rail, so its settlement backend is `None`.
type Backends = (Arc<dyn BlockchainBackend>, Option<Arc<dyn SettlementBackend>>);

async fn build_backend(config: &Config) -> anyhow::Result<Backends> {
    match config.blockchain.kind {
        BlockchainKind::Mock => Ok((Arc::new(MockBackend::new()), None)),
        BlockchainKind::Cardano => {
            let c = obp_blockchain::cardano::CardanoBackend::new(config.blockchain.cardano.clone())
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
                FxSourceKind::Stub => {
                    warn!(
                        rate = config.settlement.ada_rate_minor_per_whole_ada,
                        "FX source: STUB fixed rate — tests/offline development only"
                    );
                    Arc::new(StubFxSource::new(config.settlement.ada_rate_minor_per_whole_ada.into()))
                }
            };
            let settlement: Arc<dyn SettlementBackend> =
                Arc::new(CardanoAdaSettlement::from_backend(&c, fx));
            let backend: Arc<dyn BlockchainBackend> = Arc::new(c);
            Ok((backend, Some(settlement)))
        }
    }
}

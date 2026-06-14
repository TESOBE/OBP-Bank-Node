//! OBP Bank Node — main binary.
//!
//! Wires together config, the blockchain backend, and the south-side REST
//! router. AMQP consumer, outbox, delivery modes, and OBP API client will
//! land in dedicated modules over subsequent commits.

mod dispatcher;
mod obp_client;
mod outbox;
mod rest;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use obp_blockchain::{cardano::CardanoConfig, mock::MockBackend, BlockchainBackend};

use crate::dispatcher::{Dispatcher, DispatcherConfig};
use crate::obp_client::{ObpAuth, ObpClient};
use crate::outbox::OutboxStore;
use crate::rest::{build_router, BankNodeState};

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    server: ServerConfig,
    bank: BankConfig,
    blockchain: BlockchainConfig,
    obp_api: ObpApiConfig,
    outbox: OutboxConfig,
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
                cardano: None,
            },
            obp_api: ObpApiConfig::default(),
            outbox: OutboxConfig::default(),
        }
    }
}

/// Interface B — how to reach OBP-API. The `oauth2_*` fields carry the OBP
/// OAuth1.0a 4-tuple from registration. Request signing for that scheme is not
/// wired yet (see [`ObpAuth::OAuth1`]); with the fields empty the client runs
/// unauthenticated, which is fine against a local/mock OBP-API.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct ObpApiConfig {
    base_url: String,
    #[serde(default)]
    oauth2_consumer_key: String,
    #[serde(default)]
    oauth2_consumer_secret: String,
    #[serde(default)]
    oauth2_access_token: String,
    #[serde(default)]
    oauth2_token_secret: String,
}

impl Default for ObpApiConfig {
    fn default() -> Self {
        ObpApiConfig {
            base_url: "http://localhost:8080".into(),
            oauth2_consumer_key: String::new(),
            oauth2_consumer_secret: String::new(),
            oauth2_access_token: String::new(),
            oauth2_token_secret: String::new(),
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
    #[serde(default)]
    cardano: Option<CardanoConfig>,
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
    let backend = build_backend(&config.blockchain).await?;
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
        obp,
        backend,
        blockchain_label,
        DispatcherConfig::default(),
    );
    tokio::spawn(dispatcher.run());

    let state = BankNodeState {
        outbox,
        blockchain_label,
        bank_id: config.bank.bank_id.clone(),
        account_id: config.bank.account_id.clone(),
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
    if std::path::Path::new(&path).exists() {
        info!(config_path = %path, "loading YAML overrides");
        fig = fig.merge(Yaml::file(&path));
    } else {
        warn!(config_path = %path, "config file not found, using defaults");
    }
    fig = fig.merge(Env::prefixed("OBP_BN_").split("__"));
    fig.extract().context("failed to parse config")
}

/// Choose the OBP-API auth scheme from config. With no `oauth2_consumer_key`
/// the client runs unauthenticated (local/mock OBP-API). OAuth1.0a signing is
/// still a stub — see [`ObpAuth::OAuth1`].
fn build_obp_auth(cfg: &ObpApiConfig) -> ObpAuth {
    if cfg.oauth2_consumer_key.is_empty() {
        warn!("no OBP-API credentials configured — OBP client will send unauthenticated requests");
        ObpAuth::None
    } else {
        ObpAuth::OAuth1 {
            consumer_key: cfg.oauth2_consumer_key.clone(),
            consumer_secret: cfg.oauth2_consumer_secret.clone(),
            access_token: cfg.oauth2_access_token.clone(),
            token_secret: cfg.oauth2_token_secret.clone(),
        }
    }
}

async fn build_backend(cfg: &BlockchainConfig) -> anyhow::Result<Arc<dyn BlockchainBackend>> {
    match cfg.kind {
        BlockchainKind::Mock => Ok(Arc::new(MockBackend::new())),
        BlockchainKind::Cardano => {
            let cardano_cfg = cfg
                .cardano
                .clone()
                .context("blockchain.type=cardano requires a blockchain.cardano block")?;
            let c = obp_blockchain::cardano::CardanoBackend::new(cardano_cfg)
                .await
                .context("failed to construct CardanoBackend")?;
            Ok(Arc::new(c))
        }
    }
}

//! OBP Bank Node — main binary.
//!
//! Wires together config, the blockchain connector, and the south-side REST
//! router. AMQP consumer, outbox, delivery modes, and OBP API client will
//! land in dedicated modules over subsequent commits.

mod rest;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use obp_blockchain::{cardano::CardanoConfig, mock::MockConnector, BlockchainConnector};

use crate::rest::{build_router, BankNodeState};

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    server: ServerConfig,
    bank: BankConfig,
    blockchain: BlockchainConfig,
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
    let connector = build_connector(&config.blockchain).await?;
    let blockchain_label: &'static str = match config.blockchain.kind {
        BlockchainKind::Mock => "mock",
        BlockchainKind::Cardano => "cardano",
    };

    info!(
        bind = %config.server.bind,
        bank_id = %config.bank.bank_id,
        account_id = %config.bank.account_id,
        blockchain = blockchain_label,
        version = env!("CARGO_PKG_VERSION"),
        "OBP Bank Node starting"
    );

    let state = BankNodeState {
        connector,
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

async fn build_connector(cfg: &BlockchainConfig) -> anyhow::Result<Arc<dyn BlockchainConnector>> {
    match cfg.kind {
        BlockchainKind::Mock => Ok(Arc::new(MockConnector::new())),
        BlockchainKind::Cardano => {
            let cardano_cfg = cfg
                .cardano
                .clone()
                .context("blockchain.type=cardano requires a blockchain.cardano block")?;
            let c = obp_blockchain::cardano::CardanoConnector::new(cardano_cfg)
                .await
                .context("failed to construct CardanoConnector")?;
            Ok(Arc::new(c))
        }
    }
}

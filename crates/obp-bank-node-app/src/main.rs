//! OBP Bank Node App — demo / manual-test UI (`WIP/APP.md`).
//!
//! Serves the static single-page UI and a whitelisted JSON proxy over the
//! configured Bank Nodes (demo topology: node A on :8088, node B on :8089).
//! The app talks ONLY to Bank Node south-side APIs — never OBP-API, RabbitMQ,
//! or the chain — and holds no business logic: it displays, the nodes decide.
//! The backend exists so node credentials stay out of the browser and no CORS
//! changes are needed on the node.

use anyhow::Context;
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use tracing::info;

mod proxy;

#[cfg(test)]
mod tests;

const CONFIG_FILE: &str = "obp-bank-node-app-config.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// The Bank Nodes this app fronts. Names appear in the UI and on the
    /// proxy URL (`/api/nodes/{name}/...`).
    pub nodes: Vec<NodeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub name: String,
    pub base_url: String,
    /// Bearer token sent on proxied requests. The demo nodes are
    /// unauthenticated on localhost; this is the seam for the OAuth2 +
    /// PSD2-CERT scheme of `DOCS/A1_A2.md` so the app never bakes in the
    /// unauthenticated assumption.
    #[serde(default)]
    pub bearer_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: ServerConfig {
                bind: "0.0.0.0:8090".into(),
            },
            nodes: vec![
                NodeConfig {
                    name: "node-a".into(),
                    base_url: "http://localhost:8088".into(),
                    bearer_token: None,
                },
                NodeConfig {
                    name: "node-b".into(),
                    base_url: "http://localhost:8089".into(),
                    bearer_token: None,
                },
            ],
        }
    }
}

fn load_config() -> anyhow::Result<Config> {
    // `OBP_BANK_NODE_APP_CONFIG` overrides the config file path, mirroring the
    // node's `OBP_BANK_NODE_CONFIG` — it's how several app instances (one per
    // node, as in the roundtrip dev environment) run from the same directory.
    let path =
        std::env::var("OBP_BANK_NODE_APP_CONFIG").unwrap_or_else(|_| CONFIG_FILE.to_string());
    Figment::from(Serialized::defaults(Config::default()))
        .merge(Yaml::file(&path))
        .merge(Env::prefixed("OBP_BN_APP_").split("__"))
        .extract()
        .context("failed to load app config")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "obp_bank_node_app=info,tower_http=info".into()),
        )
        .init();

    let config = load_config()?;
    info!(
        bind = %config.server.bind,
        nodes = ?config.nodes.iter().map(|n| format!("{}={}", n.name, n.base_url)).collect::<Vec<_>>(),
        "OBP Bank Node App starting"
    );

    let app = proxy::build_router(proxy::AppState::new(config.nodes.clone())?);

    let addr: std::net::SocketAddr = config
        .server
        .bind
        .parse()
        .with_context(|| format!("invalid server.bind: {}", config.server.bind))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    axum::serve(listener, app)
        .await
        .context("axum::serve failed")?;
    Ok(())
}

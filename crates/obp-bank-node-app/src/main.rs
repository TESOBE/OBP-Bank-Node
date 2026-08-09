//! OBP Bank Node App — demo / manual-test UI (`WIP/APP.md`).
//!
//! Serves the static single-page UI and a whitelisted JSON proxy over the
//! configured Bank Nodes (demo topology: node A on :8088, node B on :8089).
//! The storyline pages talk ONLY to Bank Node south-side APIs — never OBP-API,
//! RabbitMQ, or the chain — and hold no business logic: they display, the
//! nodes decide. The backend exists so node credentials stay out of the
//! browser and no CORS changes are needed on the node.
//!
//! The one deliberate exception is the `/setup` operator page: with an
//! `obp_api` config block present, an administrator can log in to OBP-API
//! through its OIDC provider (the same authorization-code flow the OBP Portal
//! and API Manager use) and reconcile the instance against the declarative
//! `setup` block — banks, routing schemes, accounts, FX rates, Open Corridor
//! brokers, role grants. Tokens live in a server-side session; the browser
//! never sees a credential. The page never registers users and never touches
//! email validation — service users are provisioned outside the app.

use anyhow::Context;
use figment::providers::{Env, Format, Serialized, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use tracing::info;

mod oidc;
mod proxy;
mod setup;

#[cfg(test)]
mod setup_tests;
#[cfg(test)]
mod tests;

const CONFIG_FILE: &str = "obp-bank-node-app-config.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// The Bank Nodes this app fronts. Names appear in the UI and on the
    /// proxy URL (`/api/nodes/{name}/...`).
    pub nodes: Vec<NodeConfig>,
    /// Per-instance form prefill, keyed by form-field name (e.g.
    /// `other_bank: rt.bank.b`, `other_bank_id: rt.bank.b`). Served verbatim
    /// at `GET /api/ui-defaults`; the UI applies each entry to the matching
    /// Send/Settle input at load. Purely cosmetic — validation still happens
    /// at the node.
    #[serde(default)]
    pub ui_defaults: std::collections::HashMap<String, String>,
    /// OBP-API connection + OIDC client for the `/setup` operator page.
    /// Absent ⇒ the page reports itself as not configured and no OBP-API
    /// call is ever made.
    #[serde(default)]
    pub obp_api: Option<ObpApiConfig>,
    /// Desired Open Corridor state the `/setup` page reconciles OBP-API
    /// against. Only meaningful together with `obp_api`.
    #[serde(default)]
    pub setup: SetupConfig,
}

/// How the `/setup` page reaches and authenticates to OBP-API — the same
/// scheme the OBP Portal / API Manager use: the OIDC provider list comes from
/// OBP-API's `/obp/v5.1.0/well-known`, the admin logs in interactively via
/// the provider's authorization-code flow (PKCE), and this app exchanges the
/// code server-side with its registered consumer's client id/secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObpApiConfig {
    /// e.g. `http://localhost:8080`
    pub base_url: String,
    /// Provider name to select from the well-known list (Portal names:
    /// `obp-oidc`, `keycloak`, `google`).
    #[serde(default = "default_oauth_provider")]
    pub oauth_provider: String,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
    /// Must equal the redirect URL registered for the consumer:
    /// `http://<this app>/setup/callback`.
    pub callback_url: String,
    /// Optional override: fetch the OIDC discovery document from this URL
    /// directly instead of consulting the well-known list.
    #[serde(default)]
    pub discovery_url: Option<String>,
}

fn default_oauth_provider() -> String {
    "obp-oidc".into()
}

/// Declarative desired state for the `/setup` page. Wire-shaped blocks
/// (`routing_schemes`, `broker`) are kept as raw JSON so the YAML carries
/// exactly what OBP-API's endpoints receive — no re-modelling here.
///
/// Single-bank by construction (decided 2026-08-09): the app acts on behalf
/// of ONE node = one bank, so its setup block describes only that bank's
/// world — never another bank's accounts, broker credentials, or grants.
/// The exception is `routing_schemes`, which are system-level catalogue
/// entries (API-operator scope in production; carried here for dev).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetupConfig {
    /// Bodies for `POST /obp/v7.0.0/routing-schemes`, verbatim. Checked via
    /// `GET /routing-schemes/{scheme}`.
    #[serde(default)]
    pub routing_schemes: Vec<serde_json::Value>,
    /// This instance's own bank.
    #[serde(default)]
    pub bank: Option<SetupBank>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupBank {
    pub id: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub accounts: Vec<SetupAccount>,
    /// One entry per direction for `PUT /obp/v2.2.0/banks/{id}/fx`.
    #[serde(default)]
    pub fx: Vec<SetupFxRate>,
    /// Body for `PUT /obp/v7.0.0/banks/{id}/open-corridor/broker`, verbatim.
    #[serde(default)]
    pub broker: Option<serde_json::Value>,
    /// Entitlements at THIS bank for users that already exist (the bank's
    /// node service user). The page grants but never creates users.
    #[serde(default)]
    pub role_grants: Vec<SetupRoleGrant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupAccount {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub currency: String,
    /// Existing OBP username to own the account; defaults to the logged-in
    /// admin. Looked up, never created.
    #[serde(default)]
    pub owner_username: Option<String>,
    /// Account routings (scheme + address). The CARDANO routing on
    /// OBP-INCOMING-SETTLEMENT-ACCOUNT is the bank's on-chain settlement
    /// address — the broker block carries transport coordinates only.
    #[serde(default)]
    pub routings: Vec<SetupRouting>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetupRouting {
    pub scheme: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupFxRate {
    pub from_currency: String,
    pub to_currency: String,
    pub rate: f64,
    pub inverse_rate: f64,
}

/// A role at the configured bank (the bank id is implied — grants outside
/// the own bank are not expressible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupRoleGrant {
    pub username: String,
    pub role: String,
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
            ui_defaults: std::collections::HashMap::new(),
            obp_api: None,
            setup: SetupConfig::default(),
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
        setup = config.obp_api.as_ref().map(|o| o.base_url.as_str()).unwrap_or("(not configured)"),
        "OBP Bank Node App starting"
    );

    let setup_state = config
        .obp_api
        .clone()
        .map(|obp| setup::SetupState::new(obp, config.setup.clone()))
        .transpose()?;
    let app = proxy::build_router(
        proxy::AppState::new(config.nodes.clone(), config.ui_defaults.clone())?,
        setup_state,
    );

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

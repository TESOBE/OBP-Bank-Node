//! Cardano backend — Phase 1: foundation.
//!
//! - Talks to a local `cardano-node` via Ogmios (JSON-RPC over WebSocket).
//! - Loads the Shelley payment key trio (.skey/.vkey/.addr) at construction.
//! - `confirm()` is implemented for real (queries the chain via Ogmios).
//! - `write_*` methods are still stubs — they need tx build + sign + submit,
//!   which lands in Phase 2 once the chain sync is at tip and we can
//!   end-to-end-test against a real preprod node.

pub mod ogmios;
pub mod wallet;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
    BlockchainBackend, BlockchainError, ConfirmationStatus, ExceptionRecord, PromiseRecord,
    Result, SettlementRecord, TxReference,
};

use self::ogmios::OgmiosClient;
use self::wallet::Wallet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardanoConfig {
    /// `ws://host:port` of the Ogmios endpoint in front of cardano-node.
    pub ogmios_url: String,
    /// `preprod` / `preview` / `mainnet` — used for logging + sanity checks.
    pub network: String,
    /// Path to the wallet address file (bech32, e.g. `addr_test1...`).
    pub wallet_address_path: PathBuf,
    /// Path to the verification key envelope (.vkey).
    pub wallet_vkey_path: PathBuf,
    /// Path to the signing key envelope (.skey). Treat as a secret.
    pub wallet_skey_path: PathBuf,
}

pub struct CardanoBackend {
    ogmios: OgmiosClient,
    wallet: Arc<Wallet>,
    network: String,
}

impl CardanoBackend {
    /// Connect to Ogmios and load the wallet from disk.
    pub async fn new(config: CardanoConfig) -> Result<Self> {
        let ogmios = OgmiosClient::new(&config.ogmios_url);

        // Probe the connection up-front so misconfigurations fail at startup
        // rather than on the first transaction.
        let tip = ogmios.tip().await.map_err(map_ogmios)?;
        info!(
            network = %config.network,
            ogmios_url = %config.ogmios_url,
            tip_slot = tip.slot,
            tip_id = %tip.id,
            "connected to Ogmios"
        );

        let wallet = Wallet::load(
            &config.wallet_skey_path,
            &config.wallet_vkey_path,
            &config.wallet_address_path,
        )
        .map_err(|e| BlockchainError::Internal(format!("wallet load failed: {e}")))?;
        info!(address = %wallet.address, "wallet loaded");

        Ok(Self {
            ogmios,
            wallet: Arc::new(wallet),
            network: config.network,
        })
    }

    pub fn network(&self) -> &str {
        &self.network
    }

    pub fn wallet_address(&self) -> &str {
        &self.wallet.address
    }
}

#[async_trait]
impl BlockchainBackend for CardanoBackend {
    async fn write_promise(&self, _p: &PromiseRecord) -> Result<TxReference> {
        Err(write_not_yet_implemented("promise"))
    }

    async fn write_settlement(&self, _s: &SettlementRecord) -> Result<TxReference> {
        Err(write_not_yet_implemented("settlement"))
    }

    async fn write_exception(&self, _e: &ExceptionRecord) -> Result<TxReference> {
        Err(write_not_yet_implemented("exception"))
    }

    /// Check whether a previously-submitted tx is present on chain by looking
    /// for any UTxO whose tx hash matches. This is a coarse confirmation
    /// signal — it tells us the tx landed and at least one of its outputs is
    /// still unspent. It cannot report depth without a chain-sync follower.
    /// A persistent-connection chain-sync subscription will replace this in
    /// Phase 2.
    async fn confirm(&self, r: &TxReference) -> Result<ConfirmationStatus> {
        if r.chain != "cardano" {
            return Err(BlockchainError::Internal(format!(
                "confirm: TxReference is for chain '{}', expected 'cardano'",
                r.chain
            )));
        }
        let utxos = self
            .ogmios
            .utxos_at(&self.wallet.address)
            .await
            .map_err(map_ogmios)?;
        let found = utxos.iter().any(|u| {
            u.get("transaction")
                .and_then(|t| t.get("id"))
                .and_then(|id| id.as_str())
                .map(|id| id.eq_ignore_ascii_case(&r.tx_id))
                .unwrap_or(false)
        });
        if found {
            Ok(ConfirmationStatus::Confirmed { depth: 1 })
        } else {
            Ok(ConfirmationStatus::Pending)
        }
    }
}

fn map_ogmios(e: ogmios::OgmiosError) -> BlockchainError {
    match e {
        ogmios::OgmiosError::Rpc { code, message } => {
            BlockchainError::Rejected(format!("ogmios rpc {code}: {message}"))
        }
        other => BlockchainError::Transport(other.to_string()),
    }
}

fn write_not_yet_implemented(kind: &str) -> BlockchainError {
    warn!(
        record_kind = kind,
        "CardanoBackend.write_{kind} called but Phase 2 (tx build + sign + submit) is not yet \
         implemented — use MockBackend until chain sync finishes and tx-builder lands"
    );
    BlockchainError::Internal(format!(
        "write_{kind}: CardanoBackend tx submission not yet implemented (Phase 2). \
         Phase 1 supports connect, wallet load, and confirm() only. \
         Set blockchain.type=mock until Phase 2 lands."
    ))
}

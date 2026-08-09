//! Cardano backend.
//!
//! - Talks to a local `cardano-node` via Ogmios (JSON-RPC over WebSocket).
//! - Loads the Shelley payment signing key (.skey) at construction; the
//!   verification key and enterprise address are derived from it.
//! - `write_*` build, sign, and submit metadata-only self-payments carrying the
//!   notary record's hash commitment (via the shared [`tx`] builder).
//! - `confirm()` reads the [`follower`] chain-sync task for real confirmation
//!   depth (with rollback handling); transactions the follower never saw —
//!   submitted by an earlier process — fall back to a coarse UTxO-presence
//!   check reported as depth 1.

pub mod follower;
pub mod ogmios;
pub mod settlement;
pub mod tx;
pub mod wallet;

pub use settlement::CardanoAdaSettlement;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

use crate::{
    BlockchainBackend, BlockchainError, ConfirmationStatus, ExceptionRecord, PromiseRecord,
    Result, SettlementRecord, TxReference,
};

use self::follower::ChainFollower;
use self::ogmios::OgmiosClient;
use self::wallet::Wallet;

/// Every field has a default matching the bundled installation package, so a
/// bare `blockchain: { type: cardano }` config boots against the local stack.
/// The verification key and wallet address are derived from the signing key,
/// so the `.skey` file is the only wallet input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardanoConfig {
    /// `ws://host:port` of the Ogmios endpoint in front of cardano-node.
    /// Default: the bundled container. Point at a managed provider to
    /// override.
    #[serde(default = "default_ogmios_url")]
    pub ogmios_url: String,
    /// `preprod` (default) / `preview` / `mainnet`.
    #[serde(default = "default_network")]
    pub network: String,
    /// Path to the signing key envelope (.skey). Treat as a secret.
    #[serde(default = "default_skey_path")]
    pub wallet_skey_path: PathBuf,
    /// Per-call Ogmios deadline in seconds. UTxO-by-address queries scan the
    /// node's whole UTxO set and routinely take ~30-60s on public testnets.
    #[serde(default = "default_query_timeout_secs")]
    pub query_timeout_secs: u64,
}

fn default_ogmios_url() -> String {
    "ws://localhost:1337".into()
}

fn default_network() -> String {
    "preprod".into()
}

fn default_skey_path() -> PathBuf {
    "./secrets/cardano.skey".into()
}

fn default_query_timeout_secs() -> u64 {
    90
}

impl Default for CardanoConfig {
    fn default() -> Self {
        CardanoConfig {
            ogmios_url: default_ogmios_url(),
            network: default_network(),
            wallet_skey_path: default_skey_path(),
            query_timeout_secs: default_query_timeout_secs(),
        }
    }
}

pub struct CardanoBackend {
    ogmios: OgmiosClient,
    wallet: Arc<Wallet>,
    network: String,
    /// Serialises submissions from this wallet so two concurrent writes cannot
    /// select the same UTxO and collide as a double-spend. Shared with the ADA
    /// settlement backend (same wallet) via `CardanoAdaSettlement::from_backend`.
    submit_lock: Arc<Mutex<()>>,
    /// Chain-sync follower for real confirmation depth (also shared via
    /// `CardanoAdaSettlement::from_backend`). Every submitted tx id is
    /// registered with it before submission.
    follower: Arc<ChainFollower>,
}

impl CardanoBackend {
    /// Connect to Ogmios and load the wallet from disk.
    pub async fn new(config: CardanoConfig) -> Result<Self> {
        let ogmios = OgmiosClient::with_timeout(
            &config.ogmios_url,
            std::time::Duration::from_secs(config.query_timeout_secs),
        );

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

        let wallet = Wallet::load(&config.wallet_skey_path, &config.network)
            .map_err(|e| BlockchainError::Internal(format!("wallet load failed: {e}")))?;
        info!(address = %wallet.address, "wallet loaded");

        Ok(Self {
            ogmios,
            wallet: Arc::new(wallet),
            network: config.network,
            submit_lock: Arc::new(Mutex::new(())),
            follower: ChainFollower::spawn(config.ogmios_url),
        })
    }

    pub fn network(&self) -> &str {
        &self.network
    }

    pub fn wallet_address(&self) -> &str {
        &self.wallet.address
    }

    /// Write a notary record as a min-UTxO self-payment carrying
    /// `{schema, commitment, ts}` in tx metadata. Only the commitment hash
    /// reaches the chain — never cleartext payment data (see the hash-only
    /// privacy decision). Returns the on-chain [`TxReference`].
    async fn write_notary(&self, schema: &str, commitment: &str) -> Result<TxReference> {
        let pp_json = self.ogmios.protocol_parameters().await.map_err(map_ogmios)?;
        let pp = tx::ProtocolParams::from_ogmios(&pp_json)?;
        let tip = self.ogmios.tip().await.map_err(map_ogmios)?;

        let metadatum = tx::text_record_metadatum(&[
            ("schema", schema),
            ("commitment", commitment),
            ("ts", &Utc::now().to_rfc3339()),
        ]);

        // Hold the wallet lock across UTxO read → build → submit so a concurrent
        // writer cannot select the same input underneath us.
        let _guard = self.submit_lock.lock().await;
        let utxo_entries = self
            .ogmios
            .utxos_at(&self.wallet.address)
            .await
            .map_err(map_ogmios)?;
        let utxos = tx::parse_utxos(&utxo_entries);

        let signed = tx::build_signed_payment(
            &self.wallet,
            &self.network,
            &utxos,
            &pp,
            &self.wallet.address, // self-payment
            tx::MIN_UTXO_LOVELACE,
            tip.slot,
            Some((tx::OBP_METADATA_LABEL, metadatum)),
        )?;

        // Register with the follower BEFORE submitting so the inclusion block
        // cannot race past the registration.
        self.follower.watch(&signed.tx_id);
        let submitted = self.ogmios.submit_transaction(&signed.cbor_hex).await;
        let submitted_id = match submitted {
            Ok(id) => id,
            Err(e) => {
                self.follower.unwatch(&signed.tx_id);
                return Err(map_ogmios(e));
            }
        };
        if !submitted_id.eq_ignore_ascii_case(&signed.tx_id) {
            return Err(BlockchainError::Internal(format!(
                "node reported tx id {submitted_id} but we computed {}",
                signed.tx_id
            )));
        }
        info!(schema, tx_id = %signed.tx_id, fee = signed.fee, "notary record written to Cardano");
        Ok(TxReference {
            chain: "cardano".into(),
            tx_id: signed.tx_id,
            submitted_at: Utc::now(),
        })
    }
}

#[async_trait]
impl BlockchainBackend for CardanoBackend {
    async fn write_promise(&self, p: &PromiseRecord) -> Result<TxReference> {
        // The Promise already carries a salted commitment; write it as-is.
        self.write_notary(&p.schema, &p.commitment).await
    }

    async fn write_settlement(&self, s: &SettlementRecord, salt: &[u8]) -> Result<TxReference> {
        self.write_notary(SettlementRecord::SCHEMA_V1, &s.commit_v1(salt)).await
    }

    async fn write_exception(&self, e: &ExceptionRecord, salt: &[u8]) -> Result<TxReference> {
        self.write_notary(ExceptionRecord::SCHEMA_V1, &e.commit_v1(salt)).await
    }

    /// Confirmation via the chain-sync follower: real depth
    /// (`tip − inclusion + 1`), and a rollback past the inclusion block
    /// reverts to `Pending`. Transactions this process never submitted (and
    /// so never watched — e.g. from a previous run) fall back to the coarse
    /// UTxO-presence check, reported as depth 1.
    async fn confirm(&self, r: &TxReference) -> Result<ConfirmationStatus> {
        if r.chain != "cardano" {
            return Err(BlockchainError::Internal(format!(
                "confirm: TxReference is for chain '{}', expected 'cardano'",
                r.chain
            )));
        }
        if let Some(status) = self.follower.status(&r.tx_id) {
            return Ok(status);
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


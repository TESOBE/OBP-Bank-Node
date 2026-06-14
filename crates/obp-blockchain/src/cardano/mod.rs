//! Cardano backend.
//!
//! - Talks to a local `cardano-node` via Ogmios (JSON-RPC over WebSocket).
//! - Loads the Shelley payment key trio (.skey/.vkey/.addr) at construction.
//! - `write_*` build, sign, and submit metadata-only self-payments carrying the
//!   notary record's hash commitment (via the shared [`tx`] builder).
//! - `confirm()` is a coarse UTxO-presence check; a chain-sync follower
//!   reporting real depth + rollbacks is the remaining Phase-2 upgrade.

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
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::info;

use crate::{
    BlockchainBackend, BlockchainError, ConfirmationStatus, ExceptionRecord, PromiseRecord,
    Result, SettlementRecord, TxReference,
};

use self::ogmios::OgmiosClient;
use self::wallet::Wallet;

/// Schema tags for the notary record types (domain-separated, versioned). The
/// Promise tag travels on the [`PromiseRecord`] itself (`obp.promise.v1`).
const SCHEMA_SETTLEMENT: &str = "obp.settlement.v1";
const SCHEMA_EXCEPTION: &str = "obp.exception.v1";

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
    /// Serialises submissions from this wallet so two concurrent writes cannot
    /// select the same UTxO and collide as a double-spend. Shared with the ADA
    /// settlement backend (same wallet) via `CardanoAdaSettlement::from_backend`.
    submit_lock: Arc<Mutex<()>>,
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
            submit_lock: Arc::new(Mutex::new(())),
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

        let submitted_id = self
            .ogmios
            .submit_transaction(&signed.cbor_hex)
            .await
            .map_err(map_ogmios)?;
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

/// SHA-256 commitment over a record's canonical JSON. Keeps cleartext fields
/// (amounts, reasons, identifiers) off-chain while still anchoring them.
///
/// TODO: Settlement/Exception commitments are currently unsalted — fold in a
/// per-record salt (as the Promise path does via the outbox) before production,
/// so low-entropy fields can't be brute-forced from the on-chain hash.
fn commit_record<T: Serialize>(record: &T) -> String {
    let bytes = serde_json::to_vec(record).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hex::encode(hasher.finalize())
}

#[async_trait]
impl BlockchainBackend for CardanoBackend {
    async fn write_promise(&self, p: &PromiseRecord) -> Result<TxReference> {
        // The Promise already carries a salted commitment; write it as-is.
        self.write_notary(&p.schema, &p.commitment).await
    }

    async fn write_settlement(&self, s: &SettlementRecord) -> Result<TxReference> {
        self.write_notary(SCHEMA_SETTLEMENT, &commit_record(s)).await
    }

    async fn write_exception(&self, e: &ExceptionRecord) -> Result<TxReference> {
        self.write_notary(SCHEMA_EXCEPTION, &commit_record(e)).await
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

#[cfg(test)]
mod tests {
    use super::*;

    fn exception(reason: &str) -> ExceptionRecord {
        ExceptionRecord {
            bank_id: "ke.01.kcs".into(),
            transaction_request_id: "tr-1".into(),
            reason: reason.into(),
            raised_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn commit_record_is_deterministic_and_hides_cleartext() {
        let a = commit_record(&exception("unroutable destination"));
        let b = commit_record(&exception("unroutable destination"));
        assert_eq!(a, b, "same record ⇒ same commitment");
        assert_eq!(a.len(), 64, "hex SHA-256");
        // The cleartext reason must not be recoverable from the commitment.
        assert!(!a.contains("unroutable"));
    }

    #[test]
    fn commit_record_differs_on_different_input() {
        assert_ne!(commit_record(&exception("reason a")), commit_record(&exception("reason b")));
    }
}


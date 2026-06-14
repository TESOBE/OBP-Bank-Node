//! ADA settlement on Cardano — the proof-of-concept value leg.
//!
//! Settles a netted fiat position by transferring ADA from the debtor bank's
//! wallet to the creditor bank's address, sized at the **settle-time spot rate**
//! (`net_fiat ÷ ADA-rate`). Because the ADA quantity is fixed only at the moment
//! of settlement, there is no exposure to ADA drift during the promise→netting
//! window; the residual cost is ordinary FX/liquidity friction as each bank
//! enters and exits ADA — exactly what a stablecoin settlement backend
//! (AOC / cKES) removes while plugging into the same [`SettlementBackend`] trait.
//!
//! This backend runs in the **debtor** bank's node: it signs with the local
//! wallet, so it can only pay *out* from its own address. A settlement
//! instruction is dispatched (over Interface C) to whichever node is the debtor;
//! that node settles and reports the [`TxReference`] back so OBP-API can flip the
//! Settlement to `SETTLED`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Mutex;
use tracing::info;

use crate::cardano::ogmios::{OgmiosClient, OgmiosError};
use crate::cardano::wallet::Wallet;
use crate::cardano::{tx, CardanoBackend};
use crate::settlement::{FxSource, SettlementBackend, SettlementInstruction, SettlementOutcome};
use crate::{BlockchainError, ConfirmationStatus, Result, TxReference};

/// Schema tag for the on-chain ADA settlement value tx's metadata.
const SCHEMA_ADA_SETTLEMENT: &str = "obp.settlement.ada.v1";

/// 1 ADA = 1_000_000 lovelace.
pub const LOVELACE_PER_ADA: u128 = 1_000_000;

/// ADA value-settlement backend. Holds an Ogmios connection, the bank's wallet
/// (the payer), and an off-chain [`FxSource`] for settle-time rates.
pub struct CardanoAdaSettlement {
    ogmios: OgmiosClient,
    wallet: Arc<Wallet>,
    network: String,
    fx: Arc<dyn FxSource>,
    /// Serialises submissions from the shared wallet (see
    /// [`CardanoBackend::submit_lock`]).
    submit_lock: Arc<Mutex<()>>,
}

impl CardanoAdaSettlement {
    pub fn new(
        ogmios: OgmiosClient,
        wallet: Arc<Wallet>,
        network: impl Into<String>,
        fx: Arc<dyn FxSource>,
    ) -> Self {
        Self {
            ogmios,
            wallet,
            network: network.into(),
            fx,
            submit_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Build an ADA settlement backend that shares an existing
    /// [`CardanoBackend`]'s Ogmios connection, wallet, and submission lock — the
    /// common wiring, since a node runs one notary and one settlement backend
    /// over one wallet. Sharing the lock serialises *all* writes from that wallet.
    pub fn from_backend(backend: &CardanoBackend, fx: Arc<dyn FxSource>) -> Self {
        Self {
            ogmios: backend.ogmios.clone(),
            wallet: Arc::clone(&backend.wallet),
            network: backend.network.clone(),
            fx,
            submit_lock: Arc::clone(&backend.submit_lock),
        }
    }

    /// The address this node settles *from*. Only instructions whose debtor
    /// account matches can be settled here.
    pub fn payer_address(&self) -> &str {
        &self.wallet.address
    }

    /// Build an ADA payment (debtor wallet → `to_address`, `lovelace`), sign
    /// with the local wallet, and submit via Ogmios. Returns the tx id.
    ///
    /// `idempotency_key` is folded into the tx metadata so the on-chain record
    /// is self-describing. Note this does *not* by itself prevent a double
    /// settlement — the caller must `confirm()` any prior tx id for the
    /// instruction before resubmitting; the shared [`Self::submit_lock`] only
    /// guards against concurrent UTxO contention from this wallet.
    async fn submit_ada_transfer(
        &self,
        to_address: &str,
        lovelace: u64,
        idempotency_key: &str,
    ) -> Result<String> {
        let pp_json = self.ogmios.protocol_parameters().await.map_err(map_ogmios)?;
        let pp = tx::ProtocolParams::from_ogmios(&pp_json)?;
        let tip = self.ogmios.tip().await.map_err(map_ogmios)?;

        let metadatum = tx::text_record_metadatum(&[
            ("schema", SCHEMA_ADA_SETTLEMENT),
            ("idempotency_key", idempotency_key),
        ]);

        // Serialise UTxO read → build → submit so concurrent settlements (or a
        // notary write sharing this wallet) cannot select the same input.
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
            to_address,
            lovelace,
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
        info!(
            to = %to_address,
            lovelace,
            tx_id = %signed.tx_id,
            fee = signed.fee,
            "ADA settlement transfer submitted"
        );
        Ok(signed.tx_id)
    }
}

#[async_trait]
impl SettlementBackend for CardanoAdaSettlement {
    fn system(&self) -> &str {
        "cardano-ada"
    }

    async fn settle(&self, instruction: &SettlementInstruction) -> Result<SettlementOutcome> {
        // This node can only pay out from its own wallet. Reject instructions
        // where we are not the debtor before doing any work.
        if instruction.debtor.account != self.wallet.address {
            return Err(BlockchainError::Rejected(format!(
                "this node settles from {}, but instruction debtor account is {}",
                self.wallet.address, instruction.debtor.account
            )));
        }

        // 1. Settle-time rate, off-chain (no on-chain oracle on the settlement chain).
        let quote = self.fx.quote("ADA", &instruction.currency).await?;

        // 2. Size the transfer in lovelace at spot.
        let lovelace = fiat_minor_to_lovelace(instruction.net_amount_minor, quote.minor_per_whole_asset)?;
        info!(
            snapshot = %instruction.snapshot_id,
            currency = %instruction.currency,
            net_minor = instruction.net_amount_minor,
            rate_minor_per_ada = quote.minor_per_whole_asset,
            lovelace,
            creditor = %instruction.creditor.account,
            "ADA settlement sized at settle-time spot"
        );

        // 3. Build + sign + submit the transfer (PHASE 2).
        let tx_id = self
            .submit_ada_transfer(
                &instruction.creditor.account,
                lovelace,
                &instruction.idempotency_key,
            )
            .await?;

        Ok(SettlementOutcome {
            tx: TxReference {
                chain: "cardano".into(),
                tx_id,
                submitted_at: Utc::now(),
            },
            asset: "ADA".into(),
            asset_amount: lovelace.to_string(),
            fx: Some(quote),
        })
    }

    /// Coarse confirmation: does a UTxO produced by `tx` exist at the payer's
    /// address? Same Phase-1 signal as [`CardanoBackend::confirm`]; Phase 2
    /// promotes both to a chain-sync follower reporting real depth.
    async fn confirm(&self, tx: &TxReference) -> Result<ConfirmationStatus> {
        if tx.chain != "cardano" {
            return Err(BlockchainError::Internal(format!(
                "confirm: TxReference is for chain '{}', expected 'cardano'",
                tx.chain
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
                .map(|id| id.eq_ignore_ascii_case(&tx.tx_id))
                .unwrap_or(false)
        });
        Ok(if found {
            ConfirmationStatus::Confirmed { depth: 1 }
        } else {
            ConfirmationStatus::Pending
        })
    }
}

/// Convert a fiat net (minor units of the book currency) to lovelace at the
/// given rate, using integer math only:
///
/// `lovelace = net_amount_minor × LOVELACE_PER_ADA ÷ minor_per_whole_asset`
///
/// Errors on a zero rate, on a net that rounds below one lovelace, or on a
/// result that overflows `u64` (the lovelace domain).
pub fn fiat_minor_to_lovelace(net_amount_minor: u128, minor_per_whole_asset: u128) -> Result<u64> {
    if minor_per_whole_asset == 0 {
        return Err(BlockchainError::Rejected("fx rate of zero".into()));
    }
    let lovelace = net_amount_minor
        .checked_mul(LOVELACE_PER_ADA)
        .ok_or_else(|| BlockchainError::Rejected("amount overflow in fiat→lovelace".into()))?
        / minor_per_whole_asset;
    if lovelace == 0 {
        return Err(BlockchainError::Rejected(
            "net amount rounds to zero lovelace at this rate".into(),
        ));
    }
    u64::try_from(lovelace)
        .map_err(|_| BlockchainError::Rejected("settlement exceeds u64 lovelace domain".into()))
}

fn map_ogmios(e: OgmiosError) -> BlockchainError {
    match e {
        OgmiosError::Rpc { code, message } => {
            BlockchainError::Rejected(format!("ogmios rpc {code}: {message}"))
        }
        other => BlockchainError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settlement::{PartyRef, StubFxSource};
    use ed25519_dalek::SigningKey;

    fn test_settlement(payer_addr: &str, rate_minor_per_ada: u128) -> CardanoAdaSettlement {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let wallet = Wallet {
            address: payer_addr.to_string(),
            signing_key,
            verifying_key,
        };
        CardanoAdaSettlement::new(
            OgmiosClient::new("ws://127.0.0.1:1337"),
            Arc::new(wallet),
            "preprod",
            Arc::new(StubFxSource::new(rate_minor_per_ada)),
        )
    }

    fn instruction(debtor_addr: &str) -> SettlementInstruction {
        SettlementInstruction {
            snapshot_id: "snap-1".into(),
            debtor: PartyRef {
                bank_id: "bank-a".into(),
                account: debtor_addr.into(),
            },
            creditor: PartyRef {
                bank_id: "bank-b".into(),
                account: "addr_test1creditor".into(),
            },
            currency: "KES".into(),
            net_amount_minor: 500, // 5.00 KES
            idempotency_key: "idem-1".into(),
        }
    }

    #[test]
    fn converts_5_kes_at_3542_to_lovelace() {
        // 1 ADA = 35.42 KES ⇒ 3542 minor/ADA. 5.00 KES = 500 minor.
        // lovelace = 500 * 1_000_000 / 3542 = 141_163.
        assert_eq!(fiat_minor_to_lovelace(500, 3542).unwrap(), 141_163);
    }

    #[test]
    fn rejects_zero_rate() {
        assert!(fiat_minor_to_lovelace(500, 0).is_err());
    }

    #[test]
    fn rejects_amount_that_rounds_below_one_lovelace() {
        // 1 minor at 2_000_000 minor/ADA ⇒ 1_000_000 / 2_000_000 = 0 lovelace.
        assert!(fiat_minor_to_lovelace(1, 2_000_000).is_err());
    }

    #[tokio::test]
    async fn rejects_instruction_when_not_the_debtor() {
        let backend = test_settlement("addr_test1me", 3542);
        let err = backend
            .settle(&instruction("addr_test1someone_else"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BlockchainError::Rejected(_)),
            "expected Rejected, got {err:?}"
        );
    }

    #[tokio::test]
    async fn debtor_match_proceeds_past_guard_to_chain_write() {
        // When we ARE the debtor, settle() does the real off-chain work (FX +
        // sizing) and then attempts the chain write. With no Ogmios node at the
        // test URL, that fails at the network layer — proving we got past the
        // debtor guard and into the real submit path (not the old stub).
        let backend = test_settlement("addr_test1me", 3542);
        let err = backend.settle(&instruction("addr_test1me")).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("this node settles from"),
            "must be past the debtor guard, got: {msg}"
        );
    }
}

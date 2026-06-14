//! Shared Cardano transaction builder (Phase 2).
//!
//! One primitive underlies both stubbed write paths — notary metadata writes
//! (`CardanoBackend::write_*`) and ADA value settlement
//! (`CardanoAdaSettlement::submit_ada_transfer`): *select inputs → build outputs
//! → compute fee → assemble → hash → sign*. They differ only in their
//! output(s) and metadata, so the assembly lives here once.
//!
//! Built on `pallas-txbuilder`'s Conway **raw** builder, which does no
//! balancing — coin selection, fee, change, and TTL are computed here (the
//! `pallas` API was confirmed against 1.1.0 before writing). Everything except
//! actual on-chain acceptance is exercised without a node: coin selection and
//! fee math are pure, and the full build+sign path runs against synthetic UTxOs
//! and a synthetic wallet in the tests.

use std::collections::BTreeMap;

use pallas_addresses::Address;
use pallas_codec::minicbor;
use pallas_codec::utils::KeyValuePairs;
use pallas_crypto::hash::Hash;
use pallas_crypto::key::ed25519::SecretKey;
use pallas_primitives::alonzo::AuxiliaryData;
use pallas_primitives::Metadatum;
use pallas_txbuilder::{BuildConway, BuiltTransaction, Input, Output, StagingTransaction};

use crate::cardano::wallet::Wallet;
use crate::{BlockchainError, Result};

/// Transaction-metadata label for OBP notary records (`0x4f4250` = "OBP").
pub const OBP_METADATA_LABEL: u64 = 5_198_160;

/// 1 ADA in lovelace.
pub const LOVELACE_PER_ADA: u64 = 1_000_000;

/// Conservative min-UTxO floor. The protocol min for an ADA-only output is
/// ~0.97 ADA; 1 ADA clears it and is what self-payments (notary writes) carry.
pub const MIN_UTXO_LOVELACE: u64 = LOVELACE_PER_ADA;

/// How many slots ahead to set `invalid_hereafter` (TTL). ~2h at 1 slot/s, so a
/// stuck tx expires instead of lingering indefinitely.
pub const TTL_SLOTS_AHEAD: u64 = 7_200;

/// Cardano caps a single metadata text/bytes value at 64 bytes; longer values
/// are split into an array of chunks (the standard CIP-20 convention).
const METADATUM_VALUE_MAX: usize = 64;

/// Bytes of slack added to the measured tx size when computing the required
/// fee, covering the ±1-2 byte wobble in the fee field's own CBOR encoding.
const FEE_SIZE_MARGIN: u64 = 4;

const MAX_FEE_ITERS: usize = 8;

/// A spendable output at the payer's address. PoC settlement wallets hold
/// ADA-only UTxOs, so only the lovelace amount is tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utxo {
    pub tx_hash: String,
    pub index: u64,
    pub lovelace: u64,
}

/// The protocol parameters the fee + min-UTxO math needs.
#[derive(Debug, Clone, Copy)]
pub struct ProtocolParams {
    /// `minFeeCoefficient` — lovelace per tx byte.
    pub fee_coefficient_per_byte: u64,
    /// `minFeeConstant` — flat lovelace added to every fee.
    pub fee_constant: u64,
    /// `maxTransactionSize` in bytes — a built tx exceeding this is rejected.
    pub max_tx_size: u64,
}

impl ProtocolParams {
    /// `fee = fee_constant + fee_coefficient_per_byte × size`.
    pub fn min_fee(&self, tx_size_bytes: u64) -> u64 {
        self.fee_constant + self.fee_coefficient_per_byte * tx_size_bytes
    }

    /// Parse the Ogmios v6 `queryLedgerState/protocolParameters` shape. Fees are
    /// `minFeeCoefficient: <int>` and `minFeeConstant: { ada: { lovelace: <int> } }`
    /// (older Ogmios returns a bare integer for the constant — both accepted).
    pub fn from_ogmios(v: &serde_json::Value) -> Result<Self> {
        let fee_coefficient_per_byte = v
            .get("minFeeCoefficient")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| missing("minFeeCoefficient"))?;
        let fee_constant = v
            .get("minFeeConstant")
            .map(ada_lovelace_or_int)
            .transpose()?
            .ok_or_else(|| missing("minFeeConstant"))?;
        let max_tx_size = v
            .get("maxTransactionSize")
            .and_then(|m| m.get("bytes").and_then(|b| b.as_u64()).or_else(|| m.as_u64()))
            .ok_or_else(|| missing("maxTransactionSize"))?;
        Ok(Self {
            fee_coefficient_per_byte,
            fee_constant,
            max_tx_size,
        })
    }
}

/// The output of a successful build: ready to submit, plus the bookkeeping the
/// caller logs / reconciles.
#[derive(Debug, Clone)]
pub struct SignedTx {
    /// Hex blake2b-256 of the tx body — the id the node will report on submit.
    pub tx_id: String,
    /// Hex CBOR of the signed transaction, for `submitTransaction`.
    pub cbor_hex: String,
    pub fee: u64,
    pub num_inputs: usize,
    /// Change returned to the payer, if any (dust below min-UTxO is folded into
    /// the fee rather than emitted as an output).
    pub change: Option<u64>,
}

/// Parse an Ogmios `queryLedgerState/utxo` array into spendable ADA-only UTxOs.
/// Entries carrying native tokens are skipped — the PoC wallets are ADA-only and
/// spending a multi-asset UTxO would require balancing those assets in outputs.
pub fn parse_utxos(entries: &[serde_json::Value]) -> Vec<Utxo> {
    let mut out = Vec::new();
    for e in entries {
        let value = match e.get("value") {
            Some(v) => v,
            None => continue,
        };
        // ADA-only: the value object must have exactly the `ada` key.
        let only_ada = value.as_object().map(|m| m.keys().all(|k| k == "ada")).unwrap_or(false);
        if !only_ada {
            continue;
        }
        let lovelace = value
            .get("ada")
            .and_then(|a| a.get("lovelace"))
            .and_then(|l| l.as_u64());
        let tx_hash = e.get("transaction").and_then(|t| t.get("id")).and_then(|i| i.as_str());
        let index = e.get("index").and_then(|i| i.as_u64());
        if let (Some(tx_hash), Some(index), Some(lovelace)) = (tx_hash, index, lovelace) {
            out.push(Utxo {
                tx_hash: tx_hash.to_string(),
                index,
                lovelace,
            });
        }
    }
    out
}

/// Largest-first coin selection: accumulate UTxOs until the running total
/// reaches `target`. Returns the chosen set, or an error if the wallet can't
/// cover `target`. Deterministic given the UTxO set.
pub fn select_largest_first(utxos: &[Utxo], target: u64) -> Result<Vec<Utxo>> {
    let mut sorted = utxos.to_vec();
    sorted.sort_by(|a, b| b.lovelace.cmp(&a.lovelace).then(a.tx_hash.cmp(&b.tx_hash)));
    let mut chosen = Vec::new();
    let mut acc: u64 = 0;
    for u in sorted {
        acc = acc.saturating_add(u.lovelace);
        chosen.push(u);
        if acc >= target {
            return Ok(chosen);
        }
    }
    Err(BlockchainError::Rejected(format!(
        "insufficient funds: have {acc} lovelace, need {target}"
    )))
}

/// Build, balance, and sign an ADA payment from `wallet` to `to_address`.
///
/// - `to_address` == the wallet's own address makes this a self-payment, which
///   is how notary metadata writes are carried on-chain.
/// - `metadata` is an optional `(label, value)` attached as Conway auxiliary
///   data; `pallas` computes and links its hash into the body.
///
/// Coin selection + fee + change are resolved iteratively: build → measure the
/// signed size → recompute the required fee → rebuild, converging in 1-2 passes
/// for these simple ADA-only txs. Change below min-UTxO is folded into the fee
/// (no dust outputs).
#[allow(clippy::too_many_arguments)]
pub fn build_signed_payment(
    wallet: &Wallet,
    network: &str,
    utxos: &[Utxo],
    pp: &ProtocolParams,
    to_address: &str,
    lovelace: u64,
    tip_slot: u64,
    metadata: Option<(u64, Metadatum)>,
) -> Result<SignedTx> {
    let to = parse_address(to_address)?;
    let change_addr = parse_address(&wallet.address)?;
    let network_id = network_id_byte(network);
    let ttl = tip_slot.saturating_add(TTL_SLOTS_AHEAD);
    let secret = SecretKey::from(wallet.signing_key.to_bytes());
    let aux_bytes = match metadata {
        Some((label, datum)) => Some(encode_auxiliary_data(label, datum)?),
        None => None,
    };

    // Start from a fee estimate for a ~250-byte tx; the loop tightens it.
    let mut fee = pp.min_fee(250);

    for _ in 0..MAX_FEE_ITERS {
        let chosen = select_largest_first(utxos, lovelace.saturating_add(fee))?;
        let sum: u64 = chosen.iter().map(|u| u.lovelace).sum();
        let remainder = sum - lovelace - fee; // select guarantees sum ≥ lovelace + fee

        // Emit change only if it clears min-UTxO; otherwise fold dust into the fee.
        let mut outputs = vec![(to.clone(), lovelace)];
        let (eff_fee, change) = if remainder >= MIN_UTXO_LOVELACE {
            outputs.push((change_addr.clone(), remainder));
            (fee, Some(remainder))
        } else {
            (fee + remainder, None)
        };

        let built = assemble_and_sign(
            &chosen,
            &outputs,
            eff_fee,
            network_id,
            ttl,
            aux_bytes.as_deref(),
            &secret,
        )?;

        let size = built.tx_bytes.0.len() as u64;
        if size > pp.max_tx_size {
            return Err(BlockchainError::Rejected(format!(
                "built tx is {size} bytes, over the {} limit",
                pp.max_tx_size
            )));
        }
        let required = pp.min_fee(size + FEE_SIZE_MARGIN);
        if eff_fee >= required {
            return Ok(SignedTx {
                tx_id: hex::encode(built.tx_hash.0),
                cbor_hex: hex::encode(&built.tx_bytes.0),
                fee: eff_fee,
                num_inputs: chosen.len(),
                change,
            });
        }
        // Under-paid: bump to the required fee and rebuild.
        fee = required;
    }

    Err(BlockchainError::Internal(
        "fee did not converge after several iterations".into(),
    ))
}

/// Assemble a Conway tx from selected inputs and outputs, then attach the
/// wallet's Ed25519 witness. Returns the signed transaction (body hash + CBOR).
fn assemble_and_sign(
    inputs: &[Utxo],
    outputs: &[(Address, u64)],
    fee: u64,
    network_id: u8,
    ttl_slot: u64,
    aux: Option<&[u8]>,
    secret: &SecretKey,
) -> Result<BuiltTransaction> {
    let mut tx = StagingTransaction::new();
    for u in inputs {
        let hash: Hash<32> = u
            .tx_hash
            .parse()
            .map_err(|_| BlockchainError::Internal(format!("bad utxo tx hash: {}", u.tx_hash)))?;
        tx = tx.input(Input::new(hash, u.index));
    }
    for (addr, amount) in outputs {
        tx = tx.output(Output::new(addr.clone(), *amount));
    }
    tx = tx.fee(fee).network_id(network_id).invalid_from_slot(ttl_slot);
    if let Some(bytes) = aux {
        tx = tx.add_auxiliary_data(bytes.to_vec());
    }
    let built = tx
        .build_conway_raw()
        .map_err(|e| BlockchainError::Internal(format!("tx build failed: {e:?}")))?;
    built
        .sign(secret)
        .map_err(|e| BlockchainError::Internal(format!("tx signing failed: {e:?}")))
}

/// Build a flat `Metadatum::Map` of text key → text value, splitting any value
/// over 64 bytes into a chunked array (CIP-20 convention). Callers use this to
/// turn a notary record into transaction metadata.
pub fn text_record_metadatum(pairs: &[(&str, &str)]) -> Metadatum {
    let entries: Vec<(Metadatum, Metadatum)> = pairs
        .iter()
        .map(|(k, v)| (Metadatum::Text((*k).to_string()), text_value(v)))
        .collect();
    Metadatum::Map(KeyValuePairs::from(entries))
}

/// A single text value, chunked into a `Metadatum::Array` if it exceeds the
/// 64-byte per-value limit.
fn text_value(s: &str) -> Metadatum {
    if s.len() <= METADATUM_VALUE_MAX {
        return Metadatum::Text(s.to_string());
    }
    let chunks: Vec<Metadatum> = s
        .as_bytes()
        .chunks(METADATUM_VALUE_MAX)
        .map(|c| Metadatum::Text(String::from_utf8_lossy(c).into_owned()))
        .collect();
    Metadatum::Array(chunks)
}

fn encode_auxiliary_data(label: u64, datum: Metadatum) -> Result<Vec<u8>> {
    let mut metadata: BTreeMap<u64, Metadatum> = BTreeMap::new();
    metadata.insert(label, datum);
    let aux = AuxiliaryData::Shelley(metadata);
    minicbor::to_vec(&aux)
        .map_err(|e| BlockchainError::Internal(format!("metadata CBOR encode failed: {e}")))
}

fn parse_address(bech32: &str) -> Result<Address> {
    Address::from_bech32(bech32)
        .map_err(|e| BlockchainError::Internal(format!("invalid address {bech32}: {e}")))
}

/// Tx-body network id: 1 = mainnet, 0 = testnet (preprod/preview).
fn network_id_byte(network: &str) -> u8 {
    if network.eq_ignore_ascii_case("mainnet") {
        1
    } else {
        0
    }
}

fn ada_lovelace_or_int(v: &serde_json::Value) -> Result<u64> {
    // Ogmios v6: { "ada": { "lovelace": N } }. Older: a bare integer.
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    v.get("ada")
        .and_then(|a| a.get("lovelace"))
        .and_then(|l| l.as_u64())
        .ok_or_else(|| missing("ada.lovelace"))
}

fn missing(field: &str) -> BlockchainError {
    BlockchainError::Internal(format!("protocol parameters missing `{field}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use pallas_addresses::{Network, ShelleyAddress, ShelleyDelegationPart, ShelleyPaymentPart};

    fn utxo(hash_byte: u8, index: u64, lovelace: u64) -> Utxo {
        Utxo {
            tx_hash: hex::encode([hash_byte; 32]),
            index,
            lovelace,
        }
    }

    fn pparams() -> ProtocolParams {
        // Realistic mainnet-ish values: 44 lovelace/byte, 155381 flat.
        ProtocolParams {
            fee_coefficient_per_byte: 44,
            fee_constant: 155_381,
            max_tx_size: 16_384,
        }
    }

    /// A synthetic wallet whose `address` is a valid testnet enterprise address
    /// derived (deterministically) so build+sign exercises the real path.
    fn test_wallet() -> Wallet {
        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let key_hash: Hash<28> = Hash::new([0xab; 28]);
        let addr = ShelleyAddress::new(
            Network::Testnet,
            ShelleyPaymentPart::key_hash(key_hash),
            ShelleyDelegationPart::Null,
        )
        .to_bech32()
        .unwrap();
        Wallet {
            address: addr,
            signing_key,
            verifying_key,
        }
    }

    #[test]
    fn min_fee_is_constant_plus_coefficient_times_size() {
        let pp = pparams();
        assert_eq!(pp.min_fee(300), 155_381 + 44 * 300);
    }

    #[test]
    fn select_picks_largest_first_until_target() {
        let utxos = vec![utxo(1, 0, 5_000_000), utxo(2, 0, 2_000_000), utxo(3, 0, 8_000_000)];
        let chosen = select_largest_first(&utxos, 9_000_000).unwrap();
        // 8M then 5M reaches 13M ≥ 9M; the 2M is untouched.
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0].lovelace, 8_000_000);
        assert_eq!(chosen[1].lovelace, 5_000_000);
    }

    #[test]
    fn select_errors_when_insufficient() {
        let utxos = vec![utxo(1, 0, 1_000_000)];
        assert!(select_largest_first(&utxos, 5_000_000).is_err());
    }

    #[test]
    fn parse_utxos_keeps_ada_only_and_skips_token_utxos() {
        let entries = vec![
            serde_json::json!({
                "transaction": { "id": "aa".repeat(32) }, "index": 0,
                "value": { "ada": { "lovelace": 3_000_000u64 } }
            }),
            serde_json::json!({
                "transaction": { "id": "bb".repeat(32) }, "index": 1,
                "value": { "ada": { "lovelace": 9_000_000u64 }, "policyid": { "TOKEN": 1 } }
            }),
        ];
        let utxos = parse_utxos(&entries);
        assert_eq!(utxos.len(), 1, "the multi-asset UTxO is skipped");
        assert_eq!(utxos[0].lovelace, 3_000_000);
        assert_eq!(utxos[0].index, 0);
    }

    #[test]
    fn protocol_params_parse_ogmios_v6_shape() {
        let v = serde_json::json!({
            "minFeeCoefficient": 44,
            "minFeeConstant": { "ada": { "lovelace": 155_381u64 } },
            "maxTransactionSize": { "bytes": 16_384u64 }
        });
        let pp = ProtocolParams::from_ogmios(&v).unwrap();
        assert_eq!(pp.fee_coefficient_per_byte, 44);
        assert_eq!(pp.fee_constant, 155_381);
        assert_eq!(pp.max_tx_size, 16_384);
    }

    #[test]
    fn long_metadata_value_is_chunked_to_fit_64_bytes() {
        let long = "x".repeat(130);
        match text_value(&long) {
            Metadatum::Array(parts) => {
                assert_eq!(parts.len(), 3); // 64 + 64 + 2
            }
            other => panic!("expected chunked array, got {other:?}"),
        }
    }

    #[test]
    fn build_self_payment_with_metadata_balances_and_signs() {
        let wallet = test_wallet();
        let pp = pparams();
        // One fat UTxO funds a 1 ADA self-payment + change + fee.
        let utxos = vec![utxo(7, 0, 10_000_000)];
        let meta = text_record_metadatum(&[("t", "promise"), ("commitment", &"a".repeat(64))]);

        let signed = build_signed_payment(
            &wallet,
            "preprod",
            &utxos,
            &pp,
            &wallet.address,
            MIN_UTXO_LOVELACE,
            1000,
            Some((1694, meta)),
        )
        .unwrap();

        // tx id is a 32-byte blake2b hash, hex-encoded.
        assert_eq!(signed.tx_id.len(), 64);
        assert!(signed.cbor_hex.len() > 64, "signed cbor present");
        assert!(signed.fee > 0);
        assert_eq!(signed.num_inputs, 1);
        // Value conservation: inputs == payment + change + fee.
        let change = signed.change.unwrap();
        assert_eq!(MIN_UTXO_LOVELACE + change + signed.fee, 10_000_000);
        // The required fee must be covered.
        assert!(signed.fee >= pp.min_fee((signed.cbor_hex.len() / 2) as u64));
    }

    #[test]
    fn build_fails_on_insufficient_funds() {
        let wallet = test_wallet();
        let pp = pparams();
        let utxos = vec![utxo(7, 0, 500_000)]; // < 1 ADA, can't cover payment+fee
        let err = build_signed_payment(
            &wallet,
            "preprod",
            &utxos,
            &pp,
            &wallet.address,
            MIN_UTXO_LOVELACE,
            1000,
            None,
        );
        assert!(err.is_err());
    }
}

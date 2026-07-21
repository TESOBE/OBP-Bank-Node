//! Cardano wallet loading: everything derives from the signing key.
//!
//! `cardano-cli address key-gen` produces a `.skey` file — JSON-wrapped CBOR
//! of the 32-byte Ed25519 seed. That seed is the only secret and the only
//! input: the verification key is derived from it, and the enterprise
//! (payment-key-only, CIP-19 type 6) address is derived from the verification
//! key plus the network tag. The `.vkey` / `.addr` files cardano-cli also
//! emits are redundant and not read.

use std::path::Path;

use ed25519_dalek::{SigningKey, VerifyingKey};
use pallas_addresses::{Network, ShelleyAddress, ShelleyDelegationPart, ShelleyPaymentPart};
use pallas_crypto::hash::Hasher;
use serde::Deserialize;
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum WalletError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {message}")]
    Parse { path: String, message: String },
    #[error("unknown network '{0}' (expected mainnet, preprod, or preview)")]
    UnknownNetwork(String),
    #[error("address encoding failed: {0}")]
    AddressEncoding(String),
}

pub type Result<T> = std::result::Result<T, WalletError>;

#[derive(Debug, Deserialize)]
struct KeyEnvelope {
    #[serde(rename = "type")]
    key_type: String,
    #[allow(dead_code)]
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "cborHex")]
    cbor_hex: String,
}

#[derive(Debug)]
pub struct Wallet {
    pub address: String,
    pub signing_key: SigningKey,
    pub verifying_key: VerifyingKey,
}

impl Wallet {
    /// Load a wallet from a single `.skey` file. The verification key is
    /// derived from the seed and the enterprise address from the verification
    /// key + network, so no `.vkey` / `.addr` files are needed.
    pub fn load(skey_path: impl AsRef<Path>, network: &str) -> Result<Self> {
        let skey_bytes = load_key_envelope_payload(skey_path.as_ref(), Some("Signing"))?;

        let seed: [u8; 32] = skey_bytes.as_slice().try_into().map_err(|_| {
            WalletError::Parse {
                path: skey_path.as_ref().display().to_string(),
                message: format!("expected 32-byte seed, got {} bytes", skey_bytes.len()),
            }
        })?;
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        let address = enterprise_address(verifying_key.as_bytes(), network)?;

        debug!(address = %address, "wallet loaded");
        Ok(Wallet {
            address,
            signing_key,
            verifying_key,
        })
    }
}

/// Derive the bech32 enterprise address (CIP-19 type 6: payment key hash, no
/// staking part) for a payment verification key on the given network.
pub fn enterprise_address(vkey_bytes: &[u8; 32], network: &str) -> Result<String> {
    let net = match network {
        "mainnet" => Network::Mainnet,
        "preprod" | "preview" => Network::Testnet,
        other => return Err(WalletError::UnknownNetwork(other.to_string())),
    };
    let key_hash = Hasher::<224>::hash(vkey_bytes);
    ShelleyAddress::new(net, ShelleyPaymentPart::Key(key_hash), ShelleyDelegationPart::Null)
        .to_bech32()
        .map_err(|e| WalletError::AddressEncoding(e.to_string()))
}

fn load_key_envelope_payload(path: &Path, type_keyword: Option<&str>) -> Result<Vec<u8>> {
    let json = std::fs::read_to_string(path).map_err(|source| WalletError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let env: KeyEnvelope = serde_json::from_str(&json).map_err(|e| WalletError::Parse {
        path: path.display().to_string(),
        message: format!("not a valid key envelope: {e}"),
    })?;
    if let Some(want) = type_keyword {
        if !env.key_type.contains(want) {
            return Err(WalletError::Parse {
                path: path.display().to_string(),
                message: format!("expected type containing '{want}', got '{}'", env.key_type),
            });
        }
    }
    decode_cbor_byte_string(&env.cbor_hex).map_err(|message| WalletError::Parse {
        path: path.display().to_string(),
        message,
    })
}

/// Decode a CBOR-encoded byte string from its hex representation. The Shelley
/// key files use exactly one CBOR construct: a definite-length byte string
/// holding the raw key bytes. We accept any length here; callers enforce
/// the 32-byte expectation.
fn decode_cbor_byte_string(cbor_hex: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = hex::decode(cbor_hex).map_err(|e| format!("bad hex: {e}"))?;
    if bytes.is_empty() {
        return Err("empty CBOR payload".into());
    }
    // Major type 2 (byte string): top 3 bits == 010 (= 0x40..0x5F).
    let initial = bytes[0];
    if (initial & 0xe0) != 0x40 {
        return Err(format!(
            "CBOR major type 2 (byte string) expected, got first byte 0x{initial:02x}"
        ));
    }
    let info = initial & 0x1f;
    let (len, payload_start) = match info {
        0..=23 => (info as usize, 1),
        24 => {
            if bytes.len() < 2 {
                return Err("truncated 1-byte length".into());
            }
            (bytes[1] as usize, 2)
        }
        25 => {
            if bytes.len() < 3 {
                return Err("truncated 2-byte length".into());
            }
            (u16::from_be_bytes([bytes[1], bytes[2]]) as usize, 3)
        }
        _ => {
            return Err(format!(
                "unsupported CBOR length encoding (additional info {info})"
            ));
        }
    };
    if bytes.len() < payload_start + len {
        return Err(format!(
            "truncated payload: header says {len} bytes, only {} available",
            bytes.len() - payload_start
        ));
    }
    Ok(bytes[payload_start..payload_start + len].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use std::fs;
    use tempfile::tempdir;

    /// CIP-19 test-vector payment verification key
    /// (`addr_vk1w0l2sr2zgfm26ztc6nl9xy8ghsk5sh6ldwemlpmp9xylzy4dtf7st80zhd`).
    const CIP19_VKEY: [u8; 32] = [
        0x73, 0xfe, 0xa8, 0x0d, 0x42, 0x42, 0x76, 0xad, 0x09, 0x78, 0xd4, 0xfe, 0x53, 0x10, 0xe8,
        0xbc, 0x2d, 0x48, 0x5f, 0x5f, 0x6b, 0xb3, 0xbf, 0x87, 0x61, 0x29, 0x89, 0xf1, 0x12, 0xad,
        0x5a, 0x7d,
    ];

    fn cbor_byte_string_hex(bytes: &[u8]) -> String {
        let mut out = Vec::with_capacity(bytes.len() + 2);
        let len = bytes.len();
        if len < 24 {
            out.push(0x40 | (len as u8));
        } else if len < 256 {
            out.push(0x58);
            out.push(len as u8);
        } else {
            out.push(0x59);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        out.extend_from_slice(bytes);
        hex::encode(out)
    }

    fn write_envelope(path: &Path, key_type: &str, payload: &[u8]) {
        let json = serde_json::json!({
            "type": key_type,
            "description": "test",
            "cborHex": cbor_byte_string_hex(payload),
        });
        fs::write(path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    }

    #[test]
    fn decodes_short_form_byte_string() {
        let bytes = vec![1u8, 2, 3, 4, 5];
        let hex = cbor_byte_string_hex(&bytes);
        assert_eq!(decode_cbor_byte_string(&hex).unwrap(), bytes);
    }

    #[test]
    fn decodes_long_form_32_byte_string() {
        let bytes: Vec<u8> = (0..32).collect();
        let hex = cbor_byte_string_hex(&bytes);
        assert_eq!(decode_cbor_byte_string(&hex).unwrap(), bytes);
    }

    #[test]
    fn rejects_non_byte_string_cbor() {
        let err = decode_cbor_byte_string("8101").unwrap_err();
        assert!(err.contains("major type 2"), "got: {err}");
    }

    /// CIP-19 appendix test vectors for the type-6 (enterprise) address.
    #[test]
    fn derives_cip19_enterprise_addresses() {
        assert_eq!(
            enterprise_address(&CIP19_VKEY, "mainnet").unwrap(),
            "addr1vx2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzers66hrl8"
        );
        assert_eq!(
            enterprise_address(&CIP19_VKEY, "preprod").unwrap(),
            "addr_test1vz2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzerspjrlsz"
        );
        assert_eq!(
            enterprise_address(&CIP19_VKEY, "preview").unwrap(),
            "addr_test1vz2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzerspjrlsz"
        );
    }

    #[test]
    fn rejects_unknown_network() {
        assert!(matches!(
            enterprise_address(&CIP19_VKEY, "testnet"),
            Err(WalletError::UnknownNetwork(_))
        ));
    }

    #[test]
    fn loads_wallet_from_skey_alone() {
        let dir = tempdir().unwrap();
        let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());

        let skey = dir.path().join("test.skey");
        write_envelope(&skey, "PaymentSigningKeyShelley_ed25519", &signing.to_bytes());

        let w = Wallet::load(&skey, "preprod").expect("load ok");
        assert_eq!(w.verifying_key.to_bytes(), signing.verifying_key().to_bytes());
        assert!(w.address.starts_with("addr_test1v"), "got: {}", w.address);
        assert_eq!(
            w.address,
            enterprise_address(signing.verifying_key().as_bytes(), "preprod").unwrap()
        );
    }

    #[test]
    fn rejects_vkey_envelope_as_skey() {
        let dir = tempdir().unwrap();
        let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());

        let skey = dir.path().join("test.skey");
        write_envelope(
            &skey,
            "PaymentVerificationKeyShelley_ed25519",
            signing.verifying_key().as_bytes(),
        );

        assert!(matches!(
            Wallet::load(&skey, "preprod"),
            Err(WalletError::Parse { .. })
        ));
    }
}

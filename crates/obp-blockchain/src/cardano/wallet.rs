//! Cardano wallet loading: parse a `cardano-cli`-style key file trio.
//!
//! The `cardano-cli address key-gen` command produces three files:
//!   `<name>.skey`  — JSON-wrapped CBOR of the 32-byte Ed25519 seed (private)
//!   `<name>.vkey`  — JSON-wrapped CBOR of the 32-byte Ed25519 public key
//!   `<name>.addr`  — bech32 Shelley address string
//!
//! This module loads the trio, derives the SigningKey from the seed, and
//! optionally cross-checks against the .vkey for integrity.

use std::path::Path;

use ed25519_dalek::{SigningKey, VerifyingKey};
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
    #[error("integrity: signing key and verification key do not match")]
    IntegrityMismatch,
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
    /// Load a wallet from the .skey, .vkey, and .addr file trio. The vkey is
    /// used to validate that the skey we loaded actually produces the
    /// expected public key — a cheap integrity check.
    pub fn load(
        skey_path: impl AsRef<Path>,
        vkey_path: impl AsRef<Path>,
        addr_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let skey_bytes = load_key_envelope_payload(skey_path.as_ref(), Some("Signing"))?;
        let vkey_bytes = load_key_envelope_payload(vkey_path.as_ref(), Some("Verification"))?;
        let address = read_text(addr_path.as_ref())?;

        let seed: [u8; 32] = skey_bytes.as_slice().try_into().map_err(|_| {
            WalletError::Parse {
                path: skey_path.as_ref().display().to_string(),
                message: format!("expected 32-byte seed, got {} bytes", skey_bytes.len()),
            }
        })?;
        let signing_key = SigningKey::from_bytes(&seed);
        let derived_vk = signing_key.verifying_key();

        let expected_vk_bytes: [u8; 32] = vkey_bytes.as_slice().try_into().map_err(|_| {
            WalletError::Parse {
                path: vkey_path.as_ref().display().to_string(),
                message: format!("expected 32-byte public key, got {} bytes", vkey_bytes.len()),
            }
        })?;
        if derived_vk.to_bytes() != expected_vk_bytes {
            return Err(WalletError::IntegrityMismatch);
        }

        debug!(address = %address, "wallet loaded");
        Ok(Wallet {
            address,
            signing_key,
            verifying_key: derived_vk,
        })
    }
}

fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|source| WalletError::Io {
            path: path.display().to_string(),
            source,
        })
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

    #[test]
    fn loads_and_validates_a_real_keypair() {
        let dir = tempdir().unwrap();
        let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let verifying = signing.verifying_key();

        let skey = dir.path().join("test.skey");
        let vkey = dir.path().join("test.vkey");
        let addr = dir.path().join("test.addr");
        write_envelope(&skey, "PaymentSigningKeyShelley_ed25519", &signing.to_bytes());
        write_envelope(
            &vkey,
            "PaymentVerificationKeyShelley_ed25519",
            verifying.as_bytes(),
        );
        fs::write(&addr, "addr_test1abcd\n").unwrap();

        let w = Wallet::load(&skey, &vkey, &addr).expect("load ok");
        assert_eq!(w.address, "addr_test1abcd");
        assert_eq!(w.verifying_key.to_bytes(), verifying.to_bytes());
    }

    #[test]
    fn detects_skey_vkey_mismatch() {
        let dir = tempdir().unwrap();
        let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let other = SigningKey::from_bytes(&rand::random::<[u8; 32]>());

        let skey = dir.path().join("test.skey");
        let vkey = dir.path().join("test.vkey");
        let addr = dir.path().join("test.addr");
        write_envelope(&skey, "PaymentSigningKeyShelley_ed25519", &signing.to_bytes());
        write_envelope(
            &vkey,
            "PaymentVerificationKeyShelley_ed25519",
            other.verifying_key().as_bytes(),
        );
        fs::write(&addr, "addr_test1abcd\n").unwrap();

        assert!(matches!(
            Wallet::load(&skey, &vkey, &addr),
            Err(WalletError::IntegrityMismatch)
        ));
    }
}

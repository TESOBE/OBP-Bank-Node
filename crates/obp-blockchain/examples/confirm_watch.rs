//! Live end-to-end check of chain-sync confirmation depth.
//!
//! Drives the full production path against a real node: `CardanoBackend::new`
//! (spawns the chain-sync follower) → `write_promise` (registers the tx id
//! with the follower, then submits) → poll `confirm()` until the follower
//! reports the tx included and the depth has grown past 1, proving inclusion
//! detection and depth arithmetic against the live chain.
//!
//! This SUBMITS a real transaction (min-UTxO self-payment + fee). Testnet use
//! only.
//!
//! Usage:
//!   WALLET_SKEY=... \
//!   cargo run -p obp-blockchain --example confirm_watch [-- URL [NETWORK]]
//!
//! Defaults: URL = ws://localhost:1337, NETWORK = preprod.

use std::time::Duration;

use chrono::Utc;
use obp_blockchain::cardano::{CardanoBackend, CardanoConfig};
use obp_blockchain::{BlockchainBackend, ConfirmationStatus, PromiseRecord};

const POLL_EVERY: Duration = Duration::from_secs(15);
const GIVE_UP_AFTER: Duration = Duration::from_secs(600);
const TARGET_DEPTH: u32 = 2;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info,obp_blockchain=debug")
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let url = positional.first().map(|s| s.as_str()).unwrap_or("ws://localhost:1337");
    let network = positional.get(1).map(|s| s.as_str()).unwrap_or("preprod");

    let config = CardanoConfig {
        ogmios_url: url.to_string(),
        network: network.to_string(),
        wallet_skey_path: env_path("WALLET_SKEY").into(),
        query_timeout_secs: 90,
    };
    let backend = CardanoBackend::new(config).await.unwrap_or_else(|e| {
        eprintln!("ERROR building backend: {e}");
        std::process::exit(1);
    });

    let promise = PromiseRecord::commit_v1(b"confirm-watch-live-check", b"example-salt", Utc::now());
    let tx = backend.write_promise(&promise).await.unwrap_or_else(|e| {
        eprintln!("ERROR writing promise: {e}");
        std::process::exit(1);
    });
    println!("submitted promise commitment: tx {}", tx.tx_id);

    let deadline = tokio::time::Instant::now() + GIVE_UP_AFTER;
    loop {
        if tokio::time::Instant::now() >= deadline {
            eprintln!("gave up: depth {TARGET_DEPTH} not reached within {GIVE_UP_AFTER:?}");
            std::process::exit(1);
        }
        match backend.confirm(&tx).await {
            Ok(ConfirmationStatus::Confirmed { depth }) => {
                println!("confirm(): Confirmed {{ depth: {depth} }}");
                if depth >= TARGET_DEPTH {
                    println!("depth ≥ {TARGET_DEPTH} — follower depth reporting verified live");
                    return;
                }
            }
            Ok(other) => println!("confirm(): {other:?}"),
            Err(e) => println!("confirm() error (will retry): {e}"),
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

fn env_path(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("ERROR: set {key} to the wallet file path");
        std::process::exit(1);
    })
}

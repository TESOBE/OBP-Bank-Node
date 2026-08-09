//! Live end-to-end ADA settlement: the value leg, debtor → creditor.
//!
//! Drives the production wiring against a real node: `CardanoBackend::new`
//! (notary backend, spawns the chain-sync follower) →
//! `CardanoAdaSettlement::from_backend` (shares wallet, submit-lock, and
//! follower) → `settle()` with a fiat-denominated `SettlementInstruction`
//! (sized in lovelace at the stub FX rate) → poll `confirm()` to real depth →
//! show the funds at the creditor address.
//!
//! This SUBMITS a real value transfer. Testnet use only.
//!
//! Usage:
//!   WALLET_SKEY=... CREDITOR_ADDR=... \
//!   cargo run -p obp-blockchain --example settle_live [-- URL [NETWORK]]
//!
//! Defaults: URL = ws://localhost:1337, NETWORK = preprod.

use std::sync::Arc;
use std::time::Duration;

use obp_blockchain::cardano::ogmios::OgmiosClient;
use obp_blockchain::cardano::{CardanoAdaSettlement, CardanoBackend, CardanoConfig};
use obp_blockchain::settlement::{
    PartyRef, SettlementBackend, SettlementInstruction, StubFxSource,
};
use obp_blockchain::ConfirmationStatus;

const POLL_EVERY: Duration = Duration::from_secs(15);
const GIVE_UP_AFTER: Duration = Duration::from_secs(600);
const TARGET_DEPTH: u32 = 2;
/// Stub settle-time rate: 1 ADA = 35.42 KES (3542 minor units per whole ADA).
const RATE_MINOR_PER_ADA: u128 = 3542;
/// Net owed: 354.20 KES ⇒ exactly 10 ADA at the stub rate — comfortably above
/// the min-UTxO floor.
const NET_AMOUNT_MINOR: u128 = 35_420;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info,obp_blockchain=debug")
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let url = positional.first().map(|s| s.as_str()).unwrap_or("ws://localhost:1337");
    let network = positional.get(1).map(|s| s.as_str()).unwrap_or("preprod");

    let creditor_addr = std::env::var("CREDITOR_ADDR").unwrap_or_else(|_| {
        eprintln!("ERROR: set CREDITOR_ADDR to the creditor's bech32 address");
        std::process::exit(1);
    });

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
    let settlement =
        CardanoAdaSettlement::from_backend(&backend, Arc::new(StubFxSource::new(RATE_MINOR_PER_ADA)));

    let instruction = SettlementInstruction {
        snapshot_id: "settle-live-check".into(),
        debtor: PartyRef {
            bank_id: "bank-a".into(),
            account: settlement.payer_address().to_string(),
        },
        creditor: PartyRef {
            bank_id: "bank-b".into(),
            account: creditor_addr.clone(),
        },
        currency: "KES".into(),
        net_amount_minor: NET_AMOUNT_MINOR,
        idempotency_key: "settle-live-check-1".into(),
    };

    let outcome = settlement.settle(&instruction).await.unwrap_or_else(|e| {
        eprintln!("ERROR settling: {e}");
        std::process::exit(1);
    });
    // `asset_amount` is in the asset's smallest unit (lovelace for ADA).
    println!(
        "settled: tx {} — {} lovelace of {} moved (rate: {} minor/{})",
        outcome.tx.tx_id,
        outcome.asset_amount,
        outcome.asset,
        outcome.fx.as_ref().map(|q| q.minor_per_whole_asset).unwrap_or_default(),
        outcome.asset,
    );

    let deadline = tokio::time::Instant::now() + GIVE_UP_AFTER;
    loop {
        if tokio::time::Instant::now() >= deadline {
            eprintln!("gave up: depth {TARGET_DEPTH} not reached within {GIVE_UP_AFTER:?}");
            std::process::exit(1);
        }
        match settlement.confirm(&outcome.tx).await {
            Ok(ConfirmationStatus::Confirmed { depth }) => {
                println!("confirm(): Confirmed {{ depth: {depth} }}");
                if depth >= TARGET_DEPTH {
                    break;
                }
            }
            Ok(other) => println!("confirm(): {other:?}"),
            Err(e) => println!("confirm() error (will retry): {e}"),
        }
        tokio::time::sleep(POLL_EVERY).await;
    }

    // Show the value actually sitting at the creditor.
    let ogmios = OgmiosClient::new(url);
    match ogmios.utxos_at(&creditor_addr).await {
        Ok(utxos) => {
            println!("creditor {creditor_addr} now holds {} UTxO(s):", utxos.len());
            for u in utxos {
                let tx_id = u["transaction"]["id"].as_str().unwrap_or("?");
                let lovelace = u["value"]["ada"]["lovelace"].as_u64().unwrap_or(0);
                println!("  {tx_id} — {lovelace} lovelace");
            }
        }
        Err(e) => eprintln!("creditor UTxO query failed: {e}"),
    }
    println!("depth ≥ {TARGET_DEPTH} — live ADA settlement verified");
}

fn env_path(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("ERROR: set {key} to the wallet file path");
        std::process::exit(1);
    })
}

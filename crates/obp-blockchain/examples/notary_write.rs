//! Live preprod verification of the Phase-2 transaction builder.
//!
//! Builds (and optionally submits) a metadata-only notary self-payment — the
//! same path `CardanoBackend::write_promise` takes — against a real
//! `cardano-node` + Ogmios, using a funded preprod wallet on disk.
//!
//! Usage:
//!   WALLET_SKEY=/secrets/cardano.skey \
//!   cargo run -p obp-blockchain --example notary_write [-- URL [NETWORK]] [--submit]
//!
//! Defaults: URL = ws://localhost:1337, NETWORK = preprod.
//! Without `--submit` it is a DRY RUN — it builds and signs but does not
//! broadcast, so it never spends tADA. Pass `--submit` to actually send.

use chrono::Utc;
use obp_blockchain::cardano::ogmios::OgmiosClient;
use obp_blockchain::cardano::tx;
use obp_blockchain::cardano::wallet::Wallet;
use obp_blockchain::PromiseRecord;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let submit = args.iter().any(|a| a == "--submit");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let url = positional.first().map(|s| s.as_str()).unwrap_or("ws://localhost:1337");
    let network = positional.get(1).map(|s| s.as_str()).unwrap_or("preprod");

    let skey = env_path("WALLET_SKEY");
    let wallet = Wallet::load(&skey, network).unwrap_or_else(|e| {
        eprintln!("ERROR loading wallet: {e}");
        std::process::exit(1);
    });
    println!("wallet: {}", wallet.address);

    let ogmios = OgmiosClient::new(url);
    let tip = unwrap_or_exit("tip", ogmios.tip().await);
    println!("tip slot: {}", tip.slot);

    let pp_json = unwrap_or_exit("protocolParameters", ogmios.protocol_parameters().await);
    let pp = tx::ProtocolParams::from_ogmios(&pp_json).unwrap_or_else(|e| {
        eprintln!("ERROR parsing protocol params: {e}");
        std::process::exit(1);
    });
    println!(
        "params: fee = {} + {}/byte, max_tx = {} bytes",
        pp.fee_constant, pp.fee_coefficient_per_byte, pp.max_tx_size
    );

    let utxo_entries = unwrap_or_exit("utxo", ogmios.utxos_at(&wallet.address).await);
    let utxos = tx::parse_utxos(&utxo_entries);
    let total: u64 = utxos.iter().map(|u| u.lovelace).sum();
    println!("ada-only UTxOs: {} ({} lovelace total)", utxos.len(), total);
    if utxos.is_empty() {
        eprintln!("no spendable UTxOs — fund the wallet from the preprod faucet first");
        std::process::exit(1);
    }

    // A sample Promise commitment — hash only, exactly what reaches the chain.
    let promise = PromiseRecord::commit_v1(b"example-instruction", b"example-salt", Utc::now());
    let metadatum = tx::text_record_metadatum(&[
        ("schema", &promise.schema),
        ("commitment", &promise.commitment),
        ("ts", &Utc::now().to_rfc3339()),
    ]);

    let signed = tx::build_signed_payment(
        &wallet,
        network,
        &utxos,
        &pp,
        &wallet.address, // self-payment
        tx::MIN_UTXO_LOVELACE,
        tip.slot,
        Some((tx::OBP_METADATA_LABEL, metadatum)),
    )
    .unwrap_or_else(|e| {
        eprintln!("ERROR building tx: {e}");
        std::process::exit(1);
    });

    println!(
        "built tx {}\n  inputs={} fee={} change={:?} cbor_bytes={}",
        signed.tx_id,
        signed.num_inputs,
        signed.fee,
        signed.change,
        signed.cbor_hex.len() / 2
    );

    if !submit {
        println!("\nDRY RUN — not broadcasting. Re-run with --submit to send.");
        return;
    }

    println!("\nsubmitting…");
    match ogmios.submit_transaction(&signed.cbor_hex).await {
        Ok(id) if id.eq_ignore_ascii_case(&signed.tx_id) => {
            println!("accepted: {id}");
            println!("explorer: https://preprod.cardanoscan.io/transaction/{id}");
        }
        Ok(id) => eprintln!("MISMATCH: node returned {id}, we computed {}", signed.tx_id),
        Err(e) => eprintln!("submit failed: {e}"),
    }
}

fn env_path(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("ERROR: set {key} to the wallet file path");
        std::process::exit(1);
    })
}

fn unwrap_or_exit<T, E: std::fmt::Display>(what: &str, r: Result<T, E>) -> T {
    r.unwrap_or_else(|e| {
        eprintln!("ERROR querying {what}: {e}");
        std::process::exit(1);
    })
}

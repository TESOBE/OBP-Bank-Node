// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! Send test ADA from one Bank Node wallet to an address, for example to fund a new node's wallet when the faucet will not.
//!
//! By default this only builds and signs the transaction and shows what it would send, with the fee. It submits only when given `--yes`.
//! It uses the same transaction builder the nodes settle with (`tx::build_signed_payment`), through Ogmios.
//!
//! Testnet use only: it refuses to run on mainnet.
//!
//! Usage:
//!   WALLET_SKEY=secrets/cardano.skey TO_ADDR=addr_test1... ADA=100 \
//!   cargo run -p obp-blockchain --example send_ada [-- URL [NETWORK]] [-- --yes]
//!
//! Defaults: URL = ws://localhost:1337, NETWORK = preprod.

// The Ogmios client: talks JSON-RPC to the Cardano node over a WebSocket.
use obp_blockchain::cardano::ogmios::OgmiosClient;
// The transaction helpers: protocol parameters, UTxO parsing and the payment builder.
use obp_blockchain::cardano::tx;
// A wallet loaded from a .skey file, with the address derived from it.
use obp_blockchain::cardano::wallet::Wallet;
// Duration, for the Ogmios call timeout.
use std::time::Duration;

// Ogmios calls that read the UTxO set can be slow on testnets, so allow well over the 15 second default.
const OGMIOS_CALL_TIMEOUT: Duration = Duration::from_secs(120);

// The entry point. `#[tokio::main]` starts the async runtime the Ogmios client needs.
#[tokio::main]
// main returns a Result, so `?` can pass any error up, and the program prints it and exits with a failure code.
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Collect the command-line arguments, without the program's own name.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--yes` means: really submit. Without it, this is a dry run.
    let submit = args.iter().any(|arg| arg == "--yes");
    // The arguments that are not flags: the Ogmios URL, then the network.
    let positional: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    // The Ogmios URL, defaulting to the local Cardano container.
    let ogmios_url = positional.first().map(|s| s.as_str()).unwrap_or("ws://localhost:1337");
    // The network, defaulting to preprod, the network the dev nodes use.
    let network = positional.get(1).map(|s| s.as_str()).unwrap_or("preprod");

    // Refuse mainnet: this tool is for moving test ADA only.
    if network == "mainnet" {
        // Return an error, which main prints.
        return Err("send_ada is for testnets only and will not run on mainnet".into());
    }

    // The path of the paying wallet's signing key, from WALLET_SKEY.
    let wallet_skey = required_env("WALLET_SKEY")?;
    // The address to pay, from TO_ADDR.
    let to_address = required_env("TO_ADDR")?;
    // The amount in whole ADA, from ADA, as text.
    let ada_text = required_env("ADA")?;
    // Parse the amount as a whole number of ADA, with a clear message if it is not one.
    let ada: u64 = ada_text.parse().map_err(|_| format!("ADA must be a whole number of ADA, got {ada_text:?}"))?;
    // Convert ADA to lovelace (1 ADA = 1,000,000 lovelace), refusing an amount so large it overflows.
    let lovelace = ada.checked_mul(tx::LOVELACE_PER_ADA).ok_or("ADA is too large")?;

    // Load the paying wallet. Its address is derived from the key and the network.
    let wallet = Wallet::load(&wallet_skey, network)?;
    // Refuse to pay a wallet to itself, which would only cost a fee.
    if wallet.address == to_address {
        // Return an error, which main prints.
        return Err("TO_ADDR is the paying wallet's own address".into());
    }

    // Connect to Ogmios, with the longer timeout.
    let ogmios = OgmiosClient::with_timeout(ogmios_url, OGMIOS_CALL_TIMEOUT);
    // Read the current protocol parameters, which the fee and minimum output size depend on.
    let protocol_params_json = ogmios.protocol_parameters().await?;
    // Turn them into the builder's ProtocolParams.
    let protocol_params = tx::ProtocolParams::from_ogmios(&protocol_params_json)?;
    // Read the chain tip, which sets the transaction's validity window.
    let tip = ogmios.tip().await?;
    // Read the paying wallet's unspent outputs (UTxOs).
    let utxo_entries = ogmios.utxos_at(&wallet.address).await?;
    // Parse them into the builder's Utxo values (ADA-only outputs).
    let utxos = tx::parse_utxos(&utxo_entries);
    // Add up the wallet's spendable balance, for the summary.
    let balance: u64 = utxos.iter().map(|utxo| utxo.lovelace).sum();

    // Build and sign the payment, with no metadata. This chooses the inputs, works out the fee and returns the change to the payer.
    let signed = tx::build_signed_payment(&wallet, network, &utxos, &protocol_params, &to_address, lovelace, tip.slot, None)?;

    // Show what the transaction does.
    println!("Network:  {network} (Ogmios {ogmios_url})");
    // The payer and its balance before the payment.
    println!("From:     {} (balance {})", wallet.address, format_ada(balance));
    // The payee.
    println!("To:       {to_address}");
    // The amount sent.
    println!("Amount:   {}", format_ada(lovelace));
    // The network fee, paid by the payer on top of the amount.
    println!("Fee:      {}", format_ada(signed.fee));
    // The transaction id, known before submitting because it is the hash of the transaction body.
    println!("Tx id:    {}", signed.tx_id);

    // Without --yes, stop here.
    if !submit {
        // Explain how to really send it.
        println!("\nDry run: nothing was sent. Run again with --yes to submit.");
        // Finish successfully.
        return Ok(());
    }

    // Submit the signed transaction through Ogmios. The node answers with the transaction id.
    let submitted_id = ogmios.submit_transaction(&signed.cbor_hex).await?;
    // Report the submission.
    println!("\nSubmitted: {submitted_id}");
    // Point to an explorer, where the payment shows once it is in a block (usually within a minute).
    println!("See it at: https://{network}.cardanoscan.io/transaction/{submitted_id}");
    // Finish successfully.
    Ok(())
}

// Read an environment variable that must be set and not empty, or return an error naming it.
fn required_env(name: &str) -> Result<String, String> {
    // Look up the variable.
    match std::env::var(name) {
        // Set and not empty: return its value.
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        // Missing or empty: say which one, and what the usage is.
        _ => Err(format!("set {name} (see the usage at the top of examples/send_ada.rs)")),
    }
}

// Show an amount of lovelace as ADA with six decimal places, for example "100.000000 tADA".
fn format_ada(lovelace: u64) -> String {
    // Whole ADA, then the remaining lovelace as the six decimal places.
    format!("{}.{:06} tADA", lovelace / tx::LOVELACE_PER_ADA, lovelace % tx::LOVELACE_PER_ADA)
}

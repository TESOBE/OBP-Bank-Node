//! Live smoke test of the Ogmios JSON-RPC client.
//!
//! Usage:
//!   cargo run -p obp-blockchain --example ogmios_smoke [-- URL [ADDRESS]]
//!
//! Defaults:
//!   URL     = ws://localhost:1337
//!   ADDRESS = (unset; UTxO query skipped)

use obp_blockchain::cardano::ogmios::OgmiosClient;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "ws://localhost:1337".to_string());
    let address = args.next();

    let client = OgmiosClient::new(&url);

    println!("== queryNetwork/tip @ {url}");
    match client.tip().await {
        Ok(t) => println!(
            "   slot={}  height={:?}  id={}",
            t.slot, t.height, t.id
        ),
        Err(e) => {
            eprintln!("   ERROR: {e}");
            std::process::exit(1);
        }
    }

    println!("\n== queryLedgerState/protocolParameters");
    match client.protocol_parameters().await {
        Ok(v) => {
            let summary: Vec<_> = v
                .as_object()
                .map(|o| o.keys().take(8).cloned().collect())
                .unwrap_or_default();
            println!("   keys (first 8): {summary:?}");
        }
        Err(e) => eprintln!("   ERROR: {e}"),
    }

    if let Some(addr) = address {
        println!("\n== queryLedgerState/utxo for {addr}");
        match client.utxos_at(&addr).await {
            Ok(utxos) => {
                println!("   {} UTxO(s)", utxos.len());
                for (i, u) in utxos.iter().enumerate().take(5) {
                    let tx_id = u
                        .get("transaction")
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let index = u.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                    let value = u.get("value").map(|v| v.to_string()).unwrap_or_default();
                    println!("   [{i}] {tx_id}#{index}  value={value}");
                }
            }
            Err(e) => eprintln!("   ERROR: {e}"),
        }
    } else {
        println!("\n== (skipping UTxO query — pass an address as 2nd arg to include)");
    }
}

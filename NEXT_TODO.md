# NEXT TODO

## Cardano integration

Right now Cardano is **stubbed** — `internal/cardano/blockfrost.go` returns deterministic hashes from `sha256(record JSON)` and never opens a network connection. The config knobs (`network`, `blockfrost_api_key`, `wallet_address`, `signing_key_path`) are read but unused.

So "running a Cardano testnet" is two separate questions:

### 1. Switch the Cardano network (config-only, easy)

Once the writer is real, switching from `mainnet` to a testnet is just:

```yaml
cardano:
  network: "preview"             # or "preprod"
  blockfrost_api_key: "preview..." # Blockfrost project key for that network
  wallet_address: "addr_test1q..." # tADA wallet (note "addr_test")
  signing_key_path: "./secrets/cardano.skey"
```

Three Cardano testnets to choose from:
- **preview** — ~2s blocks, latest features, what you'd use day-to-day
- **preprod** — mirrors mainnet config, what you'd use to validate just before mainnet
- **local devnet** — your own private chain via `cardano-node`, fully reproducible but lots of setup

For OBP Bank Node dev, **preview** is almost always the right choice. tADA is free from the [testnet faucet](https://docs.cardano.org/cardano-testnets/tools/faucet).

### 2. Make the writer actually do something (implementation work)

That's the bigger lift. The skeleton has the interface (`internal/cardano.Writer` with `WritePromise` / `WriteSettlementReference` / `WriteException`). To make it real:

**Option A — Blockfrost-only.** Build the Cardano transaction CBOR client-side, sign with the bank's `signing_key_path`, submit via Blockfrost's `/tx/submit` endpoint. Pros: no local node needed. Cons: building CBOR transactions in Go is fiddly — there's `go-cardano-serialization` and a few others, varying maturity.

**Option B — `cardano-cli` subprocess.** Use the official tool to build/sign metadata transactions, then submit via Blockfrost (or a local node). Ugly but battle-tested. Good for a v0.1 because the OC records are essentially metadata-only transactions, which `cardano-cli` handles in a few commands.

**Option C — Local `cardano-node` + Ogmios/Kupo.** Run cardano-node yourself, plus a JSON-RPC layer (Ogmios) and indexer (Kupo). More moving parts but no Blockfrost dependency.

### Recommendation

If you want to see Promise records on chain quickly:

1. Sign up for **Blockfrost** on the **preview** testnet — free tier, takes 2 minutes
2. Generate a test wallet (`cardano-cli` or a wallet app), fund it from the faucet
3. Update the config to point at preview + put the Blockfrost key in
4. Implement the writer using **Option B** (`cardano-cli` subprocess) for v0.1 — you'll have on-chain Promise records in a day, and you can replace it with a pure-Go signer later without touching any callers

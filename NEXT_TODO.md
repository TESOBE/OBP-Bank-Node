# NEXT TODO

The original content of this file (a Blockfrost-based Cardano writer plan)
is superseded.

The current Cardano direction lives in [`ARCHITECTURE.md`](ARCHITECTURE.md):
local `cardano-node` on preprod, Ogmios JSON-RPC over WebSocket, Rust
`CardanoConnector` behind the `BlockchainConnector` trait.

## Status

- **Phase 1 — foundation: done.** Ogmios client, wallet loading
  (`.skey` / `.vkey` / `.addr`), real `confirm()` against the chain.
- **Phase 2 — write path: outstanding.** Build, sign, and submit metadata-only
  transactions for Promise / Settlement Reference / Exception records via
  `pallas` (or `cardano-multiplatform-lib`); promote the Ogmios client to a
  persistent multiplexed connection for chain-sync subscriptions.

Once Phase 2 lands, this file can be deleted.

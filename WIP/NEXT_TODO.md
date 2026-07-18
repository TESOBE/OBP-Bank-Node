# NEXT TODO

The original content of this file (a Blockfrost-based Cardano writer plan)
is superseded.

The current Cardano direction lives in [`../DOCS/ARCHITECTURE.md`](../DOCS/ARCHITECTURE.md):
local `cardano-node` on preprod, Ogmios JSON-RPC over WebSocket, Rust
`CardanoBackend` behind the `BlockchainBackend` trait.

## Status

- **Phase 1 — foundation: done.** Ogmios client, wallet loading
  (`.skey` / `.vkey` / `.addr`), real `confirm()` against the chain.
- **Phase 2 — write path: built, offline-verified.** Build/sign/submit of
  metadata-only notary txs and ADA value transfers via `pallas`
  (`cardano/tx.rs`), and the chain-sync follower (`cardano/follower.rs`,
  2026-07-14): a persistent Ogmios connection driving `findIntersection` /
  `nextBlock`, giving both `confirm()` impls real depth + rollback handling.
  What remains before this file can be deleted is the **live preprod run**
  (`notary_write --submit` + follower against a synced node) — see
  `PHASE2_TX_BUILDER_TODO.md`.

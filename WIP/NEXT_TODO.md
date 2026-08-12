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

## 2026-08-12 — Platform revenue & charges (analysis + plan)

How the platform makes money, where banks are charged, and what is displayed.
Three fee layers; only one exists in code, and it is recorded but neither
collected nor displayed.

### What exists today

- **Per-promise charge (OBP-API, live, invisible).** Every
  `OPEN_CORRIDOR_PROMISE` gets a charge computed from the props key
  `transactionRequests_charge_level_OPEN_CORRIDOR_PROMISE` (default
  `0.0001` = 1 basis point) and stored on the TR next to the body's
  `charge_policy` (`SHARED`/`SENDER`/`RECEIVER`). Verified in the dev DB:
  promises carry e.g. `charge 2.0 KES`; settlement TRs correctly `0.00`.
  Netting sums promise VALUES only, so no fee money ever moves; the node
  discards the charge block from OBP-API's 201 response; the App shows
  nothing.
- **Bank→customer margin.** The banks' own pricing toward their customers
  (product fees + their customer FX margin). Their revenue, not the
  platform's; `charge_policy` is their tool.
- **Rail costs.** ADA tx fees, paid by the debtor bank's wallet, visible
  only on-chain.

### Revenue plan (staged)

1. **Meter and invoice (first, zero money-path changes).** The OBP-API hub
   durably records every promise, credit notification, and settlement it
   brokers — that is the billing feed. Charge member banks basis points on
   settled promise value plus a corridor-membership / hosted-node fee,
   invoiced monthly from those records. This is how existing schemes
   (SWIFT, card networks) bill members: fees do not ride inside payments.
2. **On-ledger fee accrual (later, if wanted).** Extend the pair-settle so
   each netting cycle also accrues the platform's bps into a fee position
   per bank, settled as its own periodic leg. Auditable in the same
   ledger; real build effort.
3. **Rejected: FX spread capture at settlement.** Would contradict the
   per-settlement rate transparency (rate + source + timestamp now
   persisted and displayed); worth more with banks than the spread.

### Display TODO (small, ~an afternoon)

- [ ] Node: keep the `charge` block from OBP-API's 201 submit response
      (currently only the TR id is parsed), store it on the outbox row,
      surface it on the south-side TR endpoints.
- [ ] App: show the charge in the Promise tables — `charge 2.00 KES ·
      SHARED`.
- [ ] App: show the configured charge level in the Send form hint so the
      pricing is visible before submitting.

Result: every transfer is metered and priced from the first promise;
collection is an invoicing choice, not missing plumbing.

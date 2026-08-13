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
2. **Fees collected in ADA via a dedicated periodic fee settlement
   (chosen design, 2026-08-12, Simon).** Basis: bps × promise value,
   accrued per bank in the ledger, due when the promise is covered by a
   netting cycle. Collection is DECOUPLED from the corridor's settlement
   rail: a periodic (monthly / threshold) fee-settlement instruction per
   bank over the existing `obp_settlement_instruction` machinery,
   `settlement_system: cardano-ada`, creditor = the platform's Cardano
   address, KES→ADA at the settle-time rate (persisted + displayed like
   any settlement). Rationale:
   - Corridor settlement is meant to be rail-optional (stablecoin /
     traditional rail = new `SettlementBackend` impl + routing scheme);
     fees must not depend on the rail choice.
   - Every bank node already runs a funded ADA wallet for PROMISE
     commitments regardless of settlement rail — ADA fee collection adds
     zero new requirements, and aligns platform and banks on the same
     asset ("good enough for the platform ⇒ good enough for the banks").
   - Piggybacking fees as an extra output on ADA settlement txs stays
     available later as an optimization for ADA-settled corridors only.

   Fee policy (2026-08-12, Simon):
   - **Originator pays.** Fees are owed by the bank that ORIGINATES a
     promise; a creditor-only bank owes nothing per-transaction, by
     design. (Bank B doing work for bank A's customer is an argument for
     an interchange-style A→B component someday — the ledger can compute
     it — not for B paying the platform.)
   - **Returns are fee-exempt.** Promises with `return_of` set are
     involuntary corridor housekeeping originated by the beneficiary
     bank; they accrue no platform fee. The accrual query must exclude
     them.
   - The flat corridor-membership fee (invoiced) is what creditor-heavy
     banks contribute; both sides benefit from reachability.
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

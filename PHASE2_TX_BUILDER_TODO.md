# Phase 2 — Cardano Transaction Builder

## Status (2026-06-14)

**Done — the builder and both write paths are wired (offline-verified):**

- `pallas` 1.1 deps added (`pallas-txbuilder/primitives/crypto/codec/addresses`);
  API confirmed against vendored 1.1.0 source before coding.
- **`crates/obp-blockchain/src/cardano/tx.rs`** — the shared builder:
  `ProtocolParams::from_ogmios`, `parse_utxos`, `select_largest_first`,
  iterative fee/change convergence, `build_signed_payment` (Conway CBOR assemble
  + blake2b-256 id + Ed25519 witness), `text_record_metadatum`. Coin-selection
  and fee math are pure-tested; the full build+sign path is tested end-to-end
  against synthetic UTxOs + wallet (no node).
- **Notary writes** — `CardanoBackend::write_{promise,settlement,exception}` now
  build/sign/submit min-UTxO self-payments carrying the record. **Hash-only:**
  only a commitment reaches the chain, never cleartext (Promise carries its
  salted commitment; Settlement/Exception are SHA-256'd in `commit_record`).
- **ADA settlement** — `CardanoAdaSettlement::submit_ada_transfer` is wired to a
  real debtor→creditor value tx; `idempotency_key` folded into metadata.
- **Per-wallet submission serialization** — `submit_lock` on `CardanoBackend`,
  shared into the settlement backend via `from_backend`.
- **Live harness** — `examples/notary_write.rs` (dry-run by default, `--submit`
  to broadcast) for verifying against a real preprod node + funded wallet.

**Remaining:**

- **Live preprod integration** — run `notary_write --submit` against a synced
  preprod node with a faucet-funded wallet; confirm the tx lands. (Offline build
  is verified; on-chain acceptance is not yet.) Then a debtor→creditor transfer.
- **Persistent Ogmios + chain-sync `confirm()`** — still the coarse
  `utxos_at`-presence check; real depth + rollbacks pending (see below).
- **Salt for Settlement/Exception commitments** — currently unsalted; plumb a
  per-record salt like the Promise path before production.
- **Dust-floor + funding policy** — decisions (see table).

**Decision made while wiring:** all three notary records are **hash-only**
on-chain, extending the Promise privacy decision to Records 4 & 5 (no cleartext
amounts/reasons on an immutable public ledger). Flag if public on-chain
settlement amounts are wanted instead.

---

Build, sign, and submit real Cardano transactions, unblocking **both** write
paths that are currently stubbed:

1. **Notary writes** — metadata-only txs for Promise / Settlement Reference /
   Exception records.
   `crates/obp-blockchain/src/cardano/mod.rs` → `CardanoBackend::write_{promise,settlement,exception}`
2. **ADA settlement** — value txs that move lovelace debtor → creditor.
   `crates/obp-blockchain/src/cardano/settlement.rs` → `CardanoAdaSettlement::submit_ada_transfer`

Both reduce to the same primitive: *select inputs → build outputs → compute fee
→ hash → sign → submit*. Build it once as a shared module; the two call sites
differ only in their outputs/metadata.

## Status going in

- **Read side is ready.** `OgmiosClient` already has `protocol_parameters()`,
  `utxos_at()`, `submit_transaction(cbor_hex)`, and `tip()`.
- **Wallet is ready.** `Wallet` exposes `signing_key` (Ed25519), `verifying_key`,
  and the bech32 `address`.
- **Off-chain sizing is done.** `CardanoAdaSettlement::settle()` already does the
  debtor guard, settle-time FX, and `fiat_minor_to_lovelace()`. Only the chain
  write is missing.

## Library

Use **`pallas`** (pure Rust, by TxPipe) — `pallas-txbuilder` for assembly,
`pallas-primitives` / `pallas-crypto` for CBOR + blake2b-256 + the Ed25519
witness. Avoids the WASM-binding friction of cardano-serialization-lib.

Caveat to verify early: `pallas-txbuilder`'s fee/coin-selection maturity. If it
won't do automatic fee+change, we do it manually (steps 3–5) — not hard for
ADA-only txs.

## The shared builder

New module `crates/obp-blockchain/src/cardano/tx.rs`:

```rust
pub struct TxPlan { /* selected inputs, outputs, fee, ttl, metadata */ }

/// Pure-ish planning: coin selection + fee + change. Testable without a node
/// given a UTxO set and protocol params.
pub fn plan_payment(utxos, pparams, to, lovelace, change_addr, metadata) -> Result<TxPlan>;

/// Assemble CBOR body, hash (blake2b-256) → tx_id, attach the Ed25519 witness
/// from the wallet, return (tx_id, signed_cbor_hex).
pub fn build_signed(plan, wallet, network) -> Result<(String, String)>;
```

- Notary write = `plan_payment(.., to = self, lovelace = min-utxo, metadata = record)` —
  a self-payment carrying the record in tx metadata.
- ADA settlement = `plan_payment(.., to = creditor, lovelace = sized_amount, metadata = idempotency tag)`.

Then `submit_transaction(cbor)` and assert the returned id matches the computed one.

## Mechanics (the parts to get right)

1. **Protocol params** — pull `txFeeFixed`, `txFeePerByte`, `utxoCostPerByte`
   (min-UTxO), `maxTxSize` from `protocol_parameters()`. Cache per cycle.
2. **UTxO set** — `utxos_at(payer_address)`. PoC settlement wallets hold
   **ADA-only** UTxOs (no native tokens) — keeps selection simple.
3. **Coin selection** — accumulate largest-first until `inputs ≥ output + fee +
   min-utxo change`. Document the algorithm; it's basic for ADA-only.
4. **Outputs + change** — output to recipient; change back to payer. Change must
   be `≥ min-UTxO` or be folded into the fee (no dust outputs).
5. **Fee** — `fee = txFeeFixed + txFeePerByte × size`. Circular (fee→change→
   size→fee): build with a fee estimate, measure CBOR size, recompute, rebuild
   once. One iteration converges for these simple txs.
6. **TTL** — set `invalid_hereafter = tip.slot + N` so a stuck tx expires rather
   than lingering. Pull slot from `tip()`.
7. **Hash + sign** — blake2b-256 over the tx body = tx id; Ed25519 sign that
   with `wallet.signing_key`; wrap as a `VKeyWitness` (vkey + sig).
8. **Network magic** — preprod vs mainnet from `self.network`; needed for the
   address network tag and submission.
9. **Submit** — `submit_transaction(cbor_hex)`; verify returned id == computed id.

## Idempotency & concurrency (do not skip)

- **Idempotency** — fold `idempotency_key` into tx metadata. On retry, the caller
  (or the existing outbox) checks `confirm()` for the prior tx id before
  resubmitting, so a Settlement settles **at most once**.
- **UTxO contention** — two concurrent settlements from one wallet can select the
  *same* UTxO and one will be rejected as a double-spend. Serialize submission
  per wallet (a per-address async mutex) or reserve selected UTxOs in the outbox
  until confirmed. The node already has the outbox pattern to lean on.
- No collateral needed — these are pure payments, no Plutus scripts.

## Confirmation upgrade (shared by both backends)

Replace the coarse `utxos_at`-presence check in both `confirm()` impls with a
**persistent chain-sync follower** (Ogmios `nextBlock` over a held connection)
that reports real depth and handles rollbacks. Promote `OgmiosClient` from
connect-per-call to one multiplexed connection (already flagged in
`crates/obp-blockchain/src/cardano/ogmios.rs` Phase-1 note).

## Edge cases / decisions

| Question | Why it matters |
|---|---|
| Dust floor for ADA settlement | A net below ~1 ADA (min-UTxO) can't be a standalone output. Accumulate across cycles, skip, or set a threshold — policy decision. |
| Fragmented payer UTxOs | Many small UTxOs may need multi-input selection; cap inputs to stay under `maxTxSize`. |
| Metadata size limit | Notary records must fit Cardano's per-tx metadata limits; PromiseRecord is just a hash, so fine — keep it that way. |
| Wallet funding / top-up | A persistently net-paying bank drains ADA; needs a funding/float process (ties to the working-ADA-float model in the settlement design). |

## Testing

- **Unit** — `plan_payment` coin selection + fee/change math against fixed UTxO
  sets and protocol params (deterministic, no node). Extend the existing
  `fiat_minor_to_lovelace` tests.
- **Integration** — against a real **preprod** node (extend
  `crates/obp-blockchain/examples/ogmios_smoke.rs`): fund a preprod faucet wallet,
  submit a tiny self-payment with metadata, confirm it lands. Then a debtor→
  creditor ADA transfer end-to-end.

## Work list

| Item | Where | Notes |
|---|---|---|
| Add `pallas` deps | `crates/obp-blockchain/Cargo.toml` | txbuilder + primitives + crypto |
| `cardano/tx.rs` shared builder | new file | `plan_payment` + `build_signed` |
| Manual fee/coin-selection (if pallas insufficient) | `cardano/tx.rs` | ADA-only, largest-first |
| Wire `CardanoAdaSettlement::submit_ada_transfer` | `cardano/settlement.rs` | replace the Phase-2 stub |
| Wire `CardanoBackend::write_{promise,settlement,exception}` | `cardano/mod.rs` | metadata self-payments |
| Per-wallet submission serialization | `cardano/` | async mutex or UTxO reservation |
| Persistent Ogmios + chain-sync `confirm()` | `cardano/ogmios.rs` + both `confirm()` | real depth + rollbacks |
| Preprod integration test | `examples/` + `tests/` | self-payment, then transfer |
| Dust-floor + funding policy | docs / config | decisions, not just code |

Once Phase 2 lands, this file and `NEXT_TODO.md` can both be deleted.

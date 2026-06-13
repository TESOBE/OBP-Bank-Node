# NEXT STEPS — pickup notes

State of the project as of the pause point, so we can resume without re-litigating
decisions.

## Where we are

The Rust rewrite is early. What exists and builds today
(`cargo run -p obp-bank-node`):

- **Cargo workspace, two crates** — `obp-bank-node` (binary) and
  `obp-blockchain` (backend trait + impls). See `ARCHITECTURE.md`.
- **Config loading (figment)** — `server` / `bank` / `blockchain` blocks,
  layered YAML (`obp-bank-node-config.yaml`) + `OBP_BN_` env overrides. Boots
  with placeholder defaults if no config file is present. (Config is currently
  an inline struct in `main.rs`; a dedicated `config.rs` module is planned.)
- **South-side REST API (Interface A)** — axum, port 8088. Routes live:
  `POST /obp-bank-node/v5.1.0/transaction-requests`,
  `GET .../transaction-requests/{id}`, `GET .../transaction-requests`, and
  `/health` (+ versioned alias). **All handlers are Phase 1 stubs**: they mint a
  UUID, log, and return a hardcoded `202` / `INITIATED` shape — no outbox, no
  OBP-API call, no chain write. (Note: the `initiate_payment` stub still emits
  the old `COUNTERPARTY` / `counterparty_id` response — it has not been migrated
  to the `OPEN_CORRIDOR` + inline-routing + `originator` body in `A1_A2.md`. The
  *request* type does already parse snake_case routing fields.)
- **Blockchain connector (Interface D), partial** — `BlockchainBackend` trait,
  `MockBackend`, and a `CardanoBackend` whose `new()` (Ogmios client + wallet
  loading) and `confirm()` work. `write_promise` / `write_settlement` /
  `write_exception` return "not yet implemented" — that is Phase 2 (see
  `NEXT_TODO.md`).

**Not built yet in Rust** (existed only in the deleted Go skeleton):

- **AMQP / RabbitMQ consumer (Interface C)** — no `lapin` dependency yet; the
  entire inbound-message path is absent.
- **OBP API client (Interface B)** — no `reqwest` client yet.
- **Outbox (durability)** — no `sqlx` / SQLite persistence yet.
- **CBS delivery modes (A2)** — none of webhook-OBP / webhook-ISO20022 /
  database / file-drop are implemented yet.

So the binary boots, serves the REST surface with stubbed responses, and can
reach a Cardano node for `confirm()` — but no payment yet flows end-to-end. The
Go skeleton's "working end-to-end" state (RabbitMQ round-trip, functional
Postgres delivery) did **not** carry over; that code was deleted on the switch
to Rust.

## The design docs in this repo

Read in this order if returning cold:

1. **`TLDR.md`** — what the OBP Bank Node is and does. Whole-system summary in
   one read. Start here.
2. **`LEDGER_DESIGN.md`** — how OBP-API will track OC netting via full
   double-entry. The *what*. Decided destination.
3. **`OBP_API_CHANGES.md`** — concrete changes inside the OBP-API codebase to
   implement `LEDGER_DESIGN.md`. The *where*, with file references and
   ordering. **This is where to start when picking up OBP-API work.**
4. **`CERT_TODO.md`** — X.509 / mTLS auth for self-service onboarding. Decided
   direction (cert-based, not password). The *what* for provisioning.
5. **`PROVISIONING_API.md`** — concrete changes inside OBP-API to implement
   `CERT_TODO.md`. The *where* for provisioning, with file references and
   ordering. Companion to `CERT_TODO.md`, same shape as `OBP_API_CHANGES.md`.
6. **`NEXT_TODO.md`** — Cardano writer status: Phase 1 done (Ogmios client +
   wallet loading + `confirm()`); Phase 2 (tx build/sign/submit via `pallas`)
   outstanding.

## Decisions committed (don't re-open without reason)

- **Project naming:** `OBP-Bank-Node`. Path-prefix `/obp-bank-node/v5.X.X/`.
  Default port 8088. No "OCN" abbreviation; no "OC" abbreviation; "Open
  Corridor" spelled out everywhere.
- **Wire format on Interface C:** OBP RPC pattern. Single shared queue
  `obp_rpc_queue` (configurable as `request_queue`). Dispatch by AMQP
  `MessageId`. Reply via OBP inbound envelope to `replyTo`. MessageIds:
  `obp_credit_notification`, `obp_netting_snapshot`,
  `obp_settlement_instruction`, `obp_status_update`.
- **Multi-tenant topology:** per-bank vhost. Each bank gets its own RabbitMQ
  vhost (`/bank.{bank_id}`) with its own credentials. Permission isolation
  enforced at the broker level. (Discussed; not yet wired in code.)
- **Auth long-term:** X.509 client certificates via TESOBE-run CA. Password
  mode is interim only.
- **OBP-API ledger model:** full double-entry. Each Promise = an OBP
  Transaction in the new `OPEN_CORRIDOR_PROMISE` `transactionType`. Settlement
  = an OBP Transaction in `OPEN_CORRIDOR_SETTLEMENT` that posts the net.
  Snapshot is a new entity grouping them.
- **Status field reuse:** the existing `MappedTransaction.status` column
  carries the Open Corridor lifecycle values (`PROMISED`, `NETTED`, `SETTLED`,
  `EXCEPTION`, `REVERSED`). No new column. BG code is **not** touched because
  Open Corridor settlement accounts (`kind = OPEN_CORRIDOR_SETTLEMENT`) are
  inter-bank ledger entries, not customer-facing accounts, so the BG filter
  never sees them.
- **Account / Transaction kind reuse:** existing `MappedBankAccount.kind` and
  `MappedTransaction.transactionType` carry the Open Corridor values.
  Genuinely new columns on `MappedTransaction`: `snapshot_id`,
  `parent_settlement_id`, `cardano_tx_hash`. Three new tables:
  `open_corridor_snapshots`, `open_corridor_settlement_policies`,
  `open_corridor_outbox`.
- **Tracer-bullet philosophy:** the *flow* may be a thin slice (one corridor,
  one currency, manual triggers, etc.) but **OBP-API code stays
  production-grade regardless**. Schema migrations, state machines, audit
  metadata, View permissions, message-doc updates — all required, not
  optional. (This is saved as a Claude memory; will persist across sessions.)

## Open questions / things we discussed but didn't finalise

- **Cycle period per currency.** Time-based? Volume-based? On-demand for v1?
- **Pre-funding vs credit lines.** Settlement accounts must be positive at
  PROMISE creation? Or allowed to go negative within a credit limit?
- **Bilateral vs multilateral netting.** Both fit the schema; need to pick.
- **Cross-currency / FX.** Same-currency netting only for v1, or include FX?
  FX needs an additional engine.
- **Settlement system per currency.** Cardano for KES? CHAPS for GBP? NIBSS
  for NGN? Manual-only initially?
- **Reversal flow.** Authority, window, regulator constraints.
- **CA backend choice.** `PROVISIONING_API.md` defaults to Vault PKI but lists
  step-ca / openssl-dev / mocked. Operator decision before §3 of that doc can
  be implemented for production.

## Plausible next steps (pick whichever makes sense)

In rough order of how big a commitment each is:

**Small / clarifying:**
- Pick answers to the open policy questions above so the OBP-API
  implementation can be properly specced.
- Pick the CA backend (Vault PKI vs step-ca) so `PROVISIONING_API.md` §3
  can be implemented against a concrete target.

**Medium / incremental Bank Node work:**
- Phase 2 of the Rust `CardanoBackend`: build, sign, and submit
  metadata-only transactions for Promise / Settlement Reference / Exception
  records via `pallas` against the local preprod node (already syncing in
  Docker — see `docker/README.md`). Funded preprod wallet already on disk
  (1000 tADA).
- Build the RabbitMQ consumer (Interface C) in Rust with `lapin` — the inbound
  message path (`obp_rpc_queue`, dispatch by `MessageId`, OBP inbound-envelope
  reply). TLS / `amqps://` is a follow-on once the broker is mTLS-enabled (see
  `CERT_TODO.md`).
- Build the SQLite outbox (`sqlx`) for durability, then the replay loop
  (Section 11 resilience). Neither exists in the Rust port yet.
- Build the OBP API client (Interface B) with `reqwest` + OAuth2 and wire
  `initiate_payment` to persist → submit → write Promise (replacing the stub),
  migrating the response body to the `OPEN_CORRIDOR` shape on the way.

**Large / OBP-API ledger work:**
- Start `OBP_API_CHANGES.md` step 1: schema migrations (additive, nullable —
  ships immediately, no behaviour change).
- Then step 2: `OpenCorridorConstants` + `OpenCorridorStatus` modules + unit
  tests.
- Then steps 3–6: Mappers → ApiRoles → `createOpenCorridorPromise` connector
  method → TR flow extension. This gets you Promise creation working
  end-to-end on the OBP-API side.

## Quick re-orientation

To build and run the Bank Node:

```bash
cd /home/simonredfern/Documents/workspace_2024/OBP-Bank-Node
cargo run -p obp-bank-node      # binds 0.0.0.0:8088
```

No external services are required to boot: with no config file it uses
placeholder `bank` values and the `mock` blockchain connector. To exercise the
real `CardanoBackend`, set `blockchain.type: cardano` in
`obp-bank-node-config.yaml` and run a local `cardano-node` + Ogmios (see
`docker/README.md`).

To check it is up:

```bash
curl -s http://localhost:8088/health | jq
# { "status": "healthy", "service": "OBP-Bank-Node", "version": "...",
#   "blockchain": "mock" | "cardano", "timestamp": "..." }
```

The REST handlers are stubs — `POST /transaction-requests` returns a `202` with
a minted `transaction_request_id` but does not yet persist or forward anything.

## Memory / Claude session

- The "OBP-API code stays robust even in tracer-bullet work" feedback rule is
  saved as a persistent Claude memory; future sessions in this project will
  apply it automatically.
- All design docs (TLDR, NEXT_TODO, CERT_TODO, LEDGER_DESIGN,
  OBP_API_CHANGES, PROVISIONING_API) are committed-style — they reflect the
  latest state of the discussion as of pause time.

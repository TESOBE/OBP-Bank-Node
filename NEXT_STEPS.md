# NEXT STEPS — pickup notes

State of the project as of the pause point, so we can resume without re-litigating
decisions.

## Where we are

End-to-end OBP Bank Node skeleton is **working** locally:

- Built and run with `./start.sh` (port 8088 by default)
- South-side REST API live on `/obp-bank-node/v5.X.X/...`
- RabbitMQ consumer (real `amqp091-go`) connected to local broker on `obp_rpc_queue`,
  dispatching by AMQP `MessageId`, replying via OBP inbound-envelope to `replyTo`
- All four CBS delivery modes implemented; **Postgres mode is fully functional**
  (auto-creates schema, INSERT works, poll loop picks up CBS-PROCESSED rows and
  marks delivered in the local outbox)
- `/health` reports live RabbitMQ state; preflight banner prints on startup
- OBP API client, Cardano writer remain stubbed (deliberate — see `NEXT_TODO.md`,
  `CERT_TODO.md`)

Last live smoke test: round-tripped a fake `obp_credit_notification` through
RabbitMQ → handler → Postgres INSERT → simulated CBS update → poll loop →
outbox MarkCreditDelivered. Reply envelope correlation IDs round-tripped cleanly.

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
- Phase 2 of the Rust `CardanoConnector`: build, sign, and submit
  metadata-only transactions for Promise / Settlement Reference / Exception
  records via `pallas` against the local preprod node (already syncing in
  Docker — see `docker/README.md`). Funded preprod wallet already on disk
  (1000 tADA).
- Add TLS support to the RabbitMQ consumer (config block + `amqps://` dial
  path) so we can later test against an mTLS-enabled broker.
- Add an outbox replay loop on the Bank Node side (Section 11 resilience —
  currently we persist but don't replay).

**Large / OBP-API ledger work:**
- Start `OBP_API_CHANGES.md` step 1: schema migrations (additive, nullable —
  ships immediately, no behaviour change).
- Then step 2: `OpenCorridorConstants` + `OpenCorridorStatus` modules + unit
  tests.
- Then steps 3–6: Mappers → ApiRoles → `createOpenCorridorPromise` connector
  method → TR flow extension. This gets you Promise creation working
  end-to-end on the OBP-API side.

## Quick re-orientation

To get the Bank Node back up:

```bash
cd /home/simonredfern/Documents/workspace_2024/OBP-Bank-Node
./start.sh                  # builds, runs, prints preflight banner
```

Pre-reqs: local RabbitMQ container running on 5672/15672 (guest/guest).
Optional: local OBP-API on 8080 (preflight will mark it reachable).

To verify everything is wired:

```bash
curl -s http://localhost:8088/health | jq
# rabbitmq should be "connected"; obp_api/cardano show "stub"
```

To test the Postgres delivery mode end-to-end, see the smoke-test commands
in the chat history (or just glance at `internal/delivery/database.go`'s
implementation).

## Memory / Claude session

- The "OBP-API code stays robust even in tracer-bullet work" feedback rule is
  saved as a persistent Claude memory; future sessions in this project will
  apply it automatically.
- All design docs (TLDR, NEXT_TODO, CERT_TODO, LEDGER_DESIGN,
  OBP_API_CHANGES, PROVISIONING_API) are committed-style — they reflect the
  latest state of the discussion as of pause time.

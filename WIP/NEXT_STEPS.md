# NEXT STEPS — pickup notes

State of the project as of the pause point, so we can resume without re-litigating
decisions.

## 2026-08-07 (later) — resume checklist executed; `/app` UI crate built

- **Checklist step 1 done**: `Http4s700RoutesTest` re-run post-rename —
  **137/137**. (A first run failed 1 test, `putBankSupportedRoutingScheme`
  404-scenario got 400 — an endpoint yesterday's commit never touched; an
  immediate re-run passed. Mechanism: that 404 relies on
  `RoutingScheme.find` returning `Empty`; a transient hiccup against the
  persistent Postgres test DB yields a `Failure` box, which
  `unboxFullOrFail`/`ErrorResponseConverter` degrade to 400. Known flake
  now, not a regression.)
- **Checklist step 2 done**: `obp-api.jar` rebuilt (JDK 17,
  2026-08-07 16:32) — v7.0.0 classes contain `other_bank_id`, zero
  `counterparty_bank_id`.
- **Checklist step 3**: OBP-API side committed by Simon (`7154d070`
  "OC related"); the Bank Node working tree remains for Simon to review.
- **Checklist step 4 done — `obp-bank-node-app` built** (see
  `APP.md` §Shape for the full description): new workspace crate with
  whitelisted JSON proxy + embedded single-page UI covering the five
  storyline steps; per-node `bearer_token` auth seam. Node-side
  addition: `TransactionRequestStatus` gained `value` / `other_bank_id`
  / `description` (projected from the stored A1.1 payload) so the
  position view can net without consulting OBP-API. **148 workspace
  tests pass** (was 138). Smoke-tested live against a mock-mode node
  through the proxy (initiate → list). Not yet exercised against the
  full roundtrip stack — that's the natural next step, plus the
  bring-up/health-check script `APP.md` §Honest-demo-caveats asks for.

## 2026-08-07 — settle endpoint reshaped to a resource; TR-attribute migration; demo-app spec

**All OBP-API changes below are UNCOMMITTED working-tree edits in
`~/Documents/workspace_2024/OBP-API-Simon/OBP-API` (on `develop`, HEAD
`1201ac795`), together with the earlier `MappedText` fix in
`TransactionRequestAttribute.scala`. Http4s700RoutesTest passed 137/137 on
2026-08-07 — but that was BEFORE the `other_bank_id` rename (block 5 below);
the post-rename re-run was cut off by shutdown. See the resume checklist at
the end of this block for the exact next actions.**

**Build environment finding:** OBP-API must be built with **JDK 17**.
The system default (21) crashes scalac (Scala 2.12: "object java.lang.Object
in compiler mirror not found"); JDK 11 fails on the pom's `-release 17`.
Simon's own builds already use 17.

**1. DB migration for `transactionrequestattribute.value` (varchar(255) → text).**
Schemifier never alters existing columns, so the earlier hand-`ALTER` on the
live sandbox DB needed a real migration. New
`code/api/util/migration/MigrationOfTransactionRequestAttributeValueType.scala`
(`ALTER ... TYPE text`, SQL Server `VARCHAR(MAX)`; modeled on
`MigrationOfConsumer`'s `aud` migration), registered in `Migration.scala`
(`alterTransactionRequestAttributeValueType`, `runOnce`-tracked, called after
`alterCounterpartyLimitFieldType`). No-op on the already-altered sandbox DB
and on fresh DBs (schemifier creates text from `MappedText`); fixes any other
pre-existing DB at boot with `migration_scripts.enabled=true`.

**2. Settle endpoint reworked to a settlement resource (decided with Simon;
rationale: the old POST /open-corridor/settle name oversold — it nets and
*initiates*; value moves later on the rail).**

- `POST /obp/v7.0.0/banks/BANK_ID/open-corridor/settlements`, body
  `{other_bank_id, currency}` (was flat `/open-corridor/settle` with
  `{bank_id_a, bank_id_b, currency}`; the body field was briefly
  `counterparty_bank_id`, renamed 2026-08-07 with Simon — OBP's idiom for
  the far side is `other_*`, and "counterparty" wrongly suggests a real
  OBP Counterparty entity). URL bank = one side of the pair.
  Endpoint val renamed `createOpenCorridorSettlement` (was
  `settleOpenCorridorPair`); doc states explicitly that 201 ≠ value moved.
- **`CanSettleOpenCorridor` is now bank-scoped** (`requiresBankId = true` in
  `ApiRole.scala`) and checked by the middleware at the URL's BANK_ID — a
  bank/node can only settle corridors it is party to. Either side may
  trigger; debtor/creditor fall out of the net's sign (`net_amount` stays
  absolute — direction is carried by the debtor/creditor fields; Simon's
  initiator/other naming idea was discussed and dropped as it needs a signed
  amount + convention and clashes with the Interface C wire contract).
- **New `GET /obp/v7.0.0/banks/BANK_ID/open-corridor/settlements/SETTLEMENT_ID`**
  (404 `OBP-40058 OpenCorridorSettlementNotFound` for unknown ids AND for
  banks that are not a party — existence not disclosed). Separates
  `ledger_status` (TR B, COMPLETED at settle time) from `settlement_status`,
  read off the settlement-instruction outbox row's recorded node reply:
  `NET_ZERO` / `INSTRUCTED` (no reply yet) / node-reported `SETTLING` /
  `SUBMITTED` (+ `settlement_depth`) / `FINAL` (row DELIVERED) / `ERROR`
  (row STICKY). Also lists all outbox messages with delivery state.
  Implementation: `OpenCorridorSettlement.getSettlementStatus` (+ covered
  promises via `settled_by_transaction_request_id` attribute query).
- Files touched: `ApiRole.scala`, `ErrorMessages.scala` (OBP-40058),
  `JSONFactory7.0.0.scala` (`PostOpenCorridorSettlementJsonV700`,
  `OpenCorridorSettlementStatusJsonV700`,
  `OpenCorridorSettlementMessageJsonV700`), `Http4s700.scala` (both routes +
  ResourceDocs), `OpenCorridorSettlement.scala`,
  `Http4s700RoutesTest.scala` (bank-scoped grants, new paths, new scenario
  "role is bank-scoped" 403, GET assertions: INSTRUCTED + creditor-side read
  + unknown-id 404 + NET_ZERO).

**3. Bank Node repo consequences (this repo):**

- `WIP/roundtrip/run_roundtrip.sh` — settle step now POSTs
  `banks/rt.bank.a/open-corridor/settlements` with
  `{other_bank_id, currency}`.
- `WIP/roundtrip/setup_obp.sh` — `CanSettleOpenCorridor` granted per-bank
  (both banks), no longer system-level.
- `WIP/OPEN_CORRIDOR_INTERFACE_C_PUBLISH_PLAN.md` §5.3 updated (resource
  shape noted as superseding the flat shape).
- **`WIP/APP.md` (new)** — spec for the `/app` demo/manual-test UI decided
  with Simon: lives in this repo, one axum crate (`obp-bank-node-app`),
  talks ONLY to Bank Node APIs (node A + node B; position view = each
  node's own outbound legs joined by the app). Requires four node API
  additions: settlement-store + evidence-store read endpoints, the
  settlement-linkage fix, and a node settle-request endpoint that calls the
  new OBP-API settlements resource over Interface B (node's M2M user needs
  the bank-scoped `CanSettleOpenCorridor` at its bank).

**4. Node API additions for `/app` — built (2026-08-07, later the same day).**
All four `WIP/APP.md` items, in `crates/obp-bank-node` (138 workspace tests
pass, was 120):

- **Settlement read API**: `GET .../settlements` +
  `GET .../settlements/{key}` (key = `idempotency_key` OR `settlement_id`;
  `SettlementStore::find/list`). Settlement + evidence stores now open
  unconditionally in `main` (reads work in mock mode / consumer off);
  finality watcher spawn still requires a rail.
- **Evidence read API**: `GET .../evidence[/{transaction_request_id}]`,
  incl. the new CBS delivery result — `cbs_status`/`cbs_reference`/
  `cbs_recorded_at` columns (guarded ALTER), recorded by the credit handler
  on DELIVERED and FAILED.
- **Settlement linkage**: outbox `settlement_id`/`settled_at` columns
  (guarded ALTER); `mark_settled(covered_obp_tr_ids, settlement_id)` stamps
  by matching `obp_transaction_request_id` (idempotent — keeps first
  `settled_at`; empty list no-op). Stamped from the settle trigger and from
  the corridor proxy (covers the non-triggering node).
  `GET .../transaction-requests/{id}` surfaces both.
- **Settle trigger**: `POST /obp-bank-node/v5.1.0/settlements`
  `{other_bank_id, currency}` → `ObpClient::create_settlement`
  (POST to OBP's bank-scoped settlements resource) + linkage stamp; 201
  relays OBP's body + `covered_outbox_rows_stamped`. Corridor view:
  `GET .../settlements/{id}/corridor` → `ObpClient::get_settlement`
  proxy. New `classify_interactive_failure`: any 4xx with an OBP code
  passes through verbatim (403/404 included — the caller is an app, not
  the dispatcher); transport → 502 `OBP-BANK-NODE-INTERFACE-B-001`.
- `setup_obp.sh`: node service users now get bank-scoped
  `CanSettleOpenCorridor` at their own bank (both sides may trigger).
- Next for `/app`: the `obp-bank-node-app` axum crate itself (static UI +
  JSON proxy over the two nodes) per `WIP/APP.md` §Shape.

**5. Field rename `counterparty_bank_id` → `other_bank_id` (2026-08-07,
last thing before shutdown — one verification still outstanding, see the
checklist).** Decided with Simon: OBP's idiom for the far side is `other_*`
(`other_bank_routing_scheme`, …), and "counterparty" wrongly suggests a real
OBP Counterparty entity, which this is not. Renamed everywhere — both repos
were uncommitted, so no wire compatibility concern:

- **OBP-API**: `PostOpenCorridorSettlementJsonV700.other_bank_id`
  (`JSONFactory7.0.0.scala`), endpoint validation + error text ("the other
  bank must differ from BANK_ID") + ResourceDoc example and prose
  (`Http4s700.scala`), test file incl. its `otherBankId` helper
  (`Http4s700RoutesTest.scala`). Zero `counterparty` left in those files.
- **Bank Node**: `SettleRequest.other_bank_id` (`rest/types.rs`),
  `ObpClient::create_settlement` param + wire body, handler validation
  messages + log fields, REST tests, `run_roundtrip.sh` settle body.
  Deliberately untouched: commit–reveal "shared with the counterparty"
  prose in `obp-blockchain` (plain English, predates this) and the
  `types.rs` comment about OBP-API counterparty creation (a real OBP
  Counterparty reference).
- Rust workspace re-verified after the rename: **138 tests pass**.

**Resume checklist (in order):**

1. ~~Run the Http4s700RoutesTest suite~~ — done for the pre-rename tree
   (137/137, after adding the missing `OpenCorridorSettlementNotFound`
   import). **BUT: a re-run to verify the `other_bank_id` rename on the
   Scala side was still in flight when the machine shut down — treat it as
   NOT verified. First action on resume:**

   ```bash
   cd ~/Documents/workspace_2024/OBP-API-Simon/OBP-API
   JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64 \
     mvn -pl obp-api test -DwildcardSuites=code.api.v7_0_0.Http4s700RoutesTest
   ```

   The rename was mechanical (sed over DTO/endpoint/test, grep-verified
   zero leftovers), so surprises are unlikely — but run it.
2. **Rebuild the OBP-API jar — the current `obp-api/target/obp-api.jar`
   (built 2026-08-07 14:35) is STALE: it predates the rename and still
   speaks `counterparty_bank_id`.** After step 1 is green:
   `JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64 mvn clean package -DskipTests`.
   (That build otherwise did its job — MappedText fix + `open_corridor_enabled`
   prop are baked; the `jar uf` hack and live-DB-only ALTER are obsolete.)
3. Simon reviews + commits the OBP-API working tree (migration + MappedText
   + settle rework + test-import fix + rename) and the Bank Node edits
   (node API additions, rename, WIP docs, scripts).
4. Then the `/app` work per `WIP/APP.md` — node API additions **done**
   (block 4 above); the `obp-bank-node-app` crate (static UI + JSON proxy
   over node A/B) is what remains.

## 2026-07-31 — FULL ROUND TRIP ACHIEVED on localhost + Cardano preprod

The end-to-end Open Corridor loop ran for real — two KES 500 payments from
`rt.bank.a`, promises on-chain, settle-pair netting (KES 1000), credit
notifications verified and CBS-posted at node B, and a 47.125-tADA
settlement transfer FINAL at depth 2, confirmed back to OBP-API's outbox
relay (row DELIVERED). Full evidence: `WIP/roundtrip/STATUS.md`.

Defects fixed on the way (details in STATUS.md "Fixes applied"):
node submit-URL had a spurious `/views/` segment (`obp_client.rs` +
dispatcher/client test fixtures); OBP's
`transactionrequestattribute.value` was varchar(255) → now `MappedText`
in OBP-API source + live-DB ALTER (**jar rebuild pending** to bake the
source fix); EUR↔KES FX rates and authuser email-validation are now part
of `setup_obp.sh`.

New follow-ups from the run:

- **FX for corridor currencies**: CoinGecko no longer supports KES as a
  vs_currency (ADA/KES returns `{}`). Node A ran on the fixed stub rate.
  Need a fiat cross-rate (ADA/USD × USD/KES from a web2 FX source) or the
  API3 path pulled forward.
- **Settlement linkage on the node status API**:
  `GET .../transaction-requests/{id}` has `settlement_id`/`settled_at`
  fields but nothing populates them — the pair-level settlement instruction
  is never joined back to the covered outbox rows.
- **Rebuild the OBP-API jar** (and re-inject `open_corridor_enabled` is no
  longer needed — the prop is in `default.props`; a plain rebuild picks up
  both) at the next natural OBP-API stop.

## Where we are

The Rust rewrite is early. What exists and builds today
(`cargo run -p obp-bank-node`):

- **Cargo workspace, two crates** — `obp-bank-node` (binary) and
  `obp-blockchain` (backend trait + impls). See `../DOCS/ARCHITECTURE.md`.
- **Config loading (figment)** — `server` / `bank` / `blockchain` blocks,
  layered YAML (`obp-bank-node-config.yaml`) + `OBP_BN_` env overrides. Boots
  with placeholder defaults if no config file is present. (Config is currently
  an inline struct in `main.rs`; a dedicated `config.rs` module is planned.)
- **South-side REST API (Interface A)** — axum, port 8088. Routes live:
  `POST /obp-bank-node/v5.1.0/transaction-requests`,
  `GET .../transaction-requests/{id}`, `GET .../transaction-requests`, and
  `/health` (+ versioned alias). The side-effect path is now wired (see the
  2026-06-13 outbound update below): `initiate_payment` validates, persists to
  the outbox, and returns `202` / `INITIATED`, with the dispatcher driving the
  OBP-API submit and chain write asynchronously. `initiate_payment` now
  emits the `OPEN_CORRIDOR` + inline-routing + `originator` response body from
  `../DOCS/A1_A2.md` (the old `COUNTERPARTY` / `counterparty_id` shape is gone) and
  performs synchronous request validation (steps 2–3 of the A1.1 table):
  malformed JSON / missing fields → `OBP-10001`, zero/negative amount →
  `OBP-40008`, empty `originator` fields → `OBP-BANK-NODE-ORIGINATOR-001`.
  Currency → settlement-account resolution (`422 OBP-BANK-NODE-ROUTING-001`)
  is deferred until the OBP API client + per-currency `bank` config land.
- **Blockchain connector (Interface D)** — `BlockchainBackend` trait,
  `MockBackend`, and a `CardanoBackend` with `new()` (Ogmios client + wallet
  loading), `confirm()`, and the `write_promise` / `write_settlement` /
  `write_exception` notary writes all implemented: each builds, signs, and
  submits a metadata-only self-payment via `tx::build_signed_payment` + Ogmios
  (`NEXT_TODO.md` Phase 2). The remaining `confirm()` limitation is depth — it
  reports presence via a UTxO lookup, not confirmation depth; a chain-sync
  follower is the follow-on.

**Outbound payment path — now built (2026-06-13):**

- **Outbox (durability)** — `outbox.rs`, `sqlx` + SQLite. Lifecycle
  `INITIATED → SUBMITTED → PROMISE_WRITTEN` / `EXCEPTION`, per-request salt
  column, RFC3339 timestamps, backoff-aware `claim_due`. `initiate_payment` now
  persists the request and returns `202` *before* any external call (the 202 is
  durable); the handler is no longer a stub. `GET`/`list` read the outbox.
- **OBP API client (Interface B)** — `obp_client.rs`, `reqwest`. Submits the
  `OPEN_CORRIDOR` TR to `/obp/v7.0.0/...`. Error split: a 400/422 with an
  `OBP-NNNNN` business code is terminal (`EXCEPTION`); 5xx/timeout/429/auth/404
  are retryable (a misconfig must not fail a real payment). Auth is abstracted
  (`ObpAuth`): OBP-API's OAuth2 client-credentials (M2M) grant — token fetch +
  cache with refresh-before-expiry — or a DirectLogin token, selected from
  config. With no credentials it runs unauthenticated against a local/mock
  OBP-API.
- **Dispatcher** — `dispatcher.rs`, background tokio task. Drains the outbox:
  submit to OBP → write the Cardano Promise **commitment** (hash-only, see the
  privacy decision below) → advance status, with backoff on transport failure.
  Verified end-to-end live against a stub OBP-API + `MockBackend`:
  POST → `INITIATED` → (tick) → `PROMISE_WRITTEN` with a SHA-256 `promise_id`.

  **Privacy / dispute model (decided 2026-06-13):** the Promise puts **only a
  salted SHA-256 commitment** on-chain, never cleartext amount/currency/PII. The
  chain is a non-repudiation anchor for inter-bank disputes (commit–reveal);
  authorship comes from Bank A's wallet signature, not the hash. Open follow-up:
  the salt must travel to the counterparty (Interface C) for the reveal to work.

**Inbound path — now built (2026-06-14):**

- **Interface C consumer (`lapin`)** — connects to the bank's RabbitMQ vhost,
  consumes `obp_rpc_queue`, dispatches by `MessageId`, replies with the OBP
  inbound-envelope. Off by default (`rabbitmq.enabled=false`); the message
  *logic* (`interface_c::Router`) is transport-free and unit-tested without a
  broker. Handlers: `credit_notification` (full), `settlement_instruction`
  (seam — chain-settle trigger pending), `netting_snapshot` / `status_update`
  (record/log), unknown → `OBP-BANK-NODE-NOT-IMPLEMENTED`.
- **Salt delivery + evidence store** — `obp_credit_notification` carries the
  evidence triplet (`promise_commitment`, `promise_salt`, `promise_preimage`).
  The handler recomputes `SHA-256(salt ‖ preimage)` via the shared
  `PromiseRecord::verify_v1` and **refuses to credit on a mismatch**
  (`OBP-BANK-NODE-COMMITMENT-MISMATCH`). Verified or not, it lands in a durable
  `evidence` SQLite store — so Bank B holds the salt + preimage independently of
  Bank A and can run the commit–reveal proof.
- **A2 CBS delivery (`webhook_obp`)** — `cbs::CbsClient` posts the credit to the
  bank's CBS (bearer = `local_secret`), expecting `{status, cbs_reference}`.
- **Settlement wiring** — the `settlement_instruction` handler now maps the
  instruction (this node = debtor, `debtor.account` from `settles_from()`) and
  calls `CardanoAdaSettlement::settle`, which does the FX sizing + real ADA
  tx build/sign/submit. `main` builds the settlement backend on Cardano (sharing
  the notary's wallet/Ogmios/submit-lock); mock mode has none. Failures →
  `OBP-BANK-NODE-SETTLEMENT-FAILED`; no rail → `…-SETTLEMENT-NOT-CONFIGURED`.

**Salted Settlement/Exception commitments + simple-netting design (2026-07-14):**

- **All three notary record types are now salted.** Previously only the Promise
  commitment was salted; Settlement and Exception records were SHA-256'd
  unsalted inside `CardanoBackend` (`commit_record`, now deleted), leaving
  low-entropy cleartext (amount, currency, short reasons) brute-forceable from
  the on-chain hash. Changes, all in `crates/obp-blockchain`:
  - The commitment scheme — hex SHA-256 over `salt ‖ canonical_bytes` — is a
    single pair of crate-level functions in `lib.rs`
    (`compute_commitment` / `verify_commitment`); `PromiseRecord`'s methods
    delegate to them.
  - `SettlementRecord` and `ExceptionRecord` carry their schema tags
    (`obp.settlement.v1` / `obp.exception.v1`, moved out of `cardano/mod.rs`)
    plus `canonical_bytes()`, `commit_v1(salt)`, and
    `verify_v1(salt, commitment)` — the same commit–reveal shape as the
    Promise. Canonical bytes = the record's JSON in struct declaration order;
    any struct change is a schema change (bump `SCHEMA_V1`).
  - **Trait change:** `BlockchainBackend::write_settlement` /
    `write_exception` now take `salt: &[u8]`. The contract: the caller mints
    the salt per record and persists it durably (as the Promise path does via
    the outbox) *before* calling — record + salt is what gets revealed in a
    dispute. There are no production callers of these two methods yet, so no
    node-side plumbing was needed; the signature forces future callers to do
    it right.
  - `MockBackend` updated; tests moved to `lib.rs` and extended (salt changes
    the commitment, verify rejects wrong salt/record, amounts don't leak).
    77 workspace tests pass.

- **`OPEN_CORRIDOR_SIMPLE_NETTING.md` (new, in this repo)** — the
  deliberately-minimal netting design: **bilateral, settle-on-demand**, built
  on OBP's existing Transaction Request / Transaction model. Promise = an
  `OPEN_CORRIDOR` TransactionRequest held at `PENDING`; settle step =
  `SUM(A→B) − SUM(B→A)`, one posted Transaction for the net, linked back via
  `transaction_ids`, TRs set `COMPLETED`. No snapshot table, no scheduler, no
  multilateral, no FX. **Note:** for this PoC slice it supersedes the
  `../DOCS/LEDGER_DESIGN.md` approach of modelling the promise as a non-posting
  Transaction with `transactionType = OPEN_CORRIDOR_PROMISE` — the TR-based
  shape needs no new column and no special non-posting transactions. Both
  work items it defines (hold the TR at `PENDING` in `OpenCorridorProcessor`;
  the admin settle-pair endpoint) live in **OBP-API (Scala)**, currently being
  worked on by a separate session — coordinate before touching OBP-API.

- **README.md** gained a plain-language TL;DR of the five record types
  (Promise / Netting Snapshot / Settlement / Exception / Reversal) with an
  honest current-state note: Promise, Settlement, Exception are built;
  Netting Snapshot and Reversal are designed only. `../DOCS/BANK_INTEGRATION.md` /
  `../DOCS/BANK_INTEGRATION_ESSENTIAL.md` got matching small edits.

- **Chain-sync `confirm()` with real depth — built (later the same day,
  offline-verified).** New `crates/obp-blockchain/src/cardano/follower.rs` +
  `OgmiosSession` in `ogmios.rs`:
  - `OgmiosSession` holds one WebSocket open — chain-sync's cursor is
    per-connection, so `findIntersection` / `nextBlock` can't ride the
    connect-per-call `OgmiosClient` (which still serves queries/submission).
  - `ChainFollower::spawn` runs a background task: intersect (at recent
    block points on reconnect, else the current tip), then a `nextBlock`
    loop. State is a pure, unit-tested machine: watched tx ids →
    `Pending` / `Included{height, slot}`, plus tip height and a capped
    recent-points deque.
  - Writers (`write_notary`, `submit_ada_transfer`) register the computed
    tx id with the follower **before** submitting (no race with the
    inclusion block; unwatch on submit failure). Both `confirm()` impls
    (`CardanoBackend`, `CardanoAdaSettlement` via `from_backend` sharing)
    now return `Confirmed { depth: tip − inclusion + 1 }`, and a rollback
    past the inclusion point reverts to `Pending` until the tx reappears.
  - Fallback: txs the follower never saw (submitted by a previous process,
    or a standalone `CardanoAdaSettlement::new`) use the old `utxos_at`
    presence check, reported as depth 1.
  - Caveat: unit-tested only (state machine + Ogmios v6 response parsing);
    not yet run against a live Ogmios. That's folded into the live-preprod
    item in `PHASE2_TX_BUILDER_TODO.md`. 90 workspace tests pass.

**Live preprod run — environment fixed, first on-chain writes (2026-07-15/16):**

- **Local preprod node was wedged and is now fixed.** The
  `cardano-node-ogmios:v6.14.0_10.5.1-preprod` container had a frozen tip at
  slot 125366397 (≈ 2026-05-22) across restarts, with healthy peering and no
  logged errors — the signature of a node that can't cross a hard fork.
  Preprod hard-forked past node 10.5; upgrading the compose image to
  `v7.0.0_11.0.1-preprod` (published 2026-06-20) fixed it immediately: the
  node crossed the fork and fully synced (chain DB volume survived, no
  resync from genesis). The compose file documents the wedge signature.
- **Ogmios v6 → v7:** our client and the follower's parsers work unchanged
  against Ogmios 7 — verified live (queries, submission, chain-sync).
- **Bank wallet created and funded.** `secrets/cardano.{addr,vkey,skey}`
  (generated with the container's `cardano-cli`, gitignored), address
  `addr_test1vz233470pc30zzsceh9k2qmzkj4gvj7n976ulvdmed5w9mg0a37ns`, funded
  with 10,000 tADA from the preprod faucet
  (tx `260535f6eca89e5756db9361ce7b7e22c098831974194ad5e1833a78f8f4cd07`).
- **Config fix:** figment cannot deserialize `u128`;
  `SettlementConfig.ada_rate_minor_per_whole_ada` is now `u64` (widened at
  the `StubFxSource` boundary). This had blocked *any* node boot with a YAML
  config file present.
- **First real on-chain notary write.** `notary_write --submit` accepted by
  the node: tx `63eacfe3dbc133f922d461bd3e6488ce21d55f03c5131cd79c965fe2e7491642`
  (<https://preprod.cardanoscan.io/transaction/63eacfe3dbc133f922d461bd3e6488ce21d55f03c5131cd79c965fe2e7491642>),
  confirmed on chain — the wallet's UTxO set is now that tx's outputs. Fee
  174477 lovelace, 430-byte tx, exactly as the offline builder computed.
- **Follower verified live.** Intersection negotiation and real
  forward-block parsing against the live chain (blocks with 1–39 txs), both
  from the running Bank Node and from the new
  `examples/confirm_watch.rs` — which drives the full production path
  (`CardanoBackend::new` → `write_promise` with watch-before-submit →
  poll `confirm()`). Its promise commitment
  (tx `72f119e53a4ad15dbc248ebddd5ab5e8bd4542d9b50f9e0d617fb38611a3632b`)
  ran the full arc live: `confirm()` reported `Pending` while the tx awaited
  a block, the follower logged the inclusion (height 4943193,
  slot 128529077), then `confirm()` returned `Confirmed { depth: 1 }` →
  `Confirmed { depth: 4 }` as blocks stacked on top. **Real depth reporting
  is verified end-to-end on preprod.** Re-run anytime:
  `confirm_watch` needs only the wallet env vars and exits 0 at depth ≥ 2
  (it submits one min-UTxO self-payment per run).
- **Live ADA settlement — the value leg (2026-07-17).** A second
  ("creditor bank") key pair was generated at `secrets/creditor.{addr,vkey,skey}`
  (address `addr_test1vqn7sn79x6k9a2353l458mk2gccmwqk7nza93zydpuvl7lquy6jcl`
  — needs no funding; it only receives). `examples/settle_live.rs` drove the
  production wiring — `CardanoBackend::new` →
  `CardanoAdaSettlement::from_backend` (shared wallet, submit-lock, follower)
  → `settle()` with a `SettlementInstruction` for 354.20 KES net: sized at
  the 3542-minor/ADA stub rate to exactly 10 ADA, submitted
  (tx `787e857c1d49735603d283965b010c0c721aa4cdea627ec1ce8be266a5112845`,
  fee 171221 lovelace), follower logged inclusion at height 4946189 ~2s
  after submission, `confirm()` on the *settlement* backend reported depth
  1 → 2, and the creditor address now holds the 10,000,000-lovelace UTxO.
  **Both Phase-2 write paths — notary and value — are verified live on
  preprod through production code.** `PHASE2_TX_BUILDER_TODO.md`'s
  live-preprod item is closed; only the dust-floor/funding-policy decisions
  remain open there.

**Interface C transport integration test — built and passing (2026-07-18):**

- New `crates/obp-bank-node/src/interface_c/transport_tests.rs` — an
  `#[ignore]`d integration test driving the real `lapin` consumer against a
  live RabbitMQ (`cargo test -p obp-bank-node -- --ignored
  interface_c_transport`; broker override via `OBP_BN_TEST_AMQP_URI`,
  default `amqp://guest:guest@localhost:5672/%2f`). Unique per-run queue
  names; cleans up after itself.
- Covers what only a broker can prove: dispatch by the AMQP `MessageId`
  property, the reply envelope on `replyTo` with `correlationId` carried in
  both the envelope and the AMQP property, acking (queue drained to 0), and
  stream resilience (a malformed body doesn't stall later messages).
  All four known MessageIds + malformed + unknown are exercised; a stub CBS
  (axum, ephemeral port) accepts the credit; asserts include the
  evidence-store side effects (verified triplet stored `verified=true`,
  tampered triplet refused with `OBP-BANK-NODE-COMMITMENT-MISMATCH` but
  still stored as evidence of tampering).
- Passing against the local `rabbitmq:3-management` container (0.11s).
  Default `cargo test` stays green without a broker (test is ignored).

**Settlement idempotency + finality policy — built (2026-07-18):**

- **`settlement_store.rs`** (new, SQLite alongside outbox/evidence; default
  `./outbox/settlements.db`): one durable row per settlement
  `idempotency_key`, lifecycle `SETTLING → SUBMITTED → FINAL` / `ERROR`
  (+ `retryable` flag). The row is **claimed before paying** (`INSERT OR
  IGNORE` is the mutual exclusion), so a redelivered or concurrent
  instruction can never trigger a second transfer, and a crash mid-settle
  leaves an ambiguous `SETTLING` row that is surfaced, never auto-retried.
- **Router `settlement_instruction` rework:** `idempotency_key` (fallback
  `settlement_id`) is now required (`BAD_MESSAGE` otherwise). Replies carry
  explicit `status` / `depth` / `finality_depth` — `SUBMITTED` means
  broadcast, not settled. **Redelivery doubles as status polling**: same key
  → recorded state back (this is how OBP-API observes `SUBMITTED → FINAL`
  within the locked wire contract; documented in the publish plan §4.4).
  Failure classification: `BlockchainError::Rejected` provably never reached
  the chain → retryable on redelivery; transport/internal errors are
  ambiguous → sticky until reconciled.
- **`finality.rs`** (new): background watcher polling
  `SettlementBackend::confirm` (chain-sync follower depth) over `SUBMITTED`
  rows — records depth, promotes to `FINAL` at
  `settlement.finality_depth` (default 15 ≈ 5 min on Cardano), resets depth
  to 0 on rollback, marks on-chain rejection as sticky `ERROR`. Spawned in
  `main` whenever a settlement rail exists (even with the Interface C
  consumer off, so pre-existing `SUBMITTED` rows still finalize).
- **Config:** `settlement.finality_depth` / `finality_poll_secs` /
  `store_path` (defaults 15 / 30 / `./outbox/settlements.db`).
- 21 new unit tests (store lifecycle, watcher transitions incl. rollback,
  router dedupe / retryable-vs-sticky / never-pay-twice); 103 tests pass
  workspace-wide; transport integration test re-run green against the local
  broker.

**OBP-API server-side half — largely built (2026-07-28; developed on OBP-API
branch `open-corridor-salt-relay`, since **merged into `develop`** — merge
commit `d0f9f1768` — and the branch deleted, 2026-07-30. Work continues on
`develop`.):**

- **Hold-at-`PENDING` (netting doc §5)**: `OPEN_CORRIDOR_PROMISE` TRs land at
  `PENDING` in both create branches (`getStatus` below threshold; the
  answer-challenge flow sets `PENDING` instead of posting), the pending-TR
  bulk-status scheduler skips `OPEN_CORRIDOR*` types, and the type's challenge
  threshold defaults to effectively infinite (prop-overridable four-eyes seam).
- **§5.1 report-back endpoint (the salt-relay intake)**:
  `POST /obp/v7.0.0/banks/BANK_ID/accounts/ACCOUNT_ID/transaction-requests/TRANSACTION_REQUEST_ID/open-corridor/promise`
  with body `{tx_hash, blockchain, commitment, salt, preimage}` — **note the
  field is `tx_hash`, not `cardano_tx_hash`** (renamed 2026-07-28; the chain is
  identified by `blockchain`). Role `CanAttachOpenCorridorPromise` (bank-level).
  Evidence stored as TR attributes (`open_corridor_tx_hash/_blockchain/
  _commitment/_salt/_preimage` + reported_by/reported_at audit), append-once,
  idempotent redelivery, row-locked. Errors OBP-40051..53.
- **§5.4 wire DTOs + MessageDocs**: flat lower_snake_case bodies matching this
  repo's `interface_c/types.rs` exactly, in obp-commons
  (`OpenCorridorInterfaceC.scala`); MessageDocs lock the format.
- **§5.2 per-bank publish**: `OpenCorridorBankBroker` registry (+ v7 admin
  endpoints, role `CanConfigureOpenCorridorBroker`; row carries the bank's
  `settlement_address` — open decision resolved) and `OpenCorridorPublisher`
  (publish-and-await-reply per bank vhost, self-contained from the global
  rabbitmq props).
- **§5.3 settle-pair + transactional outbox**:
  `POST /obp/v7.0.0/open-corridor/settle` (role `CanSettleOpenCorridor`,
  gated by `open_corridor_enabled`): nets the pair, TR B posts the net between
  the banks' settlement accounts, covered promises get
  `settled_by_transaction_ids` + `settled_by_transaction_request_id`
  attributes and flip `COMPLETED`; messages enqueued to `OpenCorridorOutbox`
  in the same DB transaction; `OpenCorridorOutboxRelay` publishes with
  backoff, treats settlement `SUBMITTED`/`SETTLING` as redeliver-to-poll
  until `FINAL`, and parks refutable business errors as STICKY. Zero-net
  decision resolved: promises discharge, no Transaction/instruction, credit
  notifications still sent.

**Bank Node consequences (the new items this unblocks):**

1. ~~**Build the promise report-back client**~~ — **done (2026-07-30)**, see
   the dated block below.
2. The node's Interface C consumer + settlement store already match the
   OBP-API relay semantics (redelivery-as-polling, idempotency_key) — no
   changes needed there.

**Promise report-back client — built (2026-07-30):**

- **Outbox**: new terminal-success status `REPORTED` after `PROMISE_WRITTEN`
  (which is no longer terminal — `claim_due` now also returns
  `PROMISE_WRITTEN` rows). New column `obp_transaction_request_id`, recorded
  by `mark_submitted` — OBP-API's TR id was previously only logged, but the
  report-back endpoint is addressed by it. Guarded `ALTER TABLE` migration
  for pre-existing databases.
- **`obp_client.rs`**: `report_promise(bank_id, account_id, obp_tr_id,
  &PromiseEvidence)` POSTs `{tx_hash, blockchain, commitment, salt,
  preimage}` (the locked §5.1 wire names) to
  `/obp/v7.0.0/banks/../accounts/../transaction-requests/{obp_tr_id}/open-corridor/promise`.
  Error split shared with the TR submit via `classify_failure`: 400/422 with
  an OBP-NNNNN code (e.g. OBP-40053 evidence conflict) is terminal →
  `EXCEPTION`; 404/5xx/timeouts are retryable. Idempotent re-post of
  identical evidence succeeds server-side, so redelivery is safe.
- **Dispatcher step 3**: after the chain write, report the evidence and mark
  `REPORTED`. The preimage sent is byte-for-byte the canonical JSON the
  commitment was computed over (`canonical_preimage`, shared with
  `build_commitment`), so the beneficiary's `SHA-256(salt ‖ preimage)` check
  holds — asserted in tests via `PromiseRecord::verify_v1` on the captured
  wire body. A `PROMISE_WRITTEN` row missing the OBP TR id or tx hash can
  never report and goes to `EXCEPTION` for manual reconciliation (affects
  legacy rows written before the column existed).
- 120 workspace tests pass (was 103). **Ops prerequisite:** the node's M2M
  service user needs `CanAttachOpenCorridorPromise` at its bank, or every
  report parks rows at `PROMISE_WRITTEN` (403 is retryable).

**Still not built yet in Rust:**

- **Other A2 delivery modes** — webhook-ISO20022 / database / file-drop (only
  `webhook_obp` is wired); plus the A2 retry schedule.

So the binary now boots and runs the *outbound* path end-to-end (REST `202` →
outbox → OBP-API submit → Cardano Promise commitment → evidence report-back to
OBP-API) and has the *inbound* Interface C consumer ready to receive credit
notifications, capture the salt as evidence, and deliver credits to the CBS.
The OBP-API server-side half (hold-at-PENDING, salt-relay intake, per-bank
publish, settle-pair + outbox relay) is merged into OBP-API `develop` — the
remaining gap to a live round-trip is deployment/config (broker registration,
roles) and end-to-end testing across the two systems.

## The design docs in this repo

Read in this order if returning cold:

1. **`../DOCS/TLDR.md`** — what the OBP Bank Node is and does. Whole-system summary in
   one read. Start here.
2. **`../DOCS/LEDGER_DESIGN.md`** — how OBP-API will track OC netting via full
   double-entry. The *what*. Decided destination.
3. **`OBP_API_CHANGES.md`** — concrete changes inside the OBP-API codebase to
   implement `../DOCS/LEDGER_DESIGN.md`. The *where*, with file references and
   ordering. **This is where to start when picking up OBP-API work.**
4. **`CERT_TODO.md`** — X.509 / mTLS auth for self-service onboarding. Decided
   direction (cert-based, not password). The *what* for provisioning.
5. **`PROVISIONING_API.md`** — concrete changes inside OBP-API to implement
   `CERT_TODO.md`. The *where* for provisioning, with file references and
   ordering. Companion to `CERT_TODO.md`, same shape as `OBP_API_CHANGES.md`.
6. **`NEXT_TODO.md`** — Cardano writer status: Phase 1 done (Ogmios client +
   wallet loading + `confirm()`); Phase 2 (tx build/sign/submit via `pallas`)
   outstanding.
7. **`OPEN_CORRIDOR_SIMPLE_NETTING.md`** — the minimal bilateral
   settle-on-demand netting slice on the existing OBP TR/Transaction model.
   For the PoC it supersedes `../DOCS/LEDGER_DESIGN.md`'s non-posting-Transaction
   promise model (see the 2026-07-14 note above). Its two work items are
   OBP-API-side.
8. **`PHASE2_TX_BUILDER_TODO.md`** — the Cardano tx-builder detail doc:
   what's wired (builder, notary writes, ADA settlement, submit-lock) and
   what remains (live preprod submit, chain-sync `confirm()`, dust/funding
   policy). Salt item closed 2026-07-14.

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
  **Revised for the PoC slice (2026-07-14):** `OPEN_CORRIDOR_SIMPLE_NETTING.md`
  models the promise as an `OPEN_CORRIDOR` TransactionRequest held at
  `PENDING` instead of a non-posting Transaction — no new column, no snapshot
  table, one posted Transaction per bilateral settle. The double-entry model
  above remains the destination if/when snapshots or multilateral are needed.
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

**Production directive (2026-07-18):** Simon: "forget about a PoC, we need to
code for real now." No more PoC framing; PoC-justified shortcuts are expired
work items (saved as a persistent memory). Scope narrowings stay legitimate
(one corridor, one currency, bilateral, settle-on-demand); quality/durability
shortcuts do not. On the OBP-API side this made the per-bank broker registry
and the transactional publish outbox requirements (publish plan §5.2/§5.3,
OBP-API repo). Bank-Node-side consequences are items 2–4 below.

**Current recommendation (updated 2026-07-18), while a separate session owns
OBP-API:** stay on Bank-Node-only work. Salt gap, chain-sync `confirm()`, and
the live-preprod run are done (see the 2026-07-15/16 block above for the
on-chain evidence). Next, in rough order:

1. ~~**Promise report-back client**~~ — **done (2026-07-30)**, see the dated
   block above: dispatcher step 3 reports the evidence to the §5.1 endpoint
   (now merged into OBP-API `develop`) and advances the row to `REPORTED`.
2. ~~**Real FX source**~~ — **done (2026-07-18), interim web2 source.**
   `crates/obp-bank-node/src/fx.rs`: `CoinGeckoFxSource` (free public API,
   quotes ADA directly in KES; live-verified — real rate ≈ 21.2 KES/ADA,
   i.e. 2122 minor). Selected via `settlement.fx_source: coingecko`
   (default); `stub` remains for offline dev. Quote failures deliberately
   map to `Rejected` (pre-submit → retryable in the settlement store);
   client sets a User-Agent (CoinGecko 403s without one). **API3**
   (TESOBE partnership) replaces this behind the same `FxSource` trait
   later — open questions for the partnership: consumption path (on-chain
   dAPI proxy read vs Signed API HTTP), fiat legs (KES), staleness
   guarantees.
2b. **Coordination — TR type renames (OBP-API decisions 2026-07-18):** the
   promise TR type becomes `OPEN_CORRIDOR_PROMISE` (was `OPEN_CORRIDOR`)
   plus a new `OPEN_CORRIDOR_SETTLEMENT` type, and discharge linkage moves
   to a `settled_by_transaction_ids` TR attribute (see the OBP-API repo's
   `OPEN_CORRIDOR_SIMPLE_NETTING.md` rev note). **Bank Node impact when the
   rename lands:** `obp_client.rs` submits type `OPEN_CORRIDOR` on
   Interface B, and `DOCS/A1_A2.md` + the A1 response body name the old
   type — follow the rename in lockstep with the OBP-API session.
3. ~~**Settlement finality policy**~~ — **done (2026-07-18)**, see the dated
   block above: durable settlement store (claim-before-pay idempotency),
   explicit `SUBMITTED`/`FINAL` reply statuses, finality watcher at
   configurable depth (default 15), redelivery-as-polling semantics
   documented in the publish plan §4.4.
4. **Key management** (production directive): the Cardano `.skey` currently
   sits on disk (0600, gitignored). Production needs an operational answer —
   at minimum documented key custody/rotation; likely an encrypted store or
   KMS/HSM-backed signing later. Ties into `CERT_TODO.md` provisioning.
5. **Open policy decisions** from `PHASE2_TX_BUILDER_TODO.md`: dust floor
   for sub-min-UTxO nets, and the wallet funding/float process. (The
   debtor→creditor transfer and the Interface C transport test were
   completed 2026-07-17/18 — see above.)
6. **Other A2 CBS delivery modes** — only `webhook_obp` is wired; add
   webhook-ISO20022 / database / file-drop, plus the A2 retry schedule.
7. **Per-vhost multi-tenant topology** (`/bank.{bank_id}`) — decided but not
   yet wired in code; TLS/`amqps://` follows once the broker is
   mTLS-enabled (`CERT_TODO.md`).

The Interface C consumer work for the settle-pair messages should still wait
until the OBP-API netting side stabilises, to avoid churning against a
moving contract.

In rough order of how big a commitment each is:

**Small / clarifying:**
- Pick answers to the open policy questions above so the OBP-API
  implementation can be properly specced.
- Pick the CA backend (Vault PKI vs step-ca) so `PROVISIONING_API.md` §3
  can be implemented against a concrete target.

**Critical path to a full round-trip — DONE (2026-07-31, see the dated
block at the top):**
- ~~OBP-API server-side half~~ — built and exercised: promise report-back,
  outbox relay publishing `credit_notification`(+salt) and
  `settlement_instruction`, redelivery-until-FINAL all verified live.
- ~~Settlement-trigger wiring~~ — the admin settle-pair endpoint is the
  trigger; exercised end-to-end (netting → instruction → ADA transfer →
  FINAL → DELIVERED).

**Medium / incremental Bank Node work:**
- ~~Integration test of the inbound transport~~ — **done (2026-07-18)**, see
  the dated block above (`interface_c/transport_tests.rs`, `#[ignore]`d,
  runs against a local RabbitMQ).
- Other A2 CBS delivery modes — only `webhook_obp` is wired; add
  webhook-ISO20022 / database / file-drop, plus the A2 retry schedule.
- Per-vhost multi-tenant topology (`/bank.{bank_id}`) — decided but not yet
  wired in code. TLS / `amqps://` for Interface C is a follow-on once the
  broker is mTLS-enabled (see `CERT_TODO.md`).

**Large / OBP-API ledger work** *(currently owned by a separate session —
coordinate before touching; and note `OPEN_CORRIDOR_SIMPLE_NETTING.md`
replaces the promise-as-Transaction model below for the PoC slice)*:
- The two items from `OPEN_CORRIDOR_SIMPLE_NETTING.md` §6: hold the
  `OPEN_CORRIDOR` TR at `PENDING` in `OpenCorridorProcessor`, then the admin
  settle-pair endpoint (net, post one Transaction, link `transaction_ids`,
  mark `COMPLETED`, publish per `OPEN_CORRIDOR_INTERFACE_C_PUBLISH_PLAN.md`).
- The fuller `OBP_API_CHANGES.md` sequence (schema migrations →
  constants/status modules → mappers → connector method → TR flow) remains
  the path if the double-entry `../DOCS/LEDGER_DESIGN.md` destination is pursued.

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

The outbound path is live: `POST /transaction-requests` validates the request,
persists it to the durable SQLite outbox, and returns a `202` with a minted
`transaction_request_id` *before* any external call. The background dispatcher
then submits to OBP-API and writes the Cardano Promise commitment. The inbound
Interface C consumer is built but off by default (`rabbitmq.enabled=false`);
enable it to receive credit notifications, capture the salt as evidence, and
deliver credits to the CBS.

## Memory / Claude session

- The "OBP-API code stays robust even in tracer-bullet work" feedback rule is
  saved as a persistent Claude memory; future sessions in this project will
  apply it automatically.
- All design docs (TLDR, NEXT_TODO, CERT_TODO, LEDGER_DESIGN,
  OBP_API_CHANGES, PROVISIONING_API) are committed-style — they reflect the
  latest state of the discussion as of pause time.

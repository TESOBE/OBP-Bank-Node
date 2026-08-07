# Localhost round-trip test — living status

**COMPLETE 2026-07-31 ~10:28 UTC — full round trip achieved on preprod.
Evidence log at the bottom. Four real defects were found and fixed on the
way (see "Fixes applied"); the follow-ups they exposed are in
`WIP/NEXT_STEPS.md`.**

Goal: full Open Corridor round trip on one machine — Bank Node A (debtor,
`rt.bank.a`, Cardano preprod) → OBP-API (one instance, both banks) →
settle-pair → RabbitMQ per-bank vhosts → Node A executes ADA settlement,
Node B receives credit notification + evidence and posts to a CBS stub.

## What is DONE

1. **Environment verified** (2026-07-30): OBP-API `develop` commit
   `1201ac795` on :8080 (Lift + http4s v7 same port); RabbitMQ :5672/:15672
   (guest/guest); cardano-node+Ogmios :1337 preprod fully synced; Postgres
   `sandbox` DB (`jdbc:postgresql://localhost:5432/sandbox?user=obp`,
   password in default.props line 34).
2. **API consumer created** directly in Postgres: name
   `obp-bank-node-roundtrip`, key `rtkey6a33e2a63adba1658290ecac776ff155`
   (secret in `consumer` table). Super-admin DirectLogin verified
   (`TheSuperUserAForTesting` / props line 290 `super_admin_inital_password`).
3. **`open_corridor_enabled=true`** appended to
   `obp-api/src/main/resources/props/default.props` (line 346) **and injected
   into the built jar** (`jar uf` — props are baked at build time, source edit
   alone is not enough). Needed because the outbox relay is boot-gated
   (`Boot.scala:566`) and the settle endpoint checks it.
4. **OBP-API restarted with the flag** — Simon's foreground java process was
   killed and relaunched via nohup (log `/tmp/obp-api.log`). Verified UP.
   **After reboot it will NOT auto-start** — see resume checklist.
5. **Bank Node wire-contract fix (code change, committed to working tree)**:
   OBP's `TransactionRequestBodyOpenCorridorJsonV700` requires `to.name`,
   `to.description`, secondary/branch routing fields and `charge_policy` —
   none had defaults, so the node's verbatim A1.1 replay would 400.
   `rest/types.rs`: `BeneficiaryRouting` extended to mirror
   `PostSimpleCounterpartyJson400` field-for-field (new fields
   serde-defaulted), `InitiateRequest.charge_policy` default `SHARED`.
   Workspace tests green (77+43 pass, 2 ignored).
6. **All round-trip artifacts written** in `dev/`:
   - `setup_rabbitmq.sh` — vhosts `/bank.rt.bank.a` + `/bank.rt.bank.b`,
     users `bank_a_node`/`rtpass-a`, `bank_b_node`/`rtpass-b` (idempotent)
   - `setup_obp.sh` — banks `rt.bank.a`/`rt.bank.b`, node users
     `rt.node.a`/`rt.node.b` (pass `RtNodePass2026!`), roles
     (CanAttachOpenCorridorPromise to node A; CanSettleOpenCorridor etc. to
     admin), KES accounts (`settlement-a`, `settlement-b`, plus the fixed-id
     `OBP-OUTGOING-SETTLEMENT-ACCOUNT` / `OBP-INCOMING-SETTLEMENT-ACCOUNT`
     at both banks — settlePair reads those ids; boot only creates them for
     the default bank), broker registrations (B's `settlement_address` =
     creditor addr), writes DirectLogin tokens to `env.sh`
   - `node-a.yaml` (:8088, cardano, consumer on A's vhost, finality_depth 2)
   - `node-b.yaml` (:8089, mock chain, consumer on B's vhost)
   - `cbs_stub.py` (:9009, appends to `data/cbs_received.jsonl`)
   - `run_roundtrip.sh` — fires payment, polls outbox to REPORTED, settles,
     polls settlement store to FINAL, dumps node-B evidence + CBS credits

## Fixes applied during first execution (2026-07-31)

1. **Node submit URL was wrong** — `obp_client.rs` built
   `.../accounts/{acct}/views/owner/transaction-request-types/...` but the
   OBP route is `.../accounts/{acct}/owner/transaction-request-types/...`
   (view id is a bare path segment, no `views` literal; `Http4s700.scala:3360`).
   Fixed in `obp_client.rs` + its test fixtures.
2. **New users need email validation before DirectLogin** (`OBP-20073`) —
   `UPDATE authuser SET validated=true WHERE username IN ('rt.node.a','rt.node.b')`
   in Postgres, then re-run `setup_obp.sh` to mint tokens.
3. **Challenge threshold needs an EUR↔KES FX rate** — submit failed with
   `OBP-50206 <- OBP-10006 (EUR to KES not supported)`. Created rates both
   directions at both banks via `PUT /obp/v2.2.0/banks/{id}/fx` (role
   `CanCreateFxRate`, rate 150 KES/EUR). `setup_obp.sh` does NOT do this yet.
4. **`transactionrequestattribute.value` was varchar(255)** — too small for
   the promise preimage JSON, report-back died with a PSQL length error.
   Fixed in OBP-API source (`TransactionRequestAttribute.scala:38` →
   `MappedText`) **and** `ALTER TABLE transactionrequestattribute ALTER COLUMN
   value TYPE text` on the live DB (running jar is fine — Lift does not crop;
   jar rebuild picks up the source change whenever it next happens).
5. **Script fixes**: bank-already-exists code is `OBP-34000` (not OBP-30206) in
   `setup_obp.sh`; `run_roundtrip.sh` — settlements column is `last_depth`
   (not `confirmation_depth`) and sqlite3 CLI is absent on this machine so it
   now uses a python3 `sq()` helper.

## Key design facts discovered (don't re-derive)

- Promise netting matches on the TR's `to_bank_id`; beneficiary routing must
  be scheme `OBP` + `other_bank_routing_address=rt.bank.b` +
  `other_account_routing_address=settlement-b` (an existing account at B) —
  resolution in `BankingData.scala:471`.
- Settle body: `{bank_id_a, bank_id_b, currency}` →
  `POST /obp/v7.0.0/open-corridor/settle`. TR B's id = `settlement_id` =
  the node's settlement `idempotency_key`.
- Report-back endpoint & role verified present in the running build.

## Original resume checklist (all steps executed 2026-07-31 — kept as the
## re-run recipe after any reboot)

1. (after reboot) Start services:
   - `docker start rabbitmq docker-cardano-node-ogmios-1` (or compose up)
   - OBP-API: `cd ~/Documents/workspace_2024/OBP-API-Simon/OBP-API && nohup java --add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.lang.reflect=ALL-UNNAMED --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.lang.invoke=ALL-UNNAMED --add-opens java.base/java.util.jar=ALL-UNNAMED --add-opens java.base/sun.reflect.generics.reflectiveObjects=ALL-UNNAMED --add-opens java.base/java.io=ALL-UNNAMED --add-opens java.base/java.util.concurrent=ALL-UNNAMED --add-opens java.base/java.security=ALL-UNNAMED -jar obp-api/target/obp-api.jar > /tmp/obp-api.log 2>&1 &`
     (the jar already contains the prop; wait ~2-3 min, check
     `curl -s localhost:8080/obp/v7.0.0/root`)
2. `bash dev/setup_rabbitmq.sh` (NOT yet run — was about to when
   paused)
3. `bash dev/setup_obp.sh` (NOT yet run). Caveats — endpoints were
   written from the JSON factories but not yet exercised; verify on first
   run: user-creation path (`POST /obp/v5.1.0/users`), user-lookup path
   (`GET /obp/v5.1.0/users/username/{u}`), create-bank version (v5.0.0),
   and the grep'd "already exists" error codes for idempotent re-runs.
4. Start the three processes (from repo root; each in its own terminal or
   nohup):
   - `python3 dev/cbs_stub.py`
   - `source dev/env.sh && OBP_BANK_NODE_CONFIG=dev/node-a.yaml OBP_BN_OBP_API__DIRECT_LOGIN_TOKEN="$RT_NODE_A_TOKEN" cargo run -p obp-bank-node`
   - `source dev/env.sh && OBP_BANK_NODE_CONFIG=dev/node-b.yaml OBP_BN_OBP_API__DIRECT_LOGIN_TOKEN="$RT_NODE_B_TOKEN" cargo run -p obp-bank-node`
5. `bash dev/run_roundtrip.sh` — caveats to verify on first run:
   the sqlite column names it queries (`settlements.confirmation_depth`,
   `evidence` table columns) were written from memory, not checked against
   the actual schemas in `settlement_store.rs` / `evidence.rs`; fix the
   SELECTs if they error.
6. Record results in the Evidence log below; update `WIP/NEXT_STEPS.md`.

## Watch-outs for the run

- Node A submits as `rt.node.a`, who owns `settlement-a` (owner view via
  account creation) — no extra TR-create role should be needed; if OBP
  returns InsufficientAuthorisation, grant `CanCreateAnyTransactionRequest`
  at `rt.bank.a`.
- DirectLogin tokens in `env.sh` may not survive an OBP-API restart
  (depends on token validation) — if nodes get 401s, re-run
  `setup_obp.sh` to mint fresh tokens.
- Settlement is real preprod ADA from the bank wallet (fees ~0.17 tADA per
  tx; wallet holds ~10k tADA). finality_depth=2 → FINAL in ~1 min.
- CoinGecko FX is live; if offline, switch `node-a.yaml` to
  `fx_source: stub` (rate 2122 minor KES/ADA already set).

## Evidence log

**Run of 2026-07-31 (all times UTC). Everything below is verifiable on
preprod / in the local stores.**

Payments (KES 500.00 each, `rt.bank.a` → `rt.bank.b`, beneficiary
`settlement-b`):

| node TR | OBP TR | promise tx (cardano preprod) |
|---|---|---|
| `eb5f9c42-8c36-4aa7-a1d4-5fd7fab0b02c` | `78334e58-9827-47ae-bf2e-20ea6f786391` | `c69b0222aec5e282649c12699d1b3ee603390d2e509ceca285e17aa3268950af` |
| `ff1126ea-f5d3-4600-8994-7e791647d202` | `db8d044b-d743-469e-8c34-d545f75fe5ec` | `9faa2ef49d221c397ab782413037eaae413d5bc6d687526c76f871fe52608014` |

(A third node TR, `9b3dc6e6…`, is EXCEPTION — it was the first attempt,
killed by the URL bug before ever reaching OBP; no on-chain or OBP artifact.)

- Both node TRs reached **REPORTED** (promise on-chain + evidence attached
  to OBP via the report-back endpoint).
- **Settle-pair** netted both: settlement `5578e3d5-ecb5-41f9-8f06-60c2b0c3ca51`,
  net **1000.00 KES**, 2 credit notifications + 1 settlement instruction
  enqueued.
- **Node B (beneficiary)**: both credit notifications received over its
  vhost, commitments **verified=1** against the relayed salt+preimage, both
  posted to the CBS stub (`RT-CBS-0001`, `RT-CBS-0002` in
  `data/cbs_received.jsonl`).
- **ADA settlement (node A)**: sized at stub rate 2122 minor-KES/ADA →
  **47 125 353 lovelace** to
  `addr_test1vqn7sn79x6k9a2353l458mk2gccmwqk7nza93zydpuvl7lquy6jcl`,
  tx `3c4ac0db6c3f43dc5131cd9386033a9efd5d4fa2cfe29a5383b2c4f54672c7e6`,
  fee 172 013 lovelace.
  https://preprod.cardanoscan.io/transaction/3c4ac0db6c3f43dc5131cd9386033a9efd5d4fa2cfe29a5383b2c4f54672c7e6
- **FINAL at depth 2** at 10:26:32Z (settlement store row). OBP's outbox
  relay re-polled, saw `status: FINAL`, and marked the instruction row
  **DELIVERED** at 10:28:39 — closing the loop end-to-end.
- The relay's redelivery-until-FINAL behaved exactly as designed: the first
  settle attempt failed on FX (CoinGecko dropped KES), the node stored the
  row as ERROR/retryable, and the relay's next redelivery — after the node
  was restarted with the stub rate — reopened and completed it.

### Follow-ups discovered (moved to WIP/NEXT_STEPS.md)

1. CoinGecko no longer quotes ADA/KES (`supported_vs_currencies` has no KES)
   — corridor currencies need a fiat cross-rate (ADA/USD × USD/KES) or API3.
2. The node's `GET .../transaction-requests/{id}` never fills
   `settlement_id`/`settled_at` (fields exist in `TransactionRequestStatus`
   but nothing links the pair-level settlement back to outbox rows).
3. OBP-API jar must be rebuilt at some point to pick up the
   `TransactionRequestAttribute.Value` → `MappedText` source fix (live DB
   already ALTERed, so not urgent).

# OBP Bank Node App — demo / manual-test UI

Decided 2026-08-07: lives at `/app` in this repo. Talks **only** to
OBP-Bank-Node APIs (in the demo topology: node A on :8088, node B on :8089).
It never calls OBP-API, RabbitMQ, or the chain directly — everything it shows
or triggers goes through a node's south-side REST API. No business logic in
the app; it is read-and-trigger only, outside the money path.

## Purpose

Manual testing of Bank Node functionality, and a demonstrable walk-through of
the Open Corridor round trip (the flow proven live 2026-07-31, see
`roundtrip/STATUS.md`) without shell scripts and SQLite dumps.

## Demo storyline the UI must carry

1. **Send** (node A): form posts the A1.1 `OPEN_CORRIDOR` request
   (`POST /obp-bank-node/v5.1.0/transaction-requests`), including the
   `originator` block.
2. **Promise** (node A): the outbox row advances
   `INITIATED → SUBMITTED → PROMISE_WRITTEN → REPORTED`; show the promise tx
   hash with a Cardanoscan preprod link, and the salted-commitment story
   (on-chain metadata = hash only; cleartext + salt held by the banks).
3. **Position** (both nodes): bilateral view assembled by the app —
   node A's unsettled outbound rows vs node B's unsettled outbound rows,
   with the net. (Pre-settlement, each node only knows its own outbound leg;
   the app joins the two. OBP-API is not consulted.)
4. **Settle** (node A): button calls the node's new settle-request endpoint
   (below); watch the settlement row go `SETTLING → SUBMITTED → FINAL` with
   confirmation depth, Cardanoscan link for the ADA transfer.
5. **Credit** (node B): credit notification received, commitment verified
   (`SHA-256(salt ‖ preimage)` recomputed and matching), credit delivered to
   the CBS.

## Node API additions required (all in `crates/obp-bank-node`)

The app is intentionally dumb; where data is missing, the node API grows,
not the app.

**All four built 2026-08-07** (138 workspace tests pass; details in
`NEXT_STEPS.md`'s dated block):

1. ~~**Settlement store read endpoints**~~ — built:
   `GET .../settlements` (list, most recent first) and
   `GET .../settlements/{key}` where `{key}` matches `idempotency_key` OR
   `settlement_id`. Returns status, `depth` + `finality_depth`, tx hash,
   `net_amount_minor`, asset/amount, error/retryable, timestamps. The store
   is now opened unconditionally in `main`, so the read API works even in
   mock mode / with the consumer off.
2. ~~**Evidence store read endpoints**~~ — built:
   `GET .../evidence` and `GET .../evidence/{transaction_request_id}`: the
   commit–reveal triplet, verified flag, and the CBS delivery result — new
   `cbs_status` / `cbs_reference` / `cbs_recorded_at` columns (guarded
   ALTER migration), recorded by the Interface C credit handler on both
   delivery success (`DELIVERED`) and failure (`FAILED`).
3. ~~**Settlement linkage**~~ — built: outbox gained `settlement_id` /
   `settled_at` columns (guarded ALTER). `OutboxStore::mark_settled` stamps
   every row whose OBP TR id appears in a settle result's
   `covered_transaction_request_ids` (idempotent; the other bank's ids match
   nothing and are skipped). Stamped from the settle trigger (below) and
   from the corridor status proxy — the latter is how a node that did NOT
   trigger the settlement picks the linkage up.
   `GET .../transaction-requests/{id}` now surfaces both fields.
4. ~~**Settle-request endpoint**~~ — built:
   `POST /obp-bank-node/v5.1.0/settlements` with body
   `{other_bank_id, currency}` → the node calls OBP-API's
   `POST /obp/v7.0.0/banks/{own BANK_ID}/open-corridor/settlements` over
   Interface B with its M2M credentials, stamps the covered outbox rows,
   and relays OBP's result (plus `covered_outbox_rows_stamped`) as the 201.
   Companion `GET .../settlements/{id}/corridor` proxies OBP-API's
   settlement status (ledger + rail + messages) for corridor-wide polling.
   Interactive error split: an OBP business rejection (403 missing role,
   404 unknown settlement, …) passes through with its original status and
   OBP code; transport trouble is a 502 `OBP-BANK-NODE-INTERFACE-B-001`.
   `roundtrip/setup_obp.sh` now grants both node service users the
   bank-scoped `CanSettleOpenCorridor` at their own bank.

## Shape

**Built 2026-08-07** (`crates/obp-bank-node-app`; 10 crate tests, 148
workspace-wide). Config file `obp-bank-node-app-config.yaml` (cwd, path
overridable via `OBP_BANK_NODE_APP_CONFIG` — mirrors the node's
`OBP_BANK_NODE_CONFIG`), env prefix `OBP_BN_APP_` (e.g.
`OBP_BN_APP_SERVER__BIND=0.0.0.0:8091` — note this machine's nginx
already occupies the default 8090). Defaults: bind `0.0.0.0:8090`, nodes
`node-a`→:8088, `node-b`→:8089. Run: `cargo run -p obp-bank-node-app`.
Dev-env wiring (2026-08-07, same day): `roundtrip/` gained
`app-a.yaml`/`app-b.yaml` (per-node UI instances on :8091/:8092, home
node listed first = default in the send/settle selectors; the other node
included so the bilateral position view works — production would list
only the bank's own node) plus `start_node_a.sh` / `start_node_b.sh` /
`start_app_a.sh` / `start_app_b.sh`, and
`commands/_helpers/open_dev_env` now opens four themed terminals running
them (nodes A/B + both UIs). The proxy
(`/api/nodes/{name}/{path}`) forwards only a whitelist of south-side
GETs plus the two POST triggers (transaction-requests, settlements), and
relays the node's status + JSON verbatim; per-node `bearer_token` config
is the auth seam (unauthenticated localhost for the demo). The UI is one
static page (embedded `include_str!`, no build step) with the five
storyline sections, 3s polling, Cardanoscan preprod links. Node-side
addition made for the position view: `TransactionRequestStatus` now
carries `value`, `other_bank_id`, `description` projected from the
stored A1.1 payload — the position table nets those client-side.

- **`/app` = one small axum crate** (`obp-bank-node-app`), added to the
  workspace: serves the static UI and proxies JSON to the configured nodes.
  Rationale for a backend at all: node credentials stay out of the browser
  (A-interface auth is OAuth2 + PSD2-CERT per `../DOCS/A1_A2.md` — locally
  unauthenticated, but the app must not bake in the assumption), and no
  CORS changes are needed on the node.
- **Config**: list of nodes (`name`, `base_url`, credentials ref). Demo
  config = node A + node B from `roundtrip/`.
- **UI**: static HTML/JS, polling the app's JSON endpoints. No framework
  build step unless it earns it.
- The app displays; the nodes decide. Any state transition shown must be
  readable from a node endpoint, never inferred by the app.

## Honest-demo caveats

- FX runs on the stub rate until the KES cross-rate / API3 work lands
  (CoinGecko dropped KES, 2026-07-31).
- The demo requires the full local stack (OBP-API, RabbitMQ, cardano-node +
  Ogmios, both nodes, CBS stub). Wrap bring-up + health checks in one
  script so a dead service is caught before, not during, a demo.

## Open

- ~~Grant `CanSettleOpenCorridor` to node service users: both sides or
  debtor only?~~ Resolved 2026-08-07: the role is bank-scoped; both banks'
  service users hold it at their own bank. Corridor settlement policies
  (min net, min age, windows — the designed `open_corridor_settlement_policies`
  table) remain the follow-on control against fee-griefing by frequent
  settles.
- Node API auth for the app in the demo environment: unauthenticated
  localhost first, or wire OAuth2 client-credentials from the start?
- ~~Whether the settle-request endpoint takes a counterparty bank parameter
  (multi-corridor future) or settles the node's single configured
  corridor.~~ Resolved 2026-08-07: it takes
  `{other_bank_id, currency}` — mirrors the OBP-API resource body,
  multi-corridor ready, and the handler refuses an `other_bank_id` equal to
  the node's own bank before any OBP call. (Field renamed from
  `counterparty_bank_id` on 2026-08-07: OBP's idiom for the far side is
  `other_*`, and "counterparty" would wrongly suggest a real OBP
  Counterparty entity.)

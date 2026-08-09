# OBP Bank Node App — demo / manual-test UI

Decided 2026-08-07: lives at `/app` in this repo. The storyline pages talk
**only** to OBP-Bank-Node APIs (in the demo topology: node A on :8088, node B
on :8089) — they never call OBP-API, RabbitMQ, or the chain directly;
everything they show or trigger goes through a node's south-side REST API. No
business logic in the app; it is read-and-trigger only, outside the money
path.

Revised 2026-08-08 (with Simon): the app gained a second, explicitly separate
surface — the `/setup` **operator page** (see §Setup page below), which DOES
talk to OBP-API, as a logged-in administrator. The node-only boundary now
applies to the storyline pages; the setup page is the one deliberate
exception, with its own auth and its own config block.

## Purpose

Manual testing of Bank Node functionality, and a demonstrable walk-through of
the Open Corridor round trip (the flow proven live 2026-07-31, see
`dev/STATUS.md`) without shell scripts and SQLite dumps.

## Demo storyline the UI must carry

1. **Send** (node A): form posts the A1.1 `OPEN_CORRIDOR` request
   (`POST /obp-bank-node/v5.1.0/transaction-requests`), including the
   `originator` block.
2. **Promise** (node A): the outbox row advances
   `INITIATED → SUBMITTED → PROMISE_WRITTEN → REPORTED`; show the promise tx
   hash with a Cardanoscan preprod link, and the salted-commitment story
   (on-chain metadata = hash only; cleartext + salt held by the banks).
3. **Position**: this bank's unsettled outbound exposure per corridor
   (other bank × currency), read from its own node. (Revised 2026-08-07
   with Simon: each app instance is **locked to one node**, reflecting what
   a bank client deploying Open Corridor actually sees — pre-settlement a
   bank only knows its own outbound legs; the authoritative bilateral net
   is OBP-API's, surfaced through the settle/corridor view in step 4. The
   earlier bilateral join across both nodes is dropped; on localhost the
   two app instances side by side give the whole-corridor picture.)
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
   `dev/setup_obp.sh` now grants both node service users the
   bank-scoped `CanSettleOpenCorridor` at their own bank.

## Shape

**Built 2026-08-07** (`crates/obp-bank-node-app`; 10 crate tests, 148
workspace-wide). Config file `obp-bank-node-app-config.yaml` (cwd, path
overridable via `OBP_BANK_NODE_APP_CONFIG` — mirrors the node's
`OBP_BANK_NODE_CONFIG`), env prefix `OBP_BN_APP_` (e.g.
`OBP_BN_APP_SERVER__BIND=0.0.0.0:8091` — note this machine's nginx
already occupies the default 8090). Defaults: bind `0.0.0.0:8090`, nodes
`node-a`→:8088, `node-b`→:8089. Run: `cargo run -p obp-bank-node-app`.
Later the same day: config gained an optional `ui_defaults` map (form
prefill keyed by field name, served at `GET /api/ui-defaults`; set per
instance in `dev/app-a/b.yaml` so each UI prefills its own demo
beneficiary — out-of-band knowledge, the app never enumerates the other
bank's accounts), the Send form takes the full end-recipient routing
(beneficiary name + bank/account/originator scheme·address pairs, each
pair grouped as one visual unit), and the app launchers print the demo
beneficiary hint. Dev-env wiring (2026-08-07, same day): `dev/` gained
`app-a.yaml`/`app-b.yaml` (per-node UI instances on :8091/:8092, each
**locked to its own node** — the bank-client view, see the revised
storyline step 3) plus `start_node_a.sh` / `start_node_b.sh` /
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
  config = node A + node B from `dev/`.
- **UI**: static HTML/JS, polling the app's JSON endpoints. No framework
  build step unless it earns it.
- The app displays; the nodes decide. Any state transition shown must be
  readable from a node endpoint, never inferred by the app.

## Setup page (`/setup`) — built 2026-08-08

Moves `dev/setup_obp.sh`'s OBP-API provisioning into the app as an operator
page, so an admin can verify an OBP-API instance is ready for Open Corridor
(and fix it) without shell scripts.

- **Auth = the Portal's scheme** (decided with Simon; implementation mirrors
  `OBP-Frontend/packages/shared/src/lib/server/oauth`): the app fetches
  OBP-API's `GET /obp/v5.1.0/well-known` provider list, picks the configured
  provider (default `obp-oidc`), reads its OIDC discovery document, and
  redirects the admin's browser through the authorization-code flow with
  PKCE (S256). The code is exchanged server-side with the registered
  consumer's client id/secret; tokens live in an in-memory session behind an
  opaque HttpOnly cookie — the browser never sees a token or credential.
  Config block `obp_api` (`base_url`, `oauth_provider`, `oauth_client_id`,
  `oauth_client_secret`, `callback_url`, optional `discovery_url` override);
  without it the page reports "not configured" and no OBP-API call is made.
  **Prerequisite:** an OBP consumer registered with redirect URL
  `http://<app>/setup/callback` — its key/secret go in the config (or
  `OBP_BN_APP_OBP_API__OAUTH_CLIENT_*` env).
- **Declarative desired state** in the `setup` config block (see
  `dev/app-a/b.yaml`, which carry the corridor world `setup_obp.sh`
  creates): routing schemes (wire-shaped, posted verbatim), banks with
  accounts / FX rates / broker registrations, and role grants. The page
  renders every item as a read-only check (`ok` / `missing` / `differs` /
  `unverified` / `error`) with per-item **Apply** + **Apply all missing**;
  applies re-derive the action server-side from config (the browser only
  names an item id) and mirror the script's idempotent calls
  ("already exists" = success). "Your entitlements" checks the admin's own
  roles against what the applies need, with self-grant as the apply.
  A free-form **test account** form covers seeding demo accounts.
  Every item carries the role its apply needs (`required_role`), and a
  **Request role** button next to each Apply files an OBP entitlement
  request (`POST /obp/v3.0.0/entitlement-requests`) for the logged-in
  admin — the path when self-granting isn't possible; a pending duplicate
  counts as success (added 2026-08-08 with Simon).
- **Status JSON block** (added 2026-08-08): a copy-pasteable
  machine-readable snapshot below the checks — the full status body
  (`generated_at`, OBP-API info, admin, all items with statuses) plus the
  instance's node list with `/health` results through the proxy. Intended
  to be handed verbatim to an agent working on the Bank Node or this app.
- **No user registration, no email validation** (Simon, 2026-08-08, hard
  scope line): accounts/grants naming an `owner_username`/`username` require
  the user to already exist — a missing user is an error, never a
  provisioning action. The node service users (and the psql
  `authuser.validated` flip + `env.sh` token writing) stay with
  `setup_obp.sh`, which remains the one-shot bootstrap; the page covers
  everything after that.
- The admin acts with their own entitlements — the page holds no service
  credential and adds no OBP-API changes; every call is an existing
  endpoint (`banks`, `routing-schemes`, `accounts`, `fx`, entitlements,
  `open-corridor/broker`).

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

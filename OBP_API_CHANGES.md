# OBP-API Changes for Open Corridor Netting

What needs to change inside the `OpenBankProject/OBP-API` codebase to support the
full double-entry ledger model from `LEDGER_DESIGN.md`. Companion document, not a
replacement.

**Robustness rule applies throughout.** Bank Node / scripts / mocks can be simple;
OBP-API code is production-grade regardless of how thin the slice is.

## 1. Database schema (Mapper / Lift schemifier)

### Reuse existing fields where possible

OBP-API already has fields that carry the right semantics. Use them by convention,
don't add parallel columns:

| Need | Existing field | Convention |
|---|---|---|
| Mark an account as an Open Corridor settlement account | `MappedBankAccount.kind` (`MappedString(255)`, comment "the account type aka financial product name") | Set `kind = "OPEN_CORRIDOR_SETTLEMENT"` |
| Mark a transaction as an Open Corridor Promise / Settlement | `MappedTransaction.transactionType` (`MappedString(100)`) | Set `transactionType = "OPEN_CORRIDOR_PROMISE"` or `"OPEN_CORRIDOR_SETTLEMENT"` |
| Lifecycle state of a Promise / Settlement | `MappedTransaction.status` (`MappedString(20)`, `Option[String]`, added Aug 2025 for Berlin Group support) | Reuse the column with a registered Open Corridor value-domain: `PROMISED`, `NETTED`, `SETTLED`, `EXCEPTION`, `REVERSED`. All values fit the 20-char limit. |

Define these well-known values as Scala constants somewhere central (e.g.
`code/open_corridor/model/OpenCorridorConstants.scala`) so they're not stringly-typed across the codebase.

#### Why `status` reuse is safe (no BG code change required)

The only place `MappedTransaction.status` is read with a value-comparison today is
`JSONFactory_BERLIN_GROUP_1_3.scala:582-583`, which classifies transactions as
booked-vs-pending against `TransactionRequestStatus.COMPLETED`. That filter never
sees an Open Corridor transaction in practice because:

1. Open Corridor settlement accounts have `kind = OPEN_CORRIDOR_SETTLEMENT` and are inter-bank ledger
   entries, not customer-facing accounts.
2. BG endpoints (`/v1.3/accounts/{id}/transactions`) operate on customer-facing
   accounts; the View / `AccountAccess` permissions never expose settlement
   accounts to BG-level callers.
3. Realistic deployments are single-purpose (BG *or* Open Corridor), not both.

The existing BG filter is therefore correct for its own scope and stays unchanged.
The robustness commitment is met by registering the Open Corridor value-domain explicitly in
`OpenCorridorConstants.scala` and using a typed enum at all call sites — not by touching code
that already works correctly.

### New fields — only what's genuinely missing

| Change | Where | Notes |
|---|---|---|
| Add `snapshot_id`, `parent_settlement_id`, `cardano_tx_hash` to `MappedTransaction.scala` | Same | All nullable for back-compat. |

### New tables

| Change | Where | Notes |
|---|---|---|
| New table `open_corridor_snapshots` | New `MappedOpenCorridorSnapshot.scala` (probably under `code/open_corridor/snapshots/`) | LongKeyedMapper following existing OBP conventions |
| New table `open_corridor_settlement_policies` | New `MappedOpenCorridorSettlementPolicy.scala` | Same |
| New table `open_corridor_outbox` | New `MappedOpenCorridorOutbox.scala` | The atomicity-fix table — see "hard parts" below |

### Indexing

Composite index on `(transactionType, status, currency)` for the netting query.
Partial index `WHERE transactionType IN ('OPEN_CORRIDOR_PROMISE','OPEN_CORRIDOR_SETTLEMENT') AND status = 'PROMISED'`
for hot path on the open snapshot. Indexes on `snapshot_id` and
`parent_settlement_id` for snapshot drilldowns.

The `transactionType` filter is what makes the partial index safe — it scopes the
index to Open Corridor rows only, so non-Open Corridor transactions sharing the same `status` column are
never indexed by it.

All migrations additive + nullable + reversible. Lift's schemifier handles this
naturally; if OBP-API has manual migration scripts somewhere, follow that path.

## 2. Domain model + state machine

| Change | Where |
|---|---|
| `OpenCorridorConstants` object — well-known string values for existing fields | New under `code/open_corridor/model/`. Defines `AccountKind.OPEN_CORRIDOR_SETTLEMENT`, `TransactionType.OPEN_CORRIDOR_PROMISE`, `TransactionType.OPEN_CORRIDOR_SETTLEMENT`. No new column — these are constants used *with* existing `MappedBankAccount.kind` and `MappedTransaction.transactionType`. |
| `OpenCorridorStatus` enum + transition validator (`PROMISED` → `NETTED` → `SETTLED` / `EXCEPTION` / `REVERSED`) | Same; centralised so connector + netting engine call the same code. Values populate the existing `MappedTransaction.status` column (no new column). |
| `SnapshotStatus` enum + transitions | Same |
| Pure-function transition validator returning `Box[OpenCorridorStatus]` | OBP-API convention — Box-based error handling, not exceptions |

Audit-trail integration: every transition logged via OBP-API's existing audit
mechanism. Don't bypass it.

## 3. Connector trait extensions

`obp-api/src/main/scala/code/bankconnectors/Connector.scala` — the central trait
every connector implements. New methods needed:

| Method | Purpose |
|---|---|
| `createOpenCorridorPromise(...)` | Called from the modified TR flow when from-account is OPEN_CORRIDOR_SETTLEMENT. Returns the Promise transaction. |
| `getOpenCorridorPromises(bankId, accountId, status, ...)` | Backs the per-bank promises listing endpoint |
| `closeOpenCorridorSnapshot(currency)` | Backs the admin snapshot-close endpoint |
| `settleOpenCorridorSnapshot(snapshotId)` | Backs the admin settle endpoint |
| `getOpenCorridorSnapshots(filter)` + `getOpenCorridorSnapshot(id)` | Backs the snapshot listing/detail endpoints |
| `getOpenCorridorPositions(bankId, currency)` | Backs the per-bank positions endpoint |
| `createOpenCorridorSettlementAccount(bankId, currency)` | Provisioning helper; called during bank onboarding |

Each gets:
- An outbound DTO + inbound DTO under
  `obp-commons/src/main/scala/com/openbankproject/commons/dto/` matching the existing
  `OutBoundXxx` / `InBoundXxx` naming
- A `messageDocs +=` entry so it auto-publishes in the message-docs surface
- An implementation in **at minimum** the Mocked connector (for tests) and the
  RabbitMQ connector (for production). LocalMappedConnector might also be relevant
  if running Open Corridor without a separate adapter.

The MessageDoc entries are not optional — once banks integrate against the published
message spec, the wire format is locked. Get the DTOs right the first time.

## 4. RabbitMQ connector — two changes

`obp-api/src/main/scala/code/bankconnectors/rabbitmq/`

### a) Multi-tenant connection pool

Currently `RabbitMQConnectionPool.scala` holds one connection. Refactor to a pool
keyed by bank ID:

```scala
def borrowConnection(bankId: BankId): Connection
```

Each bank's connection points at its own vhost. Configuration source: a new
`open_corridor_bank_brokers` table (or props with bank-id-keyed entries) populated during bank
onboarding.

### b) Server-initiated RPC

OBP-API today is RPC *client* — it publishes requests, awaits replies. For Open Corridor
outbound messages (`obp_credit_notification`, `obp_netting_snapshot`,
`obp_settlement_instruction`, `obp_status_update`) it's the *server*. Symmetric
pattern:

- Pick the right bank's connection from the pool
- Publish to `{vhost}/obp_rpc_queue` with `messageId`, `correlationId`, `replyTo`
- `replyTo` is an OBP-API-side reply queue (per-request, ephemeral, mirroring what
  banks do today)
- Wait for the bank's reply on `replyTo`; correlate by `correlationId`
- Update the originating Transaction or snapshot row with the result

Both pieces touch `RabbitMQConnector_vOct2024.scala` and `RabbitMQUtils.scala`. The
change is structural — new methods aren't enough; the pool abstraction itself needs
revisiting.

## 5. Modify the existing TransactionRequest flow

`obp-api/src/main/scala/code/transactionrequests/MappedTransactionRequestProvider.scala`
and the service layer above it.

Branch in the create-TR path: **if `from_account.account_kind == OPEN_CORRIDOR_SETTLEMENT`**:

- Validate the inline routing resolves to a registered receiving bank's settlement
  account in the same currency
- Bypass SCA challenge generation — Open Corridor payments are pre-authenticated by the bank
- Call `connector.createOpenCorridorPromise(...)` instead of the regular transaction-creation
  code
- Return the standard OBP TR response shape but with `open_corridor_status=PROMISED`

The branch is small in lines but touches a load-bearing flow. It needs the same test
coverage as the existing path. Don't fork the codebase — share validation and
serialisation code, only diverge at the persistence step.

## 6. New REST endpoints

`obp-api/src/main/scala/code/api/v5_1_0/` (or whichever version package they target).

Six new endpoints from `LEDGER_DESIGN.md` §"API surface":

| Endpoint | Role required |
|---|---|
| `POST /obp/v5.1.0/open-corridor/snapshots/close?currency=X` | `CanCloseOpenCorridorSnapshot` (system-level) |
| `POST /obp/v5.1.0/open-corridor/snapshots/{snapshot_id}/settle` | `CanForceOpenCorridorSettle` (system-level) |
| `GET /obp/v5.1.0/open-corridor/snapshots[?currency=X&status=Y]` | `CanReadOpenCorridorSnapshot` (admin sees all; per-bank user sees own) |
| `GET /obp/v5.1.0/open-corridor/snapshots/{id}` | Same |
| `GET /obp/v5.1.0/banks/{bank_id}/open-corridor/positions[?currency=X]` | `CanReadOpenCorridorPosition` |
| `GET /obp/v5.1.0/banks/{bank_id}/open-corridor/promises[?status=...]` | `CanReadOpenCorridorPromise` |

OBP-API conventions: each endpoint defined as a `ResourceDoc` with example
request/response, OAS3 metadata, and connector method delegation. The existing
endpoint files have plenty of templates to follow.

## 7. New ApiRoles

`obp-api/src/main/scala/code/api/util/ApiRole.scala` — same file used for every
existing role. Add:

- `CanCreateOpenCorridorSettlementAccount`
- `CanCloseOpenCorridorSnapshot`
- `CanForceOpenCorridorSettle`
- `CanReadOpenCorridorPosition`
- `CanReadOpenCorridorPromise`
- `CanReadOpenCorridorSnapshot`

System-level vs bank-level distinction matters — the existing `requireBankLevelRole`
/ `requireSystemLevelRole` plumbing applies.

## 8. View defaults for Open Corridor settlement accounts

When a settlement account is created (`createOpenCorridorSettlementAccount` connector
method), the provisioning code also:

- Creates the standard `owner` View on it for the holding bank's user (reusing
  existing View creation)
- Does NOT create any public View
- Adds the platform admin user(s) to a `system_oc_settlement` View for cross-bank
  visibility

Uses the existing View / `AccountAccess` machinery — no new permission infrastructure.

## 9. Netting engine

New Akka actor under `obp-api/src/main/scala/code/actorsystem/open_corridor/`. OBP-API already
runs an actor system, so this slots in naturally.

- Scheduled actor per active currency, configured via policy
- On tick:
  1. `BEGIN TRANSACTION` (Mapper transaction or DB-level)
  2. Promote OPEN snapshot → CLOSED, mark covered Promises NETTED, create Settlement
     transactions in PENDING
  3. Insert outbox rows for: Cardano publish, RabbitMQ publish per bank pair,
     downstream credit notifications
  4. `COMMIT`
- Separate worker actor drains the outbox with retries (idempotent on outbox row id)

Outbox + worker is the **non-negotiable bit** for state-machine consistency.
Without it you get "Cardano written but DB not updated" or "RabbitMQ published but
settlement not posted" silent corruption.

## 10. Message Docs

Each new connector method gets a `messageDocs +=` entry in
`RabbitMQConnector_vOct2024.scala`. The format is fixed by existing entries:

```scala
messageDocs += MessageDoc(
  process = "obp_credit_notification",
  messageFormat = "rabbitmq_vOct2024",
  description = "...",
  outboundTopic = None,
  inboundTopic = None,
  exampleOutboundMessage = OutBoundCreditNotification(...),
  exampleInboundMessage = InBoundCreditNotification(...),
  adapterImplementation = Some(AdapterImplementation("- Open Corridor", 1))
)
```

All four new outbound messages plus the modified `obp_create_transaction_request`
(for the Open Corridor path) need these. They auto-publish at:

- `/obp/v2.2.0/message-docs/rabbitmq_vOct2024`
- `/obp/v3.1.0/message-docs/rabbitmq_vOct2024/swagger2.0`
- `/obp/v6.0.0/message-docs/rabbitmq_vOct2024/json-schema`

## 11. Configuration

`props` files (`default.props`, `default.props.template`):

- `open_corridor_enabled = true|false` (default false; gates new endpoints)
- `open_corridor_bank_broker.{bank_id}.host`, `.port`, `.vhost`, `.username`, `.password` per
  bank — or move this to a DB table during onboarding. **DB table is more
  self-service-friendly.**
- Settlement-system credentials per currency (Cardano: Ogmios URL of the
  bank's cardano-node or managed provider; CHAPS: rail creds; etc.) — DB
  table again, since they're bank-keyed
- Default cycle policy seed on first run (admin-overridable later)

## 12. Tests

OBP-API conventions: ScalaTest / Specs2. New suites:

- Unit tests for `OpenCorridorStatus` + `SnapshotStatus` state machine (every legal + illegal
  transition)
- Mapper-level CRUD for `MappedOpenCorridorSnapshot`, `MappedOpenCorridorSettlementPolicy`,
  `MappedOpenCorridorOutbox`
- Connector method tests against the Mocked connector for each new method
- Integration tests: end-to-end `PROMISED` → `NETTED` → `SETTLED` → credit notification,
  against an in-memory broker stub
- Property tests on netting maths: bilateral net of N gross ≡ direct compute;
  settlement always sums to zero across the bank pair

Per the robustness rule: not "a couple of happy-path tests." OBP-API existing modules
ship with comprehensive coverage; new modules match that bar.

## 13. Rollout / opt-in

- Schema migrations applied unconditionally (additive, nullable — no behaviour change
  without Open Corridor enabled)
- `open_corridor_enabled = false` by default → endpoints return 404, netting engine doesn't
  start, no observable change for existing deployments
- Per-bank onboarding: enable Open Corridor for that bank by inserting the broker config row +
  creating settlement accounts
- No legacy data to backfill (this is greenfield — no existing Open Corridor transactions exist)

## Hard parts called out specifically for OBP-API

| Concern | Why it bites in OBP-API specifically |
|---|---|
| **Outbox pattern is non-negotiable.** RabbitMQ publishes, Cardano references, balance updates: none can be in the same DB transaction. Outbox + retrying worker is the only correct pattern. | OBP-API doesn't have one today; banks have come to expect "the message arrived" guarantees. |
| **Multi-tenant connector refactor** is a bigger change than it sounds. | Current connector pool assumes one broker. Touching every outbound publish is a wide blast radius. Probably 1+ week of work alone. |
| **Connector message protocol = spec.** Once banks integrate against `rabbitmq_vOct2024`, messages are immutable in shape. | DTOs must be designed properly the first time. Argues for review by message-docs reviewers before merging. |
| **SCA bypass** must be auditable. | OBP-API's challenge flow is regulator-relevant. The Open Corridor bypass needs a clear audit-log entry per Promise saying "SCA suppressed because Open Corridor pre-auth." |
| **Performance** at >100k promises/cycle needs proper indexing + table partitioning by day. | Standard Postgres concerns; not a code change but a deployment requirement. |

## Rough sequence

The order to land changes in OBP-API:

1. Schema migrations (no behaviour) — ships immediately, low-risk
2. `OpenCorridorStatus` + `SnapshotStatus` state machine modules + unit tests
3. Mappers for the new tables
4. New ApiRoles
5. `createOpenCorridorPromise` connector method (Mocked + RabbitMQ implementations) + DTOs +
   MessageDoc
6. TR flow extension — branches on account_kind, calls createOpenCorridorPromise
7. Outbox table + worker actor (lays groundwork for atomic side effects)
8. `closeOpenCorridorSnapshot` + `settleOpenCorridorSnapshot` connector methods + admin endpoints
9. Settlement-system handler interface; Cardano stub handler first
10. New outbound RabbitMQ messages (`obp_credit_notification` etc.) wired through
    outbox worker
11. Multi-tenant connector pool refactor
12. Per-bank read endpoints (`positions`, `promises`)
13. Netting engine actor + scheduling + policies
14. View / settlement-account provisioning during bank onboarding
15. End-to-end integration tests across the whole flow

Steps 1–6 give you the Promise creation path working. 7 onwards turn it into the
netting platform.

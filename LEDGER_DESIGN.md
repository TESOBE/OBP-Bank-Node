# OBP-API Ledger Design — Open Corridor Netting via Full Double-Entry

How OBP-API would track inter-bank netting natively, using its existing
transaction model. Decision committed: this is the destination, not a
stepping stone.

OBP-API in this role is **the Open Corridor platform's books**, not any individual bank's
books. The bank's customer-level ledger lives in the bank's CBS. OBP-API is the
clearing-house ledger — every record represents an inter-bank obligation,
never a customer-level one.

## Existing OBP-API primitives we reuse

| Primitive | Role |
|---|---|
| `Bank` | Each participating bank exists as a Bank entity |
| `BankAccount` | Has balance, currency, holder bank — perfect shape for settlement accounts |
| `Transaction` | Debit/credit between two BankAccounts, with status, metadata, completed_at |
| `TransactionRequest` | Instruction → Transaction lifecycle, already supports SIMPLE with inline routing |
| `View` | Per-account permissions — owner sees everything, others see filtered/nothing |
| `Connector` (RabbitMQ + others) | The wire layer Bank Nodes already talk to OBP-API on |
| `MessageDocs` | Auto-generated docs for the RabbitMQ message catalogue |

`Counterparty` is intentionally NOT used for Open Corridor. Counterparty assumes pre-registered
corridors; Open Corridor corridors are implicit.

## New concepts

**1. Settlement Account (account kind tag).**
A new `account_kind` field on BankAccount. Most accounts have `account_kind = NULL`
(regular customer/business). Settlement accounts have `account_kind = "OPEN_CORRIDOR_SETTLEMENT"`.
One per (bank, currency) for banks participating in Open Corridor.

**2. Promise transaction (transaction kind tag).**
A new `transaction_kind` field on Transaction. Regular transactions have NULL.
Inter-bank obligation entries have `transaction_kind = "OPEN_CORRIDOR_PROMISE"`. They debit one
settlement account and credit another, but **don't post to balance until settlement
clears**.

**3. Settlement transaction.**
Same Transaction shape, `transaction_kind = "OPEN_CORRIDOR_SETTLEMENT"`. Created by the netting
engine. **Posts to balance** when committed. Each one covers N Promise transactions
in a snapshot.

**4. Netting Snapshot.**
New entity. Groups Promise transactions and the resulting Settlement transactions
into one cycle. Has its own lifecycle (OPEN → CLOSED → SETTLING → SETTLED).

**5. Settlement Policy.**
Configuration entity. Says how snapshots close: time-based interval, per currency,
bilateral vs multilateral, settlement system to use (Cardano / CHAPS / NIBSS).

## State machine

The heart of the design.

### Promise transaction status (`open_corridor_status`)

```
                           ┌──────────────────┐
                           ▼                  │
PROMISED ──netting cycle──> NETTED ──settle──> SETTLED
   │                          │
   │                          └──fail──> EXCEPTION
   │
   └──reverse──> REVERSED
```

| State | Balance impact | Meaning |
|---|---|---|
| `PROMISED` | none (pending) | Created via Interface B; Cardano Record 1 written. Not yet in a snapshot. |
| `NETTED` | none (pending) | Picked up by a snapshot. Net being computed. Awaiting settlement. |
| `SETTLED` | covered by parent Settlement | Settlement transaction has posted; net is reflected in real balances. |
| `EXCEPTION` | none | Couldn't be settled (timeout, dispute). Cardano Record 4 written. Stays on books. |
| `REVERSED` | offset by reversal txn | A subsequent reversal transaction nets it out. The original row stays for audit. |

### Settlement transaction status (separate from Promise)

```
PENDING ──posted──> SETTLED
   │
   └──fail──> FAILED
```

| State | Balance impact | Meaning |
|---|---|---|
| `PENDING` | none | Created at snapshot close; awaiting on-chain confirmation (Cardano Record 5) or fiat-rail confirmation |
| `SETTLED` | applied | Posted to settlement account balances |
| `FAILED` | none | Settlement failed; underlying Promises stay NETTED for retry / manual intervention |

### Snapshot status

```
OPEN ──close──> CLOSED ──post settlements──> SETTLING ──confirm──> SETTLED
                  │
                  └──any settlement fails──> EXCEPTION
```

The whole thing is finite-state and inspectable. You can always answer
"why is this Promise still pending?" by walking up to its snapshot's status.

## Schema additions

All additive. Existing OBP deployments that don't enable Open Corridor see no behavioural
change — new columns are NULL, new tables sit empty.

```sql
-- Existing tables, extended
ALTER TABLE bank_accounts
    ADD COLUMN account_kind VARCHAR;       -- NULL | 'OPEN_CORRIDOR_SETTLEMENT'

ALTER TABLE transactions
    ADD COLUMN transaction_kind VARCHAR,    -- NULL | 'OPEN_CORRIDOR_PROMISE' | 'OPEN_CORRIDOR_SETTLEMENT'
    ADD COLUMN open_corridor_status VARCHAR,           -- only set when transaction_kind IS NOT NULL
    ADD COLUMN snapshot_id UUID REFERENCES open_corridor_snapshots,
    ADD COLUMN parent_settlement_id UUID,   -- on a Promise: which Settlement covered it
    ADD COLUMN cardano_tx_hash VARCHAR;     -- Promise: Record 1 hash; Settlement: Record 5 hash

-- New tables
CREATE TABLE open_corridor_snapshots (
    snapshot_id  UUID PRIMARY KEY,
    currency     CHAR(3) NOT NULL,
    status       VARCHAR NOT NULL,                  -- OPEN | CLOSED | SETTLING | SETTLED | EXCEPTION
    policy_id    UUID REFERENCES open_corridor_settlement_policies,
    opened_at    TIMESTAMPTZ NOT NULL,
    closed_at    TIMESTAMPTZ,
    settled_at   TIMESTAMPTZ,
    cardano_snapshot_tx VARCHAR
);

CREATE TABLE open_corridor_settlement_policies (
    policy_id        UUID PRIMARY KEY,
    name             VARCHAR NOT NULL,
    currency         CHAR(3),                       -- NULL = applies to all
    cycle_kind       VARCHAR NOT NULL,              -- TIME_BASED | VOLUME_BASED | MANUAL
    cycle_interval   INTERVAL,
    netting_kind     VARCHAR NOT NULL DEFAULT 'BILATERAL',
    settlement_system VARCHAR NOT NULL              -- CARDANO | CHAPS | NIBSS | …
);
```

## API surface

**One extended endpoint, six new ones, message-doc additions.**

### 1. Existing TR endpoint — extended, not replaced

`POST /banks/{bank_id}/accounts/{account_id}/views/owner/transaction-request-types/SIMPLE/transaction-requests`

The from-account being `OPEN_CORRIDOR_SETTLEMENT` triggers the Promise path:
- Resolve `to.otherBank...` routing to a receiving bank's settlement account ID
- Create a Transaction with `transaction_kind=OPEN_CORRIDOR_PROMISE`, `open_corridor_status=PROMISED`
- Don't post to balance
- Carry the customer-level routing in transaction metadata
- Return the standard OBP TR response with the new open_corridor_status value
- (The Bank Node's Cardano Promise write happens after the response; the Bank Node
  updates the Transaction's `cardano_tx_hash` via a follow-up call.)

Same URL, same body, conditioned behaviour.

### 2. New: snapshot close (admin)

`POST /obp/v5.1.0/open-corridor/snapshots/close?currency=KES`

Closes the OPEN snapshot for a currency, computes nets, creates Settlement
transactions in PENDING, marks covered Promises NETTED. Returns the snapshot
detail. Idempotent — if no OPEN snapshot, returns the most recent CLOSED one.

### 3. New: snapshot settle (admin)

`POST /obp/v5.1.0/open-corridor/snapshots/{snapshot_id}/settle`

Marks the snapshot SETTLING, expects on-chain / fiat-rail confirmation to come
back via callbacks/connectors and flip Settlement transactions to SETTLED. Optional
manual override flag.

### 4. New: snapshot listing (admin + per-bank read)

`GET /obp/v5.1.0/open-corridor/snapshots[?currency=X&status=Y]`
`GET /obp/v5.1.0/open-corridor/snapshots/{id}`

Per-bank views show only snapshots covering Promises that involve them.

### 5. New: net positions per bank (per-bank)

`GET /obp/v5.1.0/banks/{bank_id}/open-corridor/positions[?currency=X]`

Live bilateral net positions for this bank against every other bank, computed as:
```
(SETTLED-side balance delta from Settlement transactions)
+ (PROMISED|NETTED-side aggregate from Promise transactions)
```

Convenience — derivable from existing `/transactions` and `/balance`, but a
one-shot answer matters for dashboards.

### 6. New: promise listing (per-bank)

`GET /obp/v5.1.0/banks/{bank_id}/open-corridor/promises[?status=...]`

Filtered listing of Promise transactions by status, for the bank's own settlement
account.

### 7. Message-doc additions

New entries in OBP-API's existing message-doc generator for:
- `obp_credit_notification`
- `obp_netting_snapshot`
- `obp_settlement_instruction`
- `obp_status_update`

These slot into `/obp/v6.0.0/message-docs/rabbitmq_vOct2024/json-schema` alongside
everything else.

## The netting engine

A new background component inside OBP-API (or alongside, talking via OBP-API's
admin endpoints). Single-purpose, runs per-currency:

```
Loop forever (per currency, per policy):
    sleep until next cycle boundary

    open_snapshot = get_open_snapshot(currency)
    if no open snapshot, create one

    promises = SELECT * FROM transactions
               WHERE transaction_kind = 'OPEN_CORRIDOR_PROMISE'
                 AND open_corridor_status = 'PROMISED'
                 AND currency = $currency
                 AND created_at >= open_snapshot.opened_at

    if promises is empty: continue

    BEGIN TRANSACTION
        update open_snapshot.status = 'CLOSED', closed_at = now()
        update each promise: open_corridor_status = 'NETTED', snapshot_id = open_snapshot.id

        # Bilateral net: per (from_bank, to_bank) pair, compute net
        nets = aggregate(promises) by (from_bank, to_bank)
        for each (from_bank, to_bank, net_amount):
            create Settlement transaction:
                from = from_bank's settlement account
                to   = to_bank's settlement account
                amount = abs(net_amount)
                transaction_kind = 'OPEN_CORRIDOR_SETTLEMENT'
                open_corridor_status = 'PENDING'
                snapshot_id = open_snapshot.id
            update covered promises: parent_settlement_id = settlement.id
    COMMIT

    publish obp_netting_snapshot to all involved banks' vhosts

    open_snapshot.status = 'SETTLING'

    for each Settlement transaction in the snapshot:
        invoke settlement_system handler (Cardano / CHAPS / NIBSS)
            on success: open_corridor_status = 'SETTLED', balance_post()
            on failure: open_corridor_status = 'FAILED', mark_snapshot_exception()

    if all SETTLED:
        snapshot.status = 'SETTLED'
        for each Promise in snapshot: open_corridor_status = 'SETTLED'
        for each Promise in snapshot:
            publish obp_credit_notification to receiving bank's vhost
```

Three things to call out:
- The DB transaction around snapshot close + settlement creation is **the**
  correctness boundary. Either everything happens or nothing does.
- Settlement system handlers are pluggable per currency (Cardano for KES, CHAPS for
  GBP, NIBSS for NGN). The `settlement_system` field on the policy decides which.
- Credit notifications fire at the *end*, after settlement clears. Not at promise
  creation. The bank's CBS shouldn't credit a customer until the funds are actually
  settled — that's the entire point of the netting cycle.

## Connector implications

OBP-API's RabbitMQ connector currently handles messages like `obp_create_transaction_request`.
With Open Corridor enabled:

- `obp_create_transaction_request` works as today, but if the from-account is
  OPEN_CORRIDOR_SETTLEMENT the connector path includes Promise creation + Cardano hash storage
- New outbound messages from OBP-API: `obp_credit_notification`, `obp_netting_snapshot`,
  `obp_settlement_instruction`, `obp_status_update` — all server-initiated RPC.
  AMQP-wise they look the same as existing requests: published to the bank's vhost's
  `obp_rpc_queue`, with a `replyTo` queue OBP-API listens on for the bank's ack.
- The connector needs a per-bank dispatcher (the multi-tenant work) — for each
  outbound message, route it to the right vhost based on the target bank's registered
  broker connection.

## Permissions / view setup

OBP-API's existing View model handles this without new infrastructure:

- Bank A's settlement-KES account has a default View like `owner` (full read/write
  for the bank's own user)
- Other banks have no view of A's settlement account (they can't query it)
- OBP-API admin user (TESOBE operator) has a special "platform" view across all
  settlement accounts

When a new bank is provisioned, the workflow includes creating settlement accounts
and assigning the bank's user as owner-view holder. That's plain OBP-API account/view
setup, not new code.

## Things that aren't code

Decisions that need humans, not implementation:

| Question | Why it matters |
|---|---|
| Cycle period per currency | Drives policy.cycle_interval. Likely shorter for high-volume currencies. |
| Pre-funding required, or credit lines allowed? | Affects PROMISED-creation validation. |
| Bilateral vs multilateral netting | Two different aggregation algorithms; both fit the schema. |
| Cross-currency corridors → FX rates | An FX engine needs wiring in if KES → USD is allowed. |
| Reversal authority + window | Real banking allows reversals; policy must define who and when. |
| Settlement system per currency | Cardano for KES? CHAPS for GBP? NIBSS for NGN? |
| Snapshot retention / archiving | How long do SETTLED snapshots stay queryable? Regulator answer. |
| Dispute / EXCEPTION resolution flow | How does an EXCEPTION get cleared? |

## The hard parts

**1. State-machine consistency under partial failure.** What if snapshot close
succeeds but settlement creation fails midway? What if Cardano publishes succeed but
DB update fails? What if RabbitMQ publish succeeds but the receiving bank's CBS
rejects? Every state transition needs an idempotent retry path or a manual cleanup
path.

**2. The connector boundary.** The netting engine needs to atomically (a) update
DB, (b) publish RabbitMQ messages, (c) write Cardano. None of these three is in the
same transaction. Standard answer: outbox pattern at the engine — write all the
to-do work into an outbox table within the DB transaction, then a worker drains
the outbox to RabbitMQ/Cardano with retries. The OBP Bank Node already has this
pattern; OBP-API would need its own.

**3. Performance.** Netting `SUM(amount) GROUP BY (from_bank, to_bank)` over
millions of Promise rows per cycle is fine on Postgres if indexed properly, but
you'll want a partial index `WHERE open_corridor_status='PROMISED'` and probably
partition-by-day on the transactions table.

## Implementation order

1. Schema migrations (additive; nullable; safe to deploy without behaviour changes)
2. Account kind + Promise transaction creation in the existing TR flow
   (no netting yet — Promises just accumulate)
3. The Open Corridor-status state machine + exception state
4. Snapshot entity + close logic (manual trigger first)
5. Settlement transaction creation + balance posting
6. Settlement-system handlers (Cardano, then CHAPS/NIBSS as needed)
7. RabbitMQ outbound messages (`obp_netting_snapshot`, `obp_credit_notification`, etc.)
8. Bank-facing read endpoints (positions, promises listing)
9. Per-bank vhost connector (the multi-tenant work feeds in here)
10. Netting engine background scheduler with policies
11. Reversal / exception flows
12. Admin operations UI (snapshot close, force-settle, exception resolution)

Steps 1–5 give you a working manual-only pipeline. Steps 6–10 turn it into a real
automated platform. Steps 11–12 productionise it.

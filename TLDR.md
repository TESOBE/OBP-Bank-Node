## What it is

A small Go service a bank runs inside its own network. It connects the bank's Core Banking System (CBS) to the cross-border payment network (an OBP API instance hosted by TESOBE) — without the bank needing to run any OBP infrastructure itself.

The bank only deals with one thing locally: a REST endpoint on `localhost`. Everything else — talking to the OBP API, writing to the Cardano blockchain, listening for instructions over RabbitMQ — happens behind that.

## Four interfaces

```
                   Bank CBS
                      ↕  Interface A — local REST (this is all the bank touches)
                OBP Bank Node
                  ↕   ↕   ↕
         Interface B  C   D
            ↓       ↑     ↕
         OBP API  RabbitMQ  Cardano
```

- **A — South** (bank ↔ node): a localhost REST endpoint shaped exactly like an OBP Transaction Request, plus an outgoing webhook (or DB write, or file drop) for inbound credits.
- **B — North outbound** (node → OBP API): submits the payment as an OBP Transaction Request via OAuth2.
- **C — North inbound** (OBP API → node): RabbitMQ consumer for credit notifications, netting snapshots, settlement instructions, status updates.
- **D — Blockchain** (node ↔ Cardano): writes Promise records, settlement references, and exception markers to a local `cardano-node` via Ogmios (JSON-RPC over WebSocket).

## The two flows

**Outbound payment.** Customer initiates a cross-border payment. The bank's CBS POSTs to `localhost:8088/obp-bank-node/v5.1.0/banks/{bank_id}/accounts/{account_id}/views/{view_id}/transaction-request-types/SIMPLE/transaction-requests` with amount, currency, description, and inline beneficiary routing (BIC+IBAN, MOBILE_PHONE, OBP, etc.). The node:
1. Persists the request to a local SQLite outbox (durability before any external call).
2. Resolves the routing to an OBP API counterparty.
3. Submits the Transaction Request to the OBP API.
4. Writes a Promise record to Cardano.
5. Returns 202 with a `transaction_request_id` — synchronously, in milliseconds.

If the OBP API is unreachable, the request stays in the outbox and replays on reconnection — the bank still gets its 202.

**Inbound credit.** A payment from another bank arrives for one of this bank's customers. The OBP API publishes a `obp.credit.notification` to the bank's RabbitMQ queue. The node delivers the credit instruction to the CBS via whichever of four modes the bank chose at config time:
1. **REST webhook (OBP format)** — recommended.
2. **REST webhook (ISO 20022 / camt.054 JSON)** — for teams that prefer the standard.
3. **Database write** — node inserts into a staging table the CBS polls. Zero CBS development.
4. **File drop** — JSON or CSV file in a watched directory. Works with any CBS.

Failed deliveries retry with backoff for 24 hours; after that the node writes a Cardano Exception record and alerts the operator.

## What makes it useful

- **No new API for the bank to learn.** Local endpoint is byte-for-byte an OBP Transaction Request.
- **No counterparty pre-registration.** SIMPLE type — beneficiary routing is inline; corridors get created implicitly.
- **No SCA in the path.** Bank handles SCA before calling; node never challenges.
- **Resilient on bad networks.** Local SQLite outbox absorbs platform outages — important for African deployment context.
- **Auditable.** Every payment leaves a chain of Cardano records (Promise → Netting Snapshot → Settlement Reference) that compliance can verify independently.
- **Single binary, ~20MB Docker image, <50MB RAM.** Sits comfortably alongside legacy CBS hardware.

## Where it stands today

The skeleton implements the south-side REST surface end-to-end (payment init, status query, list, health, version routing across `/obp-bank-node/v5.0.0` and `/obp-bank-node/v5.1.0`), the SQLite outbox, all four delivery modes, and the AGPLv3 license / copyright headers. The OBP API client, RabbitMQ consumer, and Cardano writer are behind interfaces with stub implementations — they log realistic responses and assign deterministic IDs so the system runs end-to-end without external dependencies. Real implementations slot in without touching any callers.

# Integrating Your Bank with Open Corridor.

# The OBP Bank Node

Everything your bank's Core Banking System (CBS) needs to do to send and
receive (cross-border) payments through the Open Corridor network. The Bank Node
is a single binary you run inside your own network. Your CBS only ever talks to
it over `local-node`.

## The shape of the integration

```
        Your CBS
           ↕   Local REST (you call this for outbound payments,
           ↕   the Bank Node calls you back for inbound credits)
     OBP Bank Node
       ↕   ↕   ↕
    OBP API  RabbitMQ  Cardano
```

OBP API and RabbitMQ credentials are provisioned by TESOBE at registration.
The Cardano node is yours to operate — either co-located with the Bank Node
or via a managed provider (Demeter.run, etc.).

Four things on your side:

1. **Call** one REST endpoint when a customer initiates a cross-border payment.
2. **Receive** credit notifications via one of four delivery modes — your choice.
3. **Operate** a Cardano node (or use a managed provider like Demeter.run).
4. **Fill in** one YAML config file.

---

## 1. Outbound payments — what your CBS calls

### Initiate a payment

```
POST http://local-node:8088/obp-bank-node/v5.1.0/transaction-requests
Authorization: Bearer <local_secret>
Content-Type: application/json
```

Request body:

```json
{
  "value":       { "currency": "KES", "amount": "1500.00" },
  "description": "Invoice 4471",
  "to": {
    "other_bank_routing_scheme":     "BIC",
    "other_bank_routing_address":    "KCBLKENXXXX",
    "other_account_routing_scheme":  "IBAN",
    "other_account_routing_address": "GB29NWBK60161331926819"
  },
  "originator": {
    "name":    "Acme Coffee Ltd",
    "address": "12 Market Street, Nairobi, Kenya",
    "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" }
  }
}
```

Response — `202 Accepted`:

```json
{
  "transaction_request_id": "tr-abc-123",
  "type":                   "OPEN_CORRIDOR",
  "from": { "bank_id": "ke.01.kcs", "account_id": "…" },
  "to": {
    "other_bank_routing_scheme":     "BIC",
    "other_bank_routing_address":    "KCBLKENXXXX",
    "other_account_routing_scheme":  "IBAN",
    "other_account_routing_address": "GB29NWBK60161331926819"
  },
  "originator": {
    "name":    "Acme Coffee Ltd",
    "address": "12 Market Street, Nairobi, Kenya",
    "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" },
    "source":  "explicit"
  },
  "value":         { "currency": "KES", "amount": "1500.00" },
  "description":   "Invoice 4471",
  "status":        "PROMISE_WRITTEN",
  "promise_id":         "<cardano tx hash>",
  "promise_blockchain": "Cardano",
  "start_date":         "2026-05-09T10:30:00Z"
}
```

You get a synchronous `202` in milliseconds. The Bank Node persists the request
locally before any external call, so a 202 is durable even if the network
drops immediately afterwards.

The four `to.*` routing fields are inline — no counterparty pre-registration.
Use whatever scheme fits your destination (`BIC`+`IBAN`, `MOBILE_PHONE`, `OBP`,
etc.).

### Query a payment

```
GET  http://local-node:8088/obp-bank-node/v5.1.0/transaction-requests/{transaction_request_id}
Authorization: Bearer <local_secret>
```

Returns the same body as the initial response, with a current `status` and any
`netting_snapshot_id` / `settlement_id` once the payment has settled.

Status values:
`INITIATED` → `SUBMITTED` → `PROMISE_WRITTEN` → `PENDING_NETTING` →
`PENDING_SETTLEMENT` → `COMPLETED`. Terminal failure: `EXCEPTION`.

### List recent payments

```
GET  http://local-node:8088/obp-bank-node/v5.1.0/transaction-requests
Authorization: Bearer <local_secret>
```

### Health (no auth)

```
GET  http://local-node:8088/health
```

Returns liveness, RabbitMQ connection state, and OBP API / Cardano backend
status.

### Errors

| Code                         | Meaning                                                 |
| ---------------------------- | ------------------------------------------------------- |
| `OBP-10001`                  | Malformed JSON or missing required field                |
| `OBP-40008`                  | Amount is zero or negative                              |
| `OBP-BANK-NODE-AUTH-001`     | Missing / invalid `Authorization` header                |
| `OBP-BANK-NODE-ROUTING-001`  | Routing address could not be resolved                   |
| `OBP-BANK-NODE-PLATFORM-001` | OBP API unreachable; instruction queued in local outbox |

---

## 2. Inbound credits — what your CBS exposes

Pick **one** delivery mode. The Bank Node delivers every incoming credit in
that mode; you don't switch per-payment.

The payload is the same in every mode:

```json
{
  "transaction_request_id": "tr-abc-123",
  "netting_snapshot_id":    "snap-…",
  "from": { "bank_id": "…", "bank_routing": { "scheme": "BIC", "address": "…" } },
  "to":   { "bank_id": "…", "account_id": "…",
            "account_routing": { "scheme": "IBAN", "address": "…" } },
  "originator": {
    "name":    "Acme Coffee Ltd",
    "address": "12 Market Street, Nairobi, Kenya",
    "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" }
  },
  "value":              { "currency": "KES", "amount": "1500.00" },
  "description":        "Invoice 4471",
  "value_date":         "2026-05-09",
  "promise_id":         "<cardano tx hash>",
  "promise_blockchain": "Cardano"
}
```

### Mode 1 — `webhook_obp` (recommended)

You expose one HTTPS endpoint. The Bank Node POSTs each credit to it.

```
POST <your CBS URL>
Authorization: Bearer <shared secret>
Content-Type: application/json
<credit payload above>
```

Respond `200 OK`:

```json
{ "status": "ACCEPTED", "cbs_reference": "<your bank-side reference>" }
```

Anything else, or a timeout, triggers retry with backoff for 24 hours.

### Mode 2 — `webhook_iso20022`

Same as Mode 1 but the body is an ISO 20022 **camt.054** JSON envelope. Pick
this if your CBS already speaks ISO 20022.

### Mode 3 — `database`

You expose a PostgreSQL staging table; the Bank Node `INSERT`s into it. Your
CBS:

1. Polls rows where `status = 'PENDING'`
2. Posts the credit on its own books
3. Updates the row to `status = 'PROCESSED'` with a non-null `cbs_reference`
   and a `processed_at` timestamp

Schema (auto-created by the Bank Node on first connection):

```sql
CREATE TABLE obp_credit_instructions (
    transaction_request_id  VARCHAR PRIMARY KEY,
    netting_snapshot_id     VARCHAR,
    to_account_id           VARCHAR NOT NULL,
    currency                CHAR(3) NOT NULL,
    amount                  NUMERIC(18,5) NOT NULL,
    description             VARCHAR,
    value_date              DATE,
    promise_id              VARCHAR,
    promise_blockchain      VARCHAR,
    status                  VARCHAR NOT NULL DEFAULT 'PENDING',
    cbs_reference           VARCHAR,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at            TIMESTAMPTZ
);
```

Zero CBS HTTP development. Good fit if your CBS already has a polling worker.

### Mode 4 — `file`

You expose two shared directories. The Bank Node writes
`{transaction_request_id}.json` (or `.csv`) into the inbound directory. Your
CBS picks it up, processes it, and writes an acknowledgement file into the
acknowledgement directory.

Works with any CBS — no DB or HTTP integration required.

---

## 3. Config you fill in (`obp-bank-node-config.yaml`)

Three groups: identity (TESOBE provides at registration), CBS choice (you pick
one mode), local operations.

### TESOBE provides at registration

```yaml
bank:
  bank_id:    "ke.01.kcs"                            # your bank ID
  account_id: "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1" # your settlement account
  view_id:    "owner"

obp_api:
  base_url:               "https://obp-api.example.com"
  oauth2_consumer_key:    "..."
  oauth2_consumer_secret: "..."
  oauth2_access_token:    "..."
  oauth2_token_secret:    "..."

rabbitmq:
  host:          "rmq.openbankproject.com"
  port:          5672
  username:      "..."
  password:      "..."
  virtual_host:  "/bank.ke.01.kcs"
  request_queue: "obp_rpc_queue"
```

### You provide

```yaml
obp_bank_node:
  port:          8088
  local_secret:  "<rotate on first run; this is the Bearer your CBS sends>"

blockchain:
  type: "cardano"
  cardano:
    network:              "preprod"                  # TESOBE tells you: preprod (staging) or mainnet (production)
    ogmios_url:           "ws://localhost:1337"      # Ogmios in front of your cardano-node
    wallet_address_path:  "./secrets/cardano.addr"
    wallet_vkey_path:     "./secrets/cardano.vkey"
    wallet_skey_path:     "./secrets/cardano.skey"   # never leaves your host

cbs_delivery:
  mode: "webhook_obp"        # webhook_obp | webhook_iso20022 | database | file

  webhook:                                 # modes 1 & 2
    url:             "https://cbs.bank.local/api/credit-notifications"
    timeout_seconds: 30

  database:                                # mode 3
    driver:   "postgresql"
    host:     "cbs-db.bank.local"
    port:     5432
    name:     "cbs_staging"
    username: "obp_writer"
    password: "..."
    table:    "obp_credit_instructions"

  file:                                    # mode 4
    drop_path:            "/shared/credit-notifications/inbound/"
    acknowledgement_path: "/shared/credit-notifications/acknowledged/"
    format:               "json"           # json | csv
```

### Local operations

```yaml
telemetry:
  type:       "prometheus"
  port:       9090
  log_level:  "INFO"

dashboard:
  enabled:      false
  bind_address: "127.0.0.1"
  port:         8081
  auth:
    enabled:   true
    username:  "admin"
    password:  "<change me>"

outbox:
  path: "./outbox/obp-bank-node.db"
```

---

## 4. Verifying the install

Start the Bank Node, then:

```bash
curl -s http://local-node:8088/health | jq
```

`rabbitmq` should report `connected`. Send a test payment:

```bash
curl -X POST http://local-node:8088/obp-bank-node/v5.1.0/transaction-requests \
  -H "Authorization: Bearer <local_secret>" \
  -H "Content-Type: application/json" \
  -d '{
    "value": { "currency": "KES", "amount": "100.00" },
    "description": "test",
    "to": {
      "other_bank_routing_scheme":     "BIC",
      "other_bank_routing_address":    "KCBLKENXXXX",
      "other_account_routing_scheme":  "IBAN",
      "other_account_routing_address": "GB29NWBK60161331926819"
    },
    "originator": {
      "name":    "Acme Coffee Ltd",
      "address": "12 Market Street, Nairobi, Kenya",
      "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" }
    }
  }'
```

You should get a `202` with a `transaction_request_id`. Query it back:

```bash
curl -H "Authorization: Bearer <local_secret>" \
  http://local-node:8088/obp-bank-node/v5.1.0/transaction-requests/<transaction_request_id>
```

That's the entire bank-side surface area: one outbound REST call, one inbound
integration of your choosing, one YAML file.

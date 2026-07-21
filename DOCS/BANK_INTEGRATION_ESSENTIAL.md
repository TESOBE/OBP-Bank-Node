# Integrating Your Bank with Open Corridor.

# The OBP Bank Node

Everything your bank's Core Banking System (CBS) needs to do to send and
receive (cross-border) payments through the Open Corridor network. The Bank Node
is a single service you run inside your own network, delivered as a container
image built per Cardano network (`obp-bank-node:preprod` for staging,
`obp-bank-node:mainnet` for production) and run under Podman or Docker. Your
CBS only ever talks to it over `local-node`.

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
The blockchain component that writes the audit trail ships with the Bank Node
installation package (a bundled cardano-node + Ogmios container) — it starts
alongside the Bank Node and needs no separate setup or blockchain expertise.
The Cardano network is a property of the images, not of your configuration:
TESOBE tells you which tag to deploy (`preprod` for staging, `mainnet` for
production), and both containers come tag-matched to that network. Banks that
already use a managed provider (Demeter.run, etc.) can point the config at
that instead.

Five steps, in order:

1. **Provision** a host that can run containers (Podman or Docker).
2. **Fill in** one YAML config file.
3. **Deploy** the two containers and safeguard the signing key generated at
   installation (one `.skey` file) — it never leaves your host.
4. **Call** one REST endpoint when a customer initiates a cross-border payment.
5. **Receive** credit notifications via a webhook your CBS exposes.

---

## 1. Provision a host

One machine (VM or physical) inside your network:

- Podman 4+ (or Docker) with Compose support, linux/amd64.
- Outbound network access: HTTPS to the OBP API, AMQP (port 5672) to
  `rmq.openbankproject.com`, and Cardano peer-to-peer traffic (outbound TCP)
  for the bundled node.
- Inbound access only from your CBS network to port 8088. Nothing on this
  host needs to be reachable from the internet.

The Bank Node itself is small — one Rust binary and a local SQLite outbox,
comfortably under one core and 512 MB RAM. Sizing is driven entirely by the
bundled Cardano container:

| Deployment                               | CPU | RAM   | SSD    |
| ---------------------------------------- | --- | ----- | ------ |
| preprod (staging)                        | 2   | 8 GB  | 80 GB  |
| mainnet (production)                     | 4   | 16 GB | 250 GB |
| managed provider (no bundled container)  | 1   | 1 GB  | 10 GB  |

The disk holds the Cardano chain DB: 30–50 GB on preprod, on the order of
200 GB on mainnet, both growing slowly. The initial sync from genesis is
CPU-bound; extra cores speed it up but sit idle afterwards. Banks pointing
`ogmios_url` at a managed provider run no Cardano container at all and need
only the last row.

---

## 2. Fill in the config (`obp-bank-node-config.yaml`)

Three groups: identity (TESOBE provides at registration), CBS choice, local
operations.

### TESOBE provides at registration

```yaml
bank:
  bank_id:    "ke.01.kcs"                            # your bank ID
  account_id: "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1" # your settlement account
  view_id:    "owner"

obp_api:
  base_url:      "https://obp-api.example.com"
  # OAuth2 client-credentials (machine-to-machine).
  token_url:     "https://obp-api.example.com/oauth2/token"
  client_id:     "..."
  client_secret: "..."
  # scope:       "..."          # optional
  # Alternative: a pre-obtained DirectLogin token.
  # direct_login_token: "..."

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
  # That is the whole blockchain section. The Cardano network is fixed by the
  # image you deploy (obp-bank-node:preprod or obp-bank-node:mainnet), never
  # by this file — the node refuses to start if a network is named here that
  # differs from the image. The signing key generated at installation
  # (secrets/cardano.skey) is picked up automatically and never leaves your
  # host; the wallet address is derived from it. Banks using a managed
  # provider instead of the bundled container add:
  # cardano:
  #   ogmios_url: "wss://<provider endpoint>"

cbs_delivery:
  mode: "webhook_obp"

  webhook:
    url:             "https://cbs.bank.local/api/credit-notifications"
    timeout_seconds: 30
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

## 3. Deploy the containers

The installation package contains a compose file that runs the Bank Node and
its Cardano container together:

```bash
CARDANO_NETWORK=preprod podman compose -f docker/docker-compose.bank.yml up -d   # staging
CARDANO_NETWORK=mainnet podman compose -f docker/docker-compose.bank.yml up -d   # production
```

(`docker compose` works identically if you run Docker instead of Podman.)

`CARDANO_NETWORK` selects tag-matched images for both containers, so the Bank
Node and its Cardano node are always on the same network. Your YAML config and
signing key are mounted read-only (`/app/obp-bank-node-config.yaml`,
`/app/secrets/cardano.skey`); the key must be readable by the container user,
uid 10001 (`chmod 400 secrets/cardano.skey`, then
`podman unshare chown 10001 secrets/cardano.skey` for rootless Podman, or a
plain `chown 10001` for Docker / rootful Podman). The Bank Node refuses to
start if the YAML names a network that differs from the image.

On first start the Cardano container syncs the chain from genesis (a few
hours on preprod, longer on mainnet); the Bank Node waits for it to report
healthy. Check the node:

```bash
curl -s http://local-node:8088/health | jq
```

Returns liveness, RabbitMQ connection state, and OBP API / Cardano backend
status. `rabbitmq` should report `connected`.

**Safeguard `secrets/cardano.skey`.** It is the key the Bank Node signs
blockchain records with, it is generated at installation, and it never leaves
this host. Back it up like any other production secret; losing it means
re-registering a new key with TESOBE.

---

## 4. Outbound payments — what your CBS calls

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

### Errors

| Code                         | Meaning                                                 |
| ---------------------------- | ------------------------------------------------------- |
| `OBP-10001`                  | Malformed JSON or missing required field                |
| `OBP-40008`                  | Amount is zero or negative                              |
| `OBP-BANK-NODE-AUTH-001`     | Missing / invalid `Authorization` header                |
| `OBP-BANK-NODE-ROUTING-001`  | Routing address could not be resolved                   |
| `OBP-BANK-NODE-PLATFORM-001` | OBP API unreachable; instruction queued in local outbox |

---

## 5. Inbound credits — `webhook_obp` (recommended)

You expose one HTTPS endpoint. The Bank Node POSTs each credit to it.

```
POST <your CBS URL>
Authorization: Bearer <shared secret>
Content-Type: application/json
```

Payload:

```json
{
  "transaction_request_id": "tr-abc-123",
  "from": { "bank_id": "…", "bank_routing": { "scheme": "BIC", "address": "…" } },
  "to":   { "bank_id": "…", "account_id": "…",
            "account_routing": { "scheme": "IBAN", "address": "…" } },
  "originator": {
    "name":    "Acme Coffee Ltd",
    "address": "12 Market Street, Nairobi, Kenya",
    "account_routing": { "scheme": "IBAN", "address": "KE12KCBL0000009876543210" }
  },
  "value":         { "currency": "KES", "amount": "1500.00" },
  "description":   "Invoice 4471",
  "value_date":    "2026-05-09"
}
```

The Bank Node also includes optional metadata fields (`netting_snapshot_id`,
`promise_id`, `promise_blockchain`) for audit and reconciliation. Your CBS can
ignore or persist them opaquely; see the full integration doc if you need
them.

Respond `200 OK`:

```json
{ "status": "ACCEPTED", "cbs_reference": "<your bank-side reference>" }
```

Anything else, or a timeout, triggers retry with backoff for 24 hours.

---

That's the entire bank-side surface area: one host, one YAML file, one
outbound REST call, one inbound webhook.

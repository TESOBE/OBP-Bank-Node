# OBP Bank Node — Technical Specification

**Version:** 0.1.0-draft  
**Date:** May 2026  
**Author:** TESOBE GmbH  
**Repository:** `OpenBankProject/OBP-Bank-Node` (proposed)

---

## 1. Overview

The **OBP Bank Node** is a software component that runs inside a bank's network and connects the bank's Core Banking System (CBS) to an interbank payment network powered by an OBP (Open Bank Project) API instance hosted by TESOBE.

The OBP Bank Node is not a passive adapter. It has three distinct communication directions:

```
Bank CBS
   ↕  Interface A — South (bank-facing REST, described in this document)
OBP Bank Node
   ↕  Interface B — North outbound (OBP REST API — Transaction Requests)
   ↕  Interface C — North inbound (RabbitMQ — OBP instructions)
   ↕  Interface D — Blockchain (Cardano — Promise and Settlement records)
OBP API (netting engine, hosted by TESOBE)
   ↕
Cardano Blockchain
```

### 1.1 What the OBP Bank Node is NOT

- It is **not** a customer-facing component. SCA (Strong Customer Authentication) is handled by the bank before the OBP Bank Node is called.
- It is **not** an OBP Adapter in the traditional sense — it both listens and initiates.
- It does **not** expose the OBP API to the bank — only a small, well-defined subset is proxied locally (Section 3.4). The full OBP API runs upstream at the operator (TESOBE).
- It does **not** require the bank to run OBP.

### 1.2 Message Standards

The OBP Bank Node's RabbitMQ message format on Interface C follows the **OBP Message Doc** standard — the same envelope, correlation, and routing conventions used across the Open Bank Project ecosystem. Banks and operators familiar with OBP messaging will encounter no surprises.

The OBP Bank Node-specific message types layered on top of that envelope are:

- Promise notification
- Netting Snapshot
- Settlement instruction
- Credit notification

---

## 2. Architecture

### 2.1 Communication Interfaces

| Interface | Direction | Protocol | Purpose |
|---|---|---|---|
| A — South | Bank CBS → OBP Bank Node | REST (HTTP) | Bank initiates outbound payment |
| A — South | OBP Bank Node → Bank CBS | REST (HTTP webhook) | OBP Bank Node notifies bank to credit customer |
| B — North outbound | OBP Bank Node → OBP API | OBP REST API | Submit Transaction Request to OBP API |
| C — North inbound | OBP API → OBP Bank Node | RabbitMQ | Receive instructions from OBP API |
| D — Blockchain | OBP Bank Node ↔ Cardano | Cardano node/API | Write/read Promise and Settlement records |

### 2.2 Deployment

The OBP Bank Node is delivered as a Docker image. The bank runs it as a container inside their network. It requires outbound network access to:

- OBP API gRPC/REST endpoints (hosted by TESOBE)
- RabbitMQ broker (hosted by TESOBE, credentials provisioned at registration)
- Cardano node or Blockfrost API endpoint

No inbound ports are exposed to the internet. All connections are outbound from the bank's network.

---

## 3. Interface A — South Side (Bank-Facing)

### 3.1 Design Principle

The south-side interfaces are designed to be as close as possible to existing OBP standards so that banks familiar with OBP APIs encounter no surprises. The bank-facing payment initiation request intentionally mirrors the OBP Transaction Request (COUNTERPARTY type).

The "client exposes an endpoint" pattern is intentional — the OBP Bank Node acts as a local proxy for the OBP API. The bank calls a local endpoint; the OBP Bank Node handles all complexity with the platform.

---

### 3.2 Interface A1 — Payment Initiation (Bank CBS → OBP Bank Node)

**Purpose:** The bank's CBS calls this endpoint when a customer has initiated an outbound cross-border payment. SCA is already complete before this call is made.

#### Endpoint

```
POST http://localhost:{OBP_BANK_NODE_PORT}/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/{VIEW_ID}/transaction-request-types/SIMPLE/transaction-requests
```

The path is mounted under `/obp-bank-node/v5.X.X/` rather than `/obp/v5.X.X/` to make it explicit that the bank's CBS is calling the local node, not the upstream OBP API. The request body and field semantics are identical to an OBP SIMPLE Transaction Request — banks already familiar with OBP can reuse the same payload shapes and validators unchanged.

The OBP Bank Node currently implements the following OBP endpoint subset (see Section 3.4 for the full list). Additional OBP endpoints may be added progressively.

Default port: `8088` (configurable via `OBP_BANK_NODE_PORT` in config).

#### Authentication

The OBP Bank Node exposes this endpoint on localhost only (not on any network interface by default). Authentication is via a shared secret configured in the OBP Bank Node config file:

```
Authorization: Bearer {OBP_BANK_NODE_LOCAL_SECRET}
```

The `OBP_BANK_NODE_LOCAL_SECRET` is set by the bank in their OBP Bank Node configuration and is never transmitted over the network.

#### Request Body

Identical to OBP Transaction Request (SIMPLE) body. The bank provides beneficiary routing details inline — no pre-registered counterparty required.

```json
{
  "value": {
    "currency": "KES",
    "amount": "50000.00"
  },
  "description": "Invoice payment INV-2026-0042",
  "to": {
    "otherBankRoutingScheme": "OBP",
    "otherBankRoutingAddress": "ke.01.kcs",
    "otherAccountRoutingScheme": "OBP",
    "otherAccountRoutingAddress": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1"
  },
  "charge_policy": "SHARED"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `value.currency` | string (ISO 4217) | Yes | Currency of the payment. Must match the sending account currency. |
| `value.amount` | string (decimal) | Yes | Payment amount. Must be positive. |
| `description` | string | Yes | Payment description / reference. Included in the OBP Transaction Request and the Cardano Promise record hash. |
| `to.otherBankRoutingScheme` | string | Yes | Routing scheme for the beneficiary bank. See supported schemes below. |
| `to.otherBankRoutingAddress` | string | Yes | Beneficiary bank identifier in the given scheme. |
| `to.otherAccountRoutingScheme` | string | Yes | Routing scheme for the beneficiary account. See supported schemes below. |
| `to.otherAccountRoutingAddress` | string | Yes | Beneficiary account identifier in the given scheme. |
| `charge_policy` | string | No | `SHARED`, `SENDER`, or `RECEIVER`. Defaults to `SHARED`. |

#### Supported Routing Schemes

The OBP Bank Node accepts the following routing scheme combinations. It resolves them internally to OBP API identifiers — the bank uses whatever routing language its CBS already knows.

| `otherBankRoutingScheme` | `otherBankRoutingAddress` example | `otherAccountRoutingScheme` | `otherAccountRoutingAddress` example | Notes |
|---|---|---|---|---|
| `OBP` | `ke.01.kcs` | `OBP` | `7bc9a8e4-6d02-40e3-a129-0b2bf89de9f0` | OBP-native identifiers. Simplest for OBP-familiar banks. |
| `BIC` | `KCBLKENX` | `IBAN` | `KE12KCBL0000001234567890` | Standard international routing. Preferred for European corridors. |
| `BIC` | `KCBLKENX` | `ACCOUNT_NUMBER` | `1234567890` | BIC + local account number. Common in African banking. |
| `MOBILE_PHONE` | `+254712345678` | `MOBILE_PHONE` | `+254712345678` | Mobile money routing (e.g. M-Pesa). Bank and account are the same identifier. |
| `BANK_ID` | `bank-ke-001` | `ACCOUNT_ID` | `acct-7bc9a8e4` | OBP API native identifiers. Assigned at bank registration. |

The OBP Bank Node resolves all schemes to OBP API identifiers before submitting to the OBP API. If a routing address cannot be resolved, the OBP Bank Node returns `OBP-BANK-NODE-ROUTING-001` (see error codes below).

The OBP Bank Node maintains an internal routing registry, populated from the OBP API at startup and refreshed periodically. When it first encounters a new routing address successfully resolved by the OBP API, it caches the mapping locally — so subsequent payments to the same beneficiary resolve instantly from the local cache.

#### Why SIMPLE rather than COUNTERPARTY

The SIMPLE type avoids the need for pre-registered counterparties, which would require banks to explicitly provision each corridor before making a payment. With SIMPLE, the bank provides routing details inline at payment time. The OBP Bank Node handles counterparty resolution and any internal registration with the OBP API transparently — the bank never sees or manages counterparties.

#### Synchronous Response

The OBP Bank Node responds synchronously with an acknowledgement. This response does not mean the payment has been processed — it means the OBP Bank Node has accepted the instruction and will process it.

**HTTP 202 Accepted:**

```json
{
  "transaction_request_id": "4050046c-63b3-4868-8a22-14b4181d33a6",
  "type": "COUNTERPARTY",
  "from": {
    "bank_id": "gh.29.uk",
    "account_id": "8ca8a7e4-6d02-40e3-a129-0b2bf89de9f0"
  },
  "to": {
    "counterparty_id": "9fg8a7e4-6d02-40e3-a129-0b2bf89de8uh"
  },
  "value": {
    "currency": "KES",
    "amount": "50000.00"
  },
  "description": "Invoice payment INV-2026-0042",
  "status": "INITIATED",
  "promise_id": null,
  "start_date": "2026-05-04T10:23:45Z",
  "end_date": null,
  "challenge": null
}
```

| Field | Description |
|---|---|
| `transaction_request_id` | The Transaction Request ID — assigned by the OBP Bank Node and propagated to the OBP API. Use this to correlate status updates from Interface A2. |
| `status` | `INITIATED` on acceptance. Subsequent status updates are delivered via the CBS webhook (Interface A2). |
| `promise_id` | Transaction ID of the Promise record on the blockchain. Null until written. |
| `promise_blockchain` | Blockchain on which the Promise was written (currently `Cardano`). Null until written. |
| `challenge` | Always `null` — SCA is pre-complete. The OBP Bank Node never triggers a challenge flow. |

#### Note on `challenge: null`

In standard OBP, a Transaction Request may return a challenge if SCA is required. The OBP Bank Node explicitly suppresses this — the bank has already completed SCA before calling the OBP Bank Node. The OBP Bank Node submits the Transaction Request to OBP with `charge_policy` set and a pre-auth context that bypasses the challenge flow.

#### Error Responses

| HTTP Status | Error | Description |
|---|---|---|
| `400` | `OBP-10001` | Incorrect JSON format |
| `400` | `OBP-40008` | Amount is zero or negative |
| `400` | `OBP-40003` | Currency mismatch with sending account |
| `401` | `OBP-BANK-NODE-AUTH-001` | Invalid or missing local secret |
| `422` | `OBP-BANK-NODE-ROUTING-001` | Routing address could not be resolved to an OBP API participant |
| `503` | `OBP-BANK-NODE-PLATFORM-001` | OBP API unreachable — instruction queued in local outbox |

Note: `503` with `OBP-BANK-NODE-PLATFORM-001` does not mean the payment is lost. The OBP Bank Node outbox holds the instruction and will submit it when connectivity is restored. The `transaction_request_id` returned in the 202 response can be used to query status.

---

### 3.3 Interface A2 — Credit Delivery (OBP Bank Node → Bank CBS)

**Purpose:** When the OBP API instructs the bank to credit a customer (an inbound payment arriving from another bank), the OBP Bank Node delivers the credit instruction to the bank's CBS.

The delivery mechanism is configurable. The OBP Bank Node supports multiple modes to accommodate the wide range of CBS capabilities found across African banks — from modern REST APIs to legacy systems with no API at all. The bank chooses the mode that fits their existing infrastructure. No CBS development is required for the database and file modes.

---

#### Delivery Mode 1: REST Webhook — OBP Format (Recommended)

The OBP Bank Node POSTs a credit notification to a URL configured in the OBP Bank Node config file. The request body uses OBP Transaction Request (SIMPLE) format — familiar to any bank already connected to OBP, and straightforward to implement for those that are not.

**Config:**
```yaml
delivery_mode: "webhook_obp"
credit_webhook_url: "http://bank-cbs/api/credit-notifications"
```

**Request:**
```
POST {credit_webhook_url}
Authorization: Bearer {OBP_BANK_NODE_LOCAL_SECRET}
Content-Type: application/json
```

```json
{
  "transaction_request_id": "4050046c-63b3-4868-8a22-14b4181d33a6",
  "netting_snapshot_id": "snap-3a4b5c6d-7e8f-9a0b-1c2d-3e4f5a6b7c8d",
  "netting_blockchain": "Cardano",
  "type": "SIMPLE",
  "from": {
    "bank_id": "gh.29.uk",
    "bank_routing": {
      "scheme": "OBP",
      "address": "gh.29.uk"
    }
  },
  "to": {
    "bank_id": "ke.01.kcs",
    "account_id": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1",
    "account_routing": {
      "scheme": "OBP",
      "address": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1"
    }
  },
  "value": {
    "currency": "KES",
    "amount": "50000.00"
  },
  "description": "Invoice payment INV-2026-0042",
  "value_date": "2026-05-04",
  "charge_policy": "SHARED",
  "promise_id": "abc123def456...",
  "promise_blockchain": "Cardano"
}
```

| Field | Description |
|---|---|
| `transaction_request_id` | OBP Bank Node-local reference — use the `transaction_request_id` to track status. Use this to acknowledge and reconcile. |
| `transaction_request_id` | OBP Transaction Request ID from the originating payment. |
| `netting_snapshot_id` | Cardano Netting Snapshot this credit relates to. |
| `type` | Always `SIMPLE` in this delivery mode. |
| `from.bank_id` | Originating bank OBP identifier. |
| `to.bank_id` / `to.account_id` | Receiving bank and beneficiary account — this bank's identifiers. |
| `value.currency` / `value.amount` | Credit amount and currency. |
| `value_date` | Value date for the credit posting. |
| `promise_id` | Transaction ID of the Promise record on the blockchain. |
| `promise_blockchain` | Blockchain on which the Promise was written (currently `Cardano`). |

**Expected response — HTTP 200:**
```json
{
  "status": "ACCEPTED",
  "cbs_reference": "CBS-TXN-20260504-98765"
}
```

---

#### Delivery Mode 2: REST Webhook — ISO 20022 JSON Format

For banks whose CBS teams prefer ISO 20022 field naming. The payload is semantically equivalent to a `camt.054` Bank-to-Customer Credit Notification, expressed in JSON rather than XML. Field names follow ISO 20022 conventions with an OBP Bank Node extension block for OBP Bank Node-specific fields.

**Config:**
```yaml
delivery_mode: "webhook_iso20022"
credit_webhook_url: "http://bank-cbs/api/payments/credit"
```

**Request:**
```
POST {credit_webhook_url}
Authorization: Bearer {OBP_BANK_NODE_LOCAL_SECRET}
Content-Type: application/json
```

```json
{
  "BkToCstmrDbtCdtNtfctn": {
    "GrpHdr": {
      "MsgId": "{transaction_request_id}",
      "CreDtTm": "2026-05-04T10:23:45Z"
    },
    "Ntfctn": {
      "Id": "4050046c-63b3-4868-8a22-14b4181d33a6",
      "Acct": {
        "Id": {
          "Othr": {
            "Id": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1",
            "SchmeNm": "OBP"
          }
        }
      },
      "Ntry": {
        "Amt": {
          "value": "50000.00",
          "Ccy": "KES"
        },
        "CdtDbtInd": "CRDT",
        "Sts": "BOOK",
        "BookgDt": "2026-05-04",
        "ValDt": "2026-05-04",
        "AddtlNtryInf": "Invoice payment INV-2026-0042",
        "RltdPties": {
          "Dbtr": {
            "Agt": {
              "FinInstnId": {
                "Othr": {
                  "Id": "gh.29.uk",
                  "SchmeNm": "OBP"
                }
              }
            }
          }
        }
      }
    }
  },
  "obp_bank_node": {
      "netting_snapshot_id": "snap-3a4b5c6d-7e8f-9a0b-1c2d-3e4f5a6b7c8d",
  "netting_blockchain": "Cardano",
    "promise_id": "abc123def456...",
    "promise_blockchain": "Cardano"
  }
}
```

Note: the `OBP Bank Node` block is a non-standard extension following the ISO 20022 convention for supplementary data. The abbreviations are unavoidable in ISO 20022 — this is why Mode 1 (OBP format) is recommended for most banks.

**Expected response — HTTP 200:**
```json
{ "status": "ACCEPTED", "cbs_reference": "CBS-TXN-20260504-98765" }
```

---

#### Delivery Mode 3: Database Write

The OBP Bank Node writes the credit instruction directly to a staging table in the bank's database. The bank's CBS polls this table and processes credits from it. No webhook endpoint required — no REST development at the bank.

This is the recommended mode for banks with legacy CBS systems that have no REST API.

**Config:**
```yaml
delivery_mode: "database"
db_host: "cbs-db.bank.local"
db_port: 5432
db_name: "cbs_staging"
db_username: "obp_writer"
db_password: "..."
db_table: "obp_credit_instructions"
```

**The OBP Bank Node writes a row to `obp_credit_instructions`:**

| Column | Type | Value |
|---|---|---|
| `transaction_request_id` | varchar | OBP Transaction Request ID |
| `netting_snapshot_id` | varchar | Cardano Netting Snapshot ID |
| `to_account_id` | varchar | Beneficiary account ID |
| `currency` | char(3) | ISO 4217 currency code |
| `amount` | decimal(18,5) | Credit amount |
| `description` | varchar | Payment description |
| `value_date` | date | Value date |
| `promise_id` | varchar | Transaction ID of the Promise record on the blockchain |
| `promise_blockchain` | varchar | Blockchain on which the Promise was written (e.g. `Cardano`) |
| `status` | varchar | `PENDING` on insert |
| `cbs_reference` | varchar | Null on insert; CBS writes its reference here on processing |
| `created_at` | timestamp | Insert timestamp |
| `processed_at` | timestamp | Null on insert; CBS writes timestamp here on processing |

The OBP Bank Node polls the table and considers a row acknowledged when `status` is updated to `PROCESSED` and `cbs_reference` is populated. Unprocessed rows older than 24 hours trigger a Record 4 Exception on Cardano.

Supported databases: PostgreSQL, MySQL, Oracle, Microsoft SQL Server.

---

#### Delivery Mode 4: File Drop

The OBP Bank Node writes a JSON file to a watched directory. The bank's CBS file watcher picks it up and processes it. Works with any CBS regardless of age or technology.

**Config:**
```yaml
delivery_mode: "file"
file_drop_path: "/shared/credit-notifications/inbound/"
file_format: "json"        # or "csv"
acknowledgement_path: "/shared/credit-notifications/acknowledged/"
```

The OBP Bank Node writes `{transaction_request_id}.json` to `file_drop_path`. The CBS processes it and moves/copies an acknowledgement file to `acknowledgement_path`. File format matches the OBP JSON body from Mode 1.

---

#### Retry Behaviour (Modes 1 and 2)

If the bank's CBS webhook does not return HTTP 200:

- Attempt 1: immediate
- Attempt 2: 30 seconds
- Attempt 3: 5 minutes
- Attempt 4: 30 minutes
- Attempt 5+: hourly, up to 24 hours

After 24 hours without acknowledgement, the OBP Bank Node raises a Record 4 (Exception) on Cardano and alerts the OBP API. The credit obligation remains open and visible to both banks and their regulators — it cannot be silently lost.

---

## 3.4 OBP Bank Node Partial OBP API Proxy — Implemented Endpoints

The OBP Bank Node implements a subset of the OBP API. Banks call these endpoints on `localhost:{OBP_BANK_NODE_PORT}` exactly as they would call them on an OBP instance. The OBP Bank Node proxies, enriches, or handles each call as appropriate.

| Method | OBP Endpoint | OBP Bank Node Behaviour |
|---|---|---|
| `POST` | `/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/{VIEW_ID}/transaction-request-types/SIMPLE/transaction-requests` | Core payment initiation — see Section 3.2. Submits to OBP API via OBP REST. Writes Cardano Promise record. |
| `GET` | `/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/{VIEW_ID}/transaction-requests/{TRANSACTION_REQUEST_ID}` | Returns current status of a Transaction Request including OBP Bank Node-specific fields (`promise_id`, `promise_blockchain`, `netting_snapshot_id`, `netting_blockchain`, `settlement_id`, `settlement_system`). |
| `GET` | `/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/{VIEW_ID}/transaction-requests` | Returns list of Transaction Requests submitted via this OBP Bank Node instance. |
| `GET` | `/obp-bank-node/v5.1.0/health` | OBP Bank Node health and connection status (see Section 9). |

### Version Support

The OBP Bank Node responds to both `/obp-bank-node/v5.1.0/` and `/obp-bank-node/v5.0.0/` prefixes on all implemented endpoints, routing both to the same internal handler. This avoids version coupling — banks do not need to update their CBS integration when OBP releases a new minor version.

### Non-Implemented Endpoints

Calls to OBP endpoints not listed above return:

```json
{
  "error": "OBP-BANK-NODE-NOT-PROXIED",
  "message": "This OBP endpoint is not implemented in the OBP Bank Node. Use the OBP API directly for this operation.",
  "obp_endpoint": "/obp-bank-node/v5.1.0/banks/..."
}
```

---

## 4. Interface B — Outbound to OBP API (OBP Transaction Request)

The OBP Bank Node calls the upstream OBP REST API to submit a Transaction Request. This is the standard OBP Transaction Request (COUNTERPARTY) endpoint:

```
POST {OBP_API_URL}/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/owner/transaction-request-types/SIMPLE/transaction-requests
```

The OBP Bank Node authenticates to the OBP API using OAuth2 credentials provisioned during bank registration. The OBP Bank Node maps the Interface A1 request body directly to this OBP call — no translation required since the formats are identical.

---

## 5. Interface C — Inbound from OBP API (RabbitMQ)

Wire pattern follows OBP-API's existing RabbitMQ connector (see `obp-api/.../rabbitmq/RabbitMQUtils.scala`):

- **Queue**: shared OBP RPC request queue, default `obp_rpc_queue` (configurable; matches OBP-API's `rabbitmq_connector.request_queue` property).
- **Pattern**: request/reply RPC. OBP-API publishes a request with AMQP properties `MessageId` (operation name), `CorrelationId` (UUID), and `ReplyTo` (per-request reply queue). The OBP Bank Node dispatches on `MessageId`, processes, and publishes a JSON inbound envelope back to `ReplyTo` with the same `CorrelationId`.
- **Reply envelope**:
  ```json
  {
    "inboundAdapterCallContext": { "correlationId": "..." },
    "status": { "errorCode": "", "backendMessages": [] },
    "data": { ... }
  }
  ```
  On handler error, `status.errorCode` carries the failure reason. On unknown `MessageId`, `errorCode` is `OBP-BANK-NODE-NOT-IMPLEMENTED`.

The following message types (AMQP `MessageId` values) are recognised:

| MessageId | Description | OBP Bank Node Action |
|---|---|---|
| `obp_credit_notification` | OBP API instructs bank to credit a customer | Call Interface A2 webhook |
| `obp_netting_snapshot` | New netting snapshot published | Write to local log; used for Cardano record reconciliation |
| `obp_settlement_instruction` | OBP API instructs settlement | Initiate settlement via the indicated system (Cardano / CHAPS / NIBSS / …) |
| `obp_status_update` | Transaction Request status changed | Update local status record; visible via the south-side status query endpoint |

---

## 6. Interface D — Cardano Blockchain

The OBP Bank Node writes and reads the five Cardano record types defined in the OBP Architecture Decisions document:

| Record | When Written | OBP Bank Node Role |
|---|---|---|
| Record 1 — Promise | On outbound payment submission | OBP Bank Node writes after OBP Transaction Request accepted |
| Record 2 — Netting Snapshot | Triggered by OBP API | OBP Bank Node receives via RabbitMQ; writes to Cardano |
| Record 3 — On-Chain Settlement | If ADA bearer settlement used | OBP Bank Node initiates ADA transfer; transaction IS Record 3 |
| Record 4 — Exception | On unresolvable error | OBP Bank Node writes to signal dispute/failure |
| Record 5 — Settlement Reference | On settlement confirmation | OBP Bank Node writes to close netting snapshot |

The OBP Bank Node uses the bank's configured Cardano wallet address and signing key (held in HSM or config) for on-chain operations.

---

## 7. Configuration

The OBP Bank Node is configured via a single YAML file (`obp-bank-node-config.yaml`) provided by the OBP API at registration:

```yaml
obp_bank_node:
  port: 8088
  local_secret: "change-me-on-first-run"

bank:
  bank_id: "ke.01.kcs"
  account_id: "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1"
  view_id: "owner"

obp_api:
  base_url: "http://localhost:8080"
  oauth2_consumer_key: "provided-at-registration"
  oauth2_consumer_secret: "provided-at-registration"
  oauth2_access_token: "provided-at-registration"
  oauth2_token_secret: "provided-at-registration"

rabbitmq:
  # Defaults match the rabbitmq:3-management Docker image:
  #   AMQP        amqp://guest:guest@localhost:5672/
  #   Management  http://localhost:15672  (guest / guest)
  host: "localhost"
  port: 5672
  username: "guest"
  password: "guest"
  virtual_host: "/"
  request_queue: "obp_rpc_queue"

cardano:
  wallet_address: "addr1q..."
  signing_key_path: "/secrets/cardano.skey"
  network: "mainnet"
  blockfrost_api_key: "provided-at-registration"

cbs_delivery:
  # Choose mode: webhook_obp | webhook_iso20022 | database | file
  mode: "webhook_obp"

  # Mode: webhook_obp or webhook_iso20022
  webhook:
    url: "http://bank-cbs/api/credit-notifications"
    timeout_seconds: 30

  # Mode: database
  database:
    host: "cbs-db.bank.local"
    port: 5432
    name: "cbs_staging"
    username: "obp_writer"
    password: "..."
    table: "obp_credit_instructions"
    driver: "postgresql"   # postgresql | mysql | oracle | sqlserver

  # Mode: file
  file:
    drop_path: "/shared/credit-notifications/inbound/"
    acknowledgement_path: "/shared/credit-notifications/acknowledged/"
    format: "json"         # json | csv

telemetry:
  type: "prometheus"
  port: 9090
  log_level: "INFO"
```

---

## 8. Status Query Endpoint

The OBP Bank Node exposes a status endpoint for the bank to query the state of a payment instruction:

```
GET http://localhost:{OBP_BANK_NODE_PORT}/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/{VIEW_ID}/transaction-requests/{TRANSACTION_REQUEST_ID}
```

**Response:**

```json
{
  "transaction_request_id": "4050046c-63b3-4868-8a22-14b4181d33a6",
  "status": "COMPLETED",
  "promise_id": "abc123def456...",
  "promise_blockchain": "Cardano",
  "netting_snapshot_id": "snap-3a4b5c6d-7e8f-9a0b-1c2d-3e4f5a6b7c8d",
  "netting_blockchain": "Cardano",
  "settlement_id": "def456abc789...",
  "settlement_system": "Cardano",
  "created_at": "2026-05-04T10:23:45Z",
  "settled_at": "2026-05-07T09:15:00Z"
}
```

**Possible Status Values:**

| Status | Description |
|---|---|
| `INITIATED` | OBP Bank Node has accepted the instruction; not yet submitted to OBP API |
| `SUBMITTED` | Submitted to OBP API via OBP Transaction Request |
| `PROMISE_WRITTEN` | Cardano Promise record (Record 1) confirmed on-chain |
| `PENDING_NETTING` | Awaiting inclusion in a Netting Snapshot |
| `PENDING_SETTLEMENT` | Netting Snapshot published; awaiting settlement |
| `COMPLETED` | Settlement confirmed; Record 5 written to Cardano |
| `EXCEPTION` | Exception raised; Record 4 written to Cardano |

---

## 9. Health and Readiness

```
GET http://localhost:{OBP_BANK_NODE_PORT}/health
GET http://localhost:{OBP_BANK_NODE_PORT}/ready
```

**Health response:**

```json
{
  "status": "healthy",
  "service": "OBP-Open-Corridor-Client",
  "version": "0.1.0",
  "connections": {
    "obp_api": "connected",
    "rabbitmq": "connected",
    "cardano": "connected"
  },
  "timestamp": "2026-05-04T10:00:00Z"
}
```

---

## 10. Security Considerations

1. The south-side REST endpoint (Interface A1) binds to `localhost` only by default. It MUST NOT be exposed on any network interface accessible outside the bank's network without additional authentication.
2. The `OBP_BANK_NODE_LOCAL_SECRET` must be rotated on first deployment and stored in the bank's secrets management system.
3. The Cardano signing key must be stored in an HSM where possible. File-based key storage (`signing_key_path`) is acceptable for development and lower-tier deployments.
4. All outbound connections to the OBP API use TLS 1.3 minimum.
5. RabbitMQ credentials are provisioned per-bank by the OBP API and should be stored in the bank's secrets management system, not in plaintext config files.
6. The OBP Bank Node logs all Interface A1 requests and Interface A2 webhook calls with full request/response bodies (excluding the `OBP_BANK_NODE_LOCAL_SECRET`) for audit purposes.

---

## 11. Outbox and Resilience

The OBP Bank Node maintains a local SQLite outbox for all outbound messages. If the OBP API or RabbitMQ is unreachable:

- Interface A1 calls are acknowledged to the bank with HTTP 202 and queued in the outbox
- The outbox is replayed on reconnection in strict order
- Outbox entries are retained for 90 days
- The outbox survives container restarts (mounted as a Docker volume)

This ensures no payment instruction is lost due to intermittent connectivity — important for the African network environments the OBP Bank Node is designed for.

---

## 12. Getting Started

### Step 1 — Register with OBP API

Contact TESOBE to register your bank on the OBP API network. You will receive:

- OBP OAuth2 credentials for the OBP API
- RabbitMQ connection credentials
- Cardano Blockfrost API key
- Your bank's OBP API `bank_id`
- A pre-populated `obp-bank-node-config.yaml`

### Step 2 — Deploy

```bash
docker pull tesobe/obp-bank-node:latest

docker run -d \
  --name obp-bank-node \
  -p 8088:8088 \
  -p 9090:9090 \
  -v /path/to/obp-bank-node-config.yaml:/app/obp-bank-node-config.yaml \
  -v /path/to/cardano.skey:/secrets/cardano.skey \
  -v /path/to/outbox:/app/outbox \
  tesobe/obp-bank-node:latest
```

### Step 3 — Implement Bank CBS Integration

Your CBS team implements two things:

1. **Call Interface A1** when a customer initiates an outbound interbank payment — `POST localhost:8088/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/owner/transaction-request-types/SIMPLE/transaction-requests`

2. **Receive Interface A2** — expose a webhook endpoint at your configured `CBS_CREDIT_WEBHOOK_URL` that accepts the credit notification body and posts the credit to the beneficiary account.

### Step 4 — Sandbox Testing

A sandbox OBP API is available at `https://apisandbox.openbankproject.com`. Use sandbox credentials to test the full payment flow before connecting to production.

### Step 5 — Certification

Complete the OBP Bank Node certification checklist (provided separately). Passing certification is required before connecting to the production OBP API.

---

## 13. Open Questions

1. Should the OBP Bank Node support batched payment submission (multiple Transaction Requests in a single Interface A1 call) for banks with high transaction volumes?
2. Cardano signing key management — define HSM integration path and supported HSM vendors for production deployments.
3. Should the OBP Bank Node support a callback/webhook alternative to polling for Interface A1 status updates — i.e. the OBP Bank Node calls the bank's CBS when a payment reaches `COMPLETED` or `EXCEPTION` status?

---
---

## 14. Bank Interface Essentials

*This section is intended for bank technical teams evaluating or beginning an OBP Bank Node integration. It summarises everything your team needs to know in plain terms — without requiring knowledge of the OBP API internals, Cardano, or RabbitMQ.*

---

### What is the OBP Bank Node?

The OBP Bank Node is a small piece of software you run inside your own network. It connects your Core Banking System (CBS) to the interbank payment network. You do not need to run any OBP API infrastructure — the platform is hosted by TESOBE. The OBP Bank Node handles all communication with the platform on your behalf.

Think of it like a SWIFT Alliance client — it sits inside your network, talks to the outside world, and gives your CBS a simple local interface.

---

### What your team needs to implement

You need to implement exactly two things:

**1. Send a payment instruction to the OBP Bank Node (outbound payments)**

When one of your customers initiates a cross-border interbank payment, your CBS calls a local REST endpoint on the OBP Bank Node. This is the same format as an OBP Transaction Request — if your team already knows OBP, there is nothing new to learn.

```
POST http://localhost:8088/obp-bank-node/v5.1.0/banks/{BANK_ID}/accounts/{ACCOUNT_ID}/views/owner/transaction-request-types/SIMPLE/transaction-requests
Authorization: Bearer {your-local-secret}
Content-Type: application/json

{
  "value": {
    "currency": "KES",
    "amount": "50000.00"
  },
  "description": "Invoice payment INV-2026-0042",
  "to": {
    "otherBankRoutingScheme": "BIC",
    "otherBankRoutingAddress": "KCBLKENX",
    "otherAccountRoutingScheme": "ACCOUNT_NUMBER",
    "otherAccountRoutingAddress": "1234567890"
  }
}
```

The OBP Bank Node responds immediately with an acknowledgement and a `transaction_request_id`. Your CBS stores this reference. Everything else — submitting to the OBP API, writing to the blockchain, netting — is handled by the OBP Bank Node automatically.

**2. Receive a credit instruction from the OBP Bank Node (inbound payments)**

When a payment arrives for one of your customers from another participating bank, the OBP Bank Node delivers a credit instruction to your CBS. You choose how it is delivered based on what your CBS can handle:

| Mode | What your CBS does | Best for |
|---|---|---|
| REST webhook (OBP format) | Expose one HTTP endpoint | Banks with a REST API |
| REST webhook (ISO 20022 JSON) | Expose one HTTP endpoint | Banks preferring ISO 20022 naming |
| Database write | OBP Bank Node writes to your staging table; CBS polls it | Legacy CBS with no REST API |
| File drop | OBP Bank Node writes a file; CBS picks it up | Any CBS, no integration work |

For most banks we recommend the REST webhook (OBP format). For banks with legacy systems, the database write mode requires zero CBS development — the OBP Bank Node does all the work.

---

### What the credit instruction looks like (OBP format)

```
POST {your-configured-url}
Authorization: Bearer {your-local-secret}
Content-Type: application/json

{
  "transaction_request_id": "4050046c-63b3-4868-8a22-14b4181d33a6",
  "type": "SIMPLE",
  "from": {
    "bank_id": "gh.29.uk",
    "bank_routing": {
      "scheme": "OBP",
      "address": "gh.29.uk"
    }
  },
  "to": {
    "bank_id": "ke.01.kcs",
    "account_id": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1",
    "account_routing": {
      "scheme": "OBP",
      "address": "7bc9a8e4-5d02-40e3-b129-1c3bf89de9f1"
    }
  },
  "value": {
    "currency": "KES",
    "amount": "50000.00"
  },
  "description": "Invoice payment INV-2026-0042",
  "value_date": "2026-05-04",
  "charge_policy": "SHARED",
  "promise_id": "abc123def456...",
  "promise_blockchain": "Cardano"
}
```

Your CBS should respond with HTTP 200 to confirm the credit has been accepted for posting:

```json
{ "status": "ACCEPTED", "cbs_reference": "CBS-TXN-20260504-98765" }
```

That is the entire integration surface. Your CBS does not need to know about the OBP API, RabbitMQ, Cardano, or netting. The OBP Bank Node handles all of that.

---

### What you do NOT need to implement

- Any RabbitMQ client or configuration
- Any Cardano wallet or blockchain integration
- Any OBP API calls
- Any counterparty registration or corridor setup before making payments
- Any SCA/challenge flow — your existing customer authentication applies; the OBP Bank Node never challenges

---

### Routing — how to identify the beneficiary

You provide the beneficiary's routing details inline with each payment. The OBP Bank Node resolves them to OBP API identifiers automatically. Supported routing combinations:

| Scheme | Example use |
|---|---|
| `BIC` + `IBAN` | European payments |
| `BIC` + `ACCOUNT_NUMBER` | African interbank payments |
| `MOBILE_PHONE` + `MOBILE_PHONE` | M-Pesa and mobile money |
| `OBP` + `OBP` | Banks already using OBP |
| `BANK_ID` + `ACCOUNT_ID` | OBP API native identifiers |

If a routing address cannot be resolved to an OBP API participant, the OBP Bank Node returns a clear error — no silent failures.

---

### Deployment

The OBP Bank Node is delivered as a Docker container. To deploy:

```bash
docker pull tesobe/obp-bank-node:latest

docker run -d \
  --name obp-bank-node \
  -p 8088:8088 \
  -v /path/to/obp-bank-node-config.yaml:/app/obp-bank-node-config.yaml \
  -v /path/to/outbox:/app/outbox \
  tesobe/obp-bank-node:latest
```

TESOBE provides the pre-populated `obp-bank-node-config.yaml` with your bank's credentials when you register. You configure:

- Your bank ID and account ID
- Your CBS delivery mode and URL (or database/file path)
- Your local secret for authenticating calls between your CBS and the OBP Bank Node

Everything else in the config is pre-filled by TESOBE.

---

### Connectivity requirements

The OBP Bank Node makes outbound connections only. Your firewall needs to allow outbound TCP from the OBP Bank Node container to:

| Destination | Port | Purpose |
|---|---|---|
| `api.openbankproject.com` | 443 | OBP API REST API |
| `rmq.openbankproject.com` | 5672 | RabbitMQ (or 5671 for TLS) |
| Cardano Blockfrost endpoint | 443 | Blockchain record writing |

No inbound ports need to be opened to the internet. The OBP Bank Node does not expose any public endpoint.

---

### Reliability — what happens when connectivity is lost

The OBP Bank Node is designed for African network environments where connectivity is intermittent. All outbound payment instructions are written to a local outbox before transmission. If the OBP API is unreachable, your CBS still receives an immediate acknowledgement and the OBP Bank Node queues the instruction and retries automatically when connectivity is restored. No payment instruction is lost due to a network outage.

For inbound credit deliveries, if your CBS is temporarily unavailable, the OBP Bank Node retries for up to 24 hours. If delivery cannot be confirmed after 24 hours, the situation is flagged on the OBP API and your TESOBE account manager is alerted.

---

### How to get started

1. Contact TESOBE to register your bank on the OBP API network
2. TESOBE provides your `obp-bank-node-config.yaml`, credentials, and sandbox access
3. Deploy the OBP Bank Node Docker container in your test environment
4. Call the payment initiation endpoint from your CBS test environment
5. Confirm you can receive a credit instruction via your chosen delivery mode
6. Complete the OBP Bank Node certification checklist
7. Go live on the production OBP API

---

*Questions? Contact the TESOBE team: obp@tesobe.com*

---

## 15. OBP Bank Node Dashboard

The OBP Bank Node includes a built-in web dashboard accessible at `http://localhost:{OBP_BANK_NODE_PORT}/dashboard`. It provides real-time visibility into the bank's cross-border payment activity — for operations, treasury, and compliance teams — without requiring access to the OBP API or any blockchain tooling directly.

The dashboard is read-only. All data is sourced from the OBP Bank Node's local outbox database and the Cardano API. It is intended for internal bank use only and is bound to localhost by default.

---

### 15.1 Operations View

For the bank's payments and operations team. Shows the current state of all payment activity flowing through the OBP Bank Node.

**Outbound Payments**

A live table of all outbound payment instructions submitted by the bank's CBS, showing:

- `transaction_request_id` — with a link to query the OBP API for full detail
- Beneficiary routing (scheme and address)
- Amount and currency
- Current status (`INITIATED` → `PROMISE_WRITTEN` → `PENDING_NETTING` → `PENDING_SETTLEMENT` → `COMPLETED` / `EXCEPTION`)
- `promise_id` — with a direct link to the Cardano explorer for that transaction
- Time in current status
- Any exception detail

**Inbound Credits**

A live table of all inbound credit instructions received from the OBP API, showing:

- `transaction_request_id`
- Originating bank
- Beneficiary account
- Amount and currency
- Delivery mode used (REST / database / file)
- Delivery status (delivered and acknowledged / retrying / failed)
- `promise_id` with Cardano explorer link

**Exceptions**

A dedicated alert panel showing any Record 4 (Exception) events — payments that could not be completed or credits that could not be delivered. Each exception shows the full detail and the Cardano record link. This panel is empty in normal operation.

---

### 15.2 Treasury View

For the bank's treasury team. Shows the bank's current net position across all active corridors.

**Net Positions by Corridor**

A table showing, for each active bank pair and currency:

- Corridor (e.g. Bank A ↔ Bank B, KES)
- Gross outbound since last netting snapshot
- Gross inbound since last netting snapshot
- Current net position (positive = we are owed; negative = we owe)
- Last netting snapshot date and amount
- Next scheduled netting event
- Gross-to-net ratio for this corridor

**ADA Settlement Balance**

The bank's current ADA wallet balance available for settlement, shown alongside:

- Projected ADA required for next netting settlement (based on current net positions and current ADA/local currency rate)
- Buffer status (sufficient / low / critical)
- Link to top up (informational — the actual top-up is done outside the OBP Bank Node)

**Netting Snapshot History**

A timeline of past netting snapshots, each showing:

- Snapshot ID with Cardano explorer link
- Currencies and amounts netted
- Settlement system used (`Cardano`, `CHAPS`, `NIBSS` etc.)
- Settlement status and confirmation reference

---

### 15.3 Compliance View

For the bank's compliance and audit team. Provides a complete, independently verifiable audit trail.

**Transaction Audit Trail**

Full detail for any transaction, searchable by `transaction_request_id`, date range, amount, or counterparty bank. Each record shows the complete lifecycle:

1. Payment initiated (timestamp, CBS reference)
2. Submitted to OBP API (OBP Transaction Request ID)
3. Promise written to blockchain (`promise_id`, `promise_blockchain`, Cardano explorer link)
4. Included in netting snapshot (`netting_snapshot_id`, Cardano link)
5. Settlement confirmed (`settlement_id`, `settlement_system`, confirmation reference)

**Cardano Record Verification**

For any transaction, the dashboard provides a direct link to each on-chain record on the Cardano explorer. Compliance teams can independently verify that the OBP Bank Node's records match the blockchain — without needing any blockchain expertise. The link opens the Cardano explorer at the exact transaction.

**Export**

All views support CSV and PDF export for regulatory reporting. Exports include full audit trail data and on-chain references.

---

### 15.4 Technical View

For the bank's IT team. Shows the health and connectivity status of the OBP Bank Node.

- OBP API connection status and latency
- RabbitMQ connection status, queue depth, last message received
- Cardano API connection status and last block seen
- Outbox status — number of queued items, oldest item age
- Recent error log
- OBP Bank Node version and config summary (credentials redacted)

This is an extension of the existing health endpoint (Section 9) rendered as a visual dashboard.

---

### 15.5 Dashboard Access Control

The dashboard is accessible on localhost only by default. To make it available to internal bank teams on the corporate network, set:

```yaml
dashboard:
  enabled: true
  bind_address: "0.0.0.0"   # default: "127.0.0.1"
  port: 8081                  # separate from the API port
  auth:
    enabled: true
    username: "admin"
    password: "..."           # change on first deployment
```

The dashboard does not expose any write operations. It is read-only by design.

---

---

## 16. Implementation — Technology Decisions

### 16.1 Language: Go

The OBP Bank Node is implemented in Go. Key reasons:

1. **Single binary deployment** — the OBP Bank Node compiles to a single self-contained binary. Docker images are small (~20MB). No runtime dependencies, no classpath issues.
2. **Low memory footprint** — critical for deployment alongside CBS software on constrained hardware in African banking environments. A running OBP Bank Node instance targets <50MB RAM.
3. **Excellent concurrency model** — goroutines handle the OBP Bank Node's concurrent concerns naturally: RabbitMQ listener, REST API server, Cardano writer, outbox processor, and dashboard server all run concurrently with minimal boilerplate.
4. **Fast startup** — instant. The OBP Bank Node is ready in milliseconds after `docker run`.
5. **Contributor accessibility** — Go is readable by any developer familiar with Java, Python, or C. The barrier to a first contribution is hours, not weeks. Important for attracting African bank developer contributions.
6. **Strong standard library** — HTTP server, JSON, SQLite, TLS, and concurrent primitives are all in the standard library or well-maintained first-party packages.

### 16.2 Key Dependencies

| Concern | Library | Notes |
|---|---|---|
| RabbitMQ client | `github.com/rabbitmq/amqp091-go` | Official RabbitMQ Go client |
| REST API server | `github.com/go-chi/chi` | Lightweight, idiomatic router |
| HTTP client (OBP) | `net/http` stdlib | Standard library sufficient |
| SQLite outbox | `github.com/mattn/go-sqlite3` | Mature, CGO-based; or `modernc.org/sqlite` for pure Go |
| Cardano / Blockfrost | `net/http` stdlib | Blockfrost is REST — no SDK needed |
| Configuration | `github.com/spf13/viper` | YAML config with env var override |
| Logging | `go.uber.org/zap` | Structured logging with correlation IDs |
| Metrics | `github.com/prometheus/client_golang` | Prometheus metrics |
| Testing | `testing` stdlib + `github.com/stretchr/testify` | Standard + assertions |
| Dashboard UI | Embedded `html/template` + HTMX | Server-side rendered, minimal JS |

### 16.3 Project Structure

```
obp-bank-node/
├── cmd/
│   └── obp-bank-node/
│       └── main.go              # Entry point
├── internal/
│   ├── config/
│   │   └── config.go            # Config loading and validation
│   ├── api/
│   │   ├── server.go            # REST API server (south side)
│   │   ├── payment.go           # POST /obp-bank-node/.../transaction-requests
│   │   ├── status.go            # GET /obp-bank-node/.../transaction-requests/{transaction_request_id}
│   ├── messaging/
│   │   ├── consumer.go          # RabbitMQ consumer
│   │   ├── producer.go          # RabbitMQ producer (if needed)
│   │   └── handlers.go          # Message type handlers
│   ├── platform/
│   │   └── client.go            # OBP REST API client (outbound to OBP API)
│   ├── cardano/
│   │   └── blockfrost.go        # Cardano record writing via Blockfrost
│   ├── delivery/
│   │   ├── delivery.go          # Delivery interface
│   │   ├── webhook_obp.go       # Mode 1: REST webhook OBP format
│   │   ├── webhook_iso20022.go  # Mode 2: REST webhook ISO 20022 format
│   │   ├── database.go          # Mode 3: Database write
│   │   └── file.go              # Mode 4: File drop
│   ├── outbox/
│   │   └── outbox.go            # SQLite outbox for reliable delivery
│   ├── dashboard/
│   │   ├── server.go            # Dashboard HTTP server
│   │   ├── operations.go        # Operations view handlers
│   │   ├── treasury.go          # Treasury view handlers
│   │   ├── compliance.go        # Compliance view handlers
│   │   └── templates/           # HTML templates
│   └── telemetry/
│       ├── telemetry.go         # Telemetry interface
│       ├── prometheus.go        # Prometheus implementation
│       └── console.go           # Console implementation (dev)
├── pkg/
│   └── models/
│       └── models.go            # Shared data models (TransactionRequest etc.)
├── docker/
│   └── Dockerfile
├── obp-bank-node-config.yaml.example      # Example configuration
├── go.mod
├── go.sum
├── README.md
├── ARCHITECTURE.md
└── BANK-QUICKSTART.md           # Bank-facing quick start guide
```

### 16.4 Core Interfaces

The OBP Bank Node is built around three core interfaces that keep concerns cleanly separated:

```go
// Delivery — how credits reach the bank's CBS
type Delivery interface {
    Deliver(ctx context.Context, credit *CreditInstruction) error
    Name() string
}

// PlatformClient — how the OBP Bank Node talks to the OBP API
type PlatformClient interface {
    CreateTransactionRequest(ctx context.Context, req *TransactionRequest) (*TransactionRequestResponse, error)
    GetTransactionRequest(ctx context.Context, id string) (*TransactionRequestResponse, error)
}

// BlockchainWriter — how the OBP Bank Node writes records on-chain
type BlockchainWriter interface {
    WritePromise(ctx context.Context, promise *PromiseRecord) (string, error)
    WriteSettlementReference(ctx context.Context, ref *SettlementReference) (string, error)
    WriteException(ctx context.Context, ex *ExceptionRecord) (string, error)
}
```

These interfaces mean the Delivery mode, Platform client, and Blockchain writer are all swappable — mock implementations for testing, real implementations for production.

### 16.5 Repository

```
github.com/OpenBankProject/OBP-Bank-Node
```

GNU Affero General Public License v3.0 (AGPLv3).

---


*© TESOBE GmbH 2026 — OBP Bank Node Project*

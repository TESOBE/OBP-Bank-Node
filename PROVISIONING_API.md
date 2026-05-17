# Provisioning API for OBP Bank Node — Cert-Mode Self-Service

What needs to change inside the `OpenBankProject/OBP-API` codebase to implement
the self-service certificate-issuance flow from `CERT_TODO.md`. Companion
document, not a replacement.

**Robustness rule applies throughout.** Operator-side UI / scripts can be thin;
OBP-API code (CSR handling, CA integration, vhost provisioning, persistence) is
production-grade regardless.

## 1. The flow this implements

```
Bank                                       OBP-API
─────                                      ───────
1. openssl genpkey  → private key          (never leaves the bank)
2. openssl req -new → CSR
   subject CN = bank.{bank_id}
3. POST /provision-bank-node          ─→   • Authenticate the requester
   {                                       • Validate CSR (subject, key, replay)
     "csr": "-----BEGIN CERTIFICATE        • Sign CSR via configured CA backend
              REQUEST-----\n…",            • Create RabbitMQ vhost /bank.{bank_id}
     "requested_validity_days": 365,       • Map cert CN to broker permissions
     "intended_use": ["rabbitmq",          • Persist cert metadata (no private key)
                      "obp_api",           • Bundle config response
                      "cardano_provider"]
   }
4. ←  {                                  
        "bank_certificate": "-----BEGIN CERT-----…",
        "ca_chain":         "-----BEGIN CERT-----…",
        "cert_serial":      "0a:fc:…",
        "issued_at":        "2026-05-09T10:30:00Z",
        "expires_at":       "2027-05-09T10:30:00Z",
        "config_bundle":    { … }
      }
5. Save cert + chain to /secrets/. Merge config_bundle into
   obp-bank-node-config.yaml.
6. Connect amqps://broker:5671 with client cert  → identity established.
```

## 2. New REST endpoints

`obp-api/src/main/scala/code/api/v5_1_0/`

| Endpoint | Role | Purpose |
|---|---|---|
| `POST /obp/v5.1.0/banks/{bank_id}/provision-bank-node` | `CanProvisionBankNode` | Initial provision: signs CSR, creates vhost, returns cert + chain + config bundle. |
| `POST /obp/v5.1.0/banks/{bank_id}/provision-bank-node/renew` | `CanRenewBankNodeCertificate` (bank-level — banks renew their own) | Submit fresh CSR before expiry. Old cert remains valid until natural expiry — overlap window for zero-downtime rotation. |
| `POST /obp/v5.1.0/banks/{bank_id}/provision-bank-node/revoke` | `CanRevokeBankNodeCertificate` | Revokes a specific cert by serial; adds to CRL; broker user removed. |
| `GET  /obp/v5.1.0/banks/{bank_id}/provision-bank-node/status` | `CanReadBankNodeProvisioning` | Cert state, serial, issued_at, expires_at, revoked_at. |
| `GET  /obp/v5.1.0/provision-bank-node/ca-chain` | unauthenticated | Public CA chain in PEM. Banks pin against this when verifying broker server cert. |

OBP-API conventions: each endpoint defined as a `ResourceDoc` with example
request/response, OAS3 metadata, connector method delegation. Plenty of
templates in the existing v5.1.0 endpoint files.

### Request / response shapes

`POST /provision-bank-node` request:

```json
{
  "csr": "-----BEGIN CERTIFICATE REQUEST-----\n…\n-----END CERTIFICATE REQUEST-----",
  "requested_validity_days": 365,
  "intended_use": ["rabbitmq", "obp_api"]
}
```

Response:

```json
{
  "bank_certificate": "-----BEGIN CERTIFICATE-----\n…",
  "ca_chain":         "-----BEGIN CERTIFICATE-----\n…",
  "cert_serial":      "0a:fc:1b:…",
  "common_name":      "bank.ke.01.kcs",
  "issued_at":        "2026-05-09T10:30:00Z",
  "expires_at":       "2027-05-09T10:30:00Z",
  "config_bundle": {
    "rabbitmq": {
      "protocol":      "amqps",
      "host":          "rmq.openbankproject.com",
      "port":          5671,
      "virtual_host":  "/bank.ke.01.kcs",
      "request_queue": "obp_rpc_queue"
    },
    "obp_api": {
      "base_url": "https://obp-api.example.com",
      "auth":     "mtls"
    },
    "cardano": {
      "ogmios_url": "wss://cardano-preprod.example.com:1337",
      "auth":       "mtls"
    }
  }
}
```

The bank merges `config_bundle` into `obp-bank-node-config.yaml`, drops the
cert + chain on disk, and is done. `username` / `password` from the legacy
RabbitMQ block are absent — identity comes from the cert.

## 3. CA backend abstraction

New trait under `obp-api/src/main/scala/code/provisioning/pki/`:

```scala
trait CertificateAuthority {
  def signCsr(csrPem: String, validityDays: Int, subject: SubjectDN): Box[SignedCertificate]
  def revoke(serial: BigInt, reason: RevocationReason): Box[Unit]
  def getChain(): Box[String]            // PEM
  def getCrl():   Box[Array[Byte]]       // DER
}
```

Implementations:

| Implementation | Use |
|---|---|
| `VaultPkiCertificateAuthority` | Production. `pki/sign/{role}`, `pki/revoke`, `pki/crl`. Vault token auth. |
| `StepCaCertificateAuthority` | Alternative production. Smaller footprint; ACME story. |
| `LocalOpenSslCertificateAuthority` | Dev only. Wraps `openssl ca`. Logged WARN to prevent accidental production use; refuses to start unless `provisioning.allow_dev_ca = true`. |
| `MockedCertificateAuthority` | Tests. Deterministic certs with caller-controlled expiry. |

Selected via `provisioning.ca_backend` (see §9). Production startup refuses
`mocked` / `openssl` unless the deployment profile is explicitly dev/test.

## 4. RabbitMQ admin integration

Separate from the messaging connector that publishes business messages.
New trait under the same `code/provisioning/pki/` package:

```scala
trait BrokerAdmin {
  def ensureVhost(name: String): Box[Unit]
  def ensureUserMappedToCn(vhost: String, commonName: String, perms: Permissions): Box[Unit]
  def removeUser(commonName: String): Box[Unit]
}
```

Implementation `RabbitMqHttpAdmin` uses the broker's management HTTP API:

- `PUT /api/vhosts/{vhost}` — idempotent
- `PUT /api/users/{cn}` — no password; `auth_mechanism_ssl` plugin maps cert CN to user
- `PUT /api/permissions/{vhost}/{cn}` — regex perms scoped to `obp_rpc_queue` and `obp.bank.*`
- `DELETE /api/users/{cn}` on revoke

Vhost names follow `/bank.{bank_id}` — the bank ID is the canonical identity,
also embedded in the cert CN. The two derive from the same source (the path
parameter), so they cannot drift.

## 5. Connector trait extensions

`obp-api/src/main/scala/code/bankconnectors/Connector.scala`:

| Method | Purpose |
|---|---|
| `provisionBankNode(bankId, csrPem, validityDays, intendedUse)` | Backs the POST endpoint. Orchestrates CA sign + broker provision + persistence in a single unit of work. |
| `renewBankNodeCertificate(bankId, csrPem)` | Re-sign without re-provisioning vhost. |
| `revokeBankNodeCertificate(bankId, serial, reason)` | CA revoke + broker user removal. |
| `getBankNodeProvisioningStatus(bankId)` | Reads `MappedBankNodeCertificate`. |

Each gets outbound/inbound DTOs in `obp-commons/.../dto/` matching existing
naming. **These methods are local-only** — they don't traverse RabbitMQ to a
remote adapter, since their entire job is to *establish* the connection that
RabbitMQ messaging would use. Implement against `LocalMappedConnector` and
`MockedConnector`. No `RabbitMQConnector_vOct2024` impl needed.

`messageDocs +=` entries are therefore not required for these methods.

## 6. Database schema

New table — additive, gated by `provisioning.enabled`:

```sql
CREATE TABLE bank_node_certificates (
    id                UUID PRIMARY KEY,
    bank_id           VARCHAR NOT NULL,
    cert_serial       VARCHAR NOT NULL UNIQUE,
    common_name       VARCHAR NOT NULL,
    csr_sha256        VARCHAR NOT NULL,        -- audit; private key never stored
    issued_at         TIMESTAMPTZ NOT NULL,
    expires_at        TIMESTAMPTZ NOT NULL,
    revoked_at        TIMESTAMPTZ,
    revocation_reason VARCHAR,
    intended_use      VARCHAR NOT NULL,        -- comma-separated
    status            VARCHAR NOT NULL,        -- ACTIVE | EXPIRING | EXPIRED | REVOKED
    issued_by_backend VARCHAR NOT NULL         -- vault | stepca | openssl | mocked
);
CREATE INDEX ON bank_node_certificates (bank_id, status);
CREATE INDEX ON bank_node_certificates (expires_at) WHERE revoked_at IS NULL;
```

Mapper: `MappedBankNodeCertificate.scala` under `code/provisioning/pki/`,
following the LongKeyedMapper pattern used elsewhere in OBP-API.

The private key is **never** stored — only the CSR's sha256 (so a bank
resubmitting an old CSR is detectable). The signed cert's bytes live in the CA
backend (Vault) or on disk; the DB row references it by serial, not contents.

## 7. CSR validation

Non-negotiable. Apply before any CA call:

| Check | Rule | Error code |
|---|---|---|
| Subject CN | Must equal `bank.{bank_id}` exactly — path bank_id ↔ CSR CN ↔ vhost all derive from the same value | `OBP-PROVISION-CSR-CN-MISMATCH` |
| Subject extras (O / OU / C) | Optional; if present must match a per-deployment policy table | `OBP-PROVISION-CSR-SUBJECT-INVALID` |
| Key algorithm | Allowlist: `RSA ≥ 3072`, `ECDSA P-256/P-384`, `Ed25519`. Reject anything else. | `OBP-PROVISION-CSR-KEY-ALG` |
| Key strength | Reject keys flagged by published weak-key advisories (Debian OpenSSL list etc.) | `OBP-PROVISION-CSR-KEY-WEAK` |
| SAN | If present, must be DNS names from a per-bank allowlist | `OBP-PROVISION-CSR-SAN-DENIED` |
| Self-signature | CSR must verify against its own embedded public key | `OBP-PROVISION-CSR-SIGNATURE` |
| Replay | Reject if `csr_sha256` already appears for this bank — forces a fresh keypair on renew | `OBP-PROVISION-CSR-REPLAY` |
| Body size | Reject CSRs > 16 KiB (DoS guard) | `OBP-PROVISION-CSR-TOO-LARGE` |

All checks return specific OBP-PROVISION-CSR-INVALID-XXX codes — banks need
actionable errors, not "400 Bad Request".

## 8. New ApiRoles

`obp-api/src/main/scala/code/api/util/ApiRole.scala`:

- `CanProvisionBankNode` — system-level (TESOBE operator initial provision)
- `CanRenewBankNodeCertificate` — bank-level (bank renews its own)
- `CanRevokeBankNodeCertificate` — both: bank can revoke their own; operator can revoke any
- `CanReadBankNodeProvisioning` — bank-level

Self-service hinges on bank-level `CanRenewBankNodeCertificate`: once initially
provisioned, the bank can rotate their cert without operator involvement —
exactly the property that makes cert auth less operationally painful than
password rotation.

## 9. Configuration

`props` files (`default.props.template`):

```
provisioning.enabled                     = true
provisioning.ca_backend                  = vault    # vault | stepca | openssl | mocked
provisioning.allow_dev_ca                = false    # must be true for openssl/mocked outside test
provisioning.default_validity_days       = 365
provisioning.renewal_window_days         = 60       # status flips ACTIVE → EXPIRING this far out
provisioning.max_csr_bytes               = 16384

provisioning.vault.address               = https://vault.tesobe.com
provisioning.vault.pki_mount             = pki_intermediate
provisioning.vault.role                  = bank-node
provisioning.vault.token_file            = /run/secrets/vault-token

provisioning.broker_admin.url            = https://rmq.openbankproject.com:15671/api
provisioning.broker_admin.username       = obp-api-admin
provisioning.broker_admin.password_file  = /run/secrets/broker-admin
provisioning.broker_admin.deployment_prefix =        # avoids vhost-name collisions across deployments
```

Bank-broker mapping is a side effect of `ensureVhost` — the row in the
`open_corridor_bank_brokers` table from `OBP_API_CHANGES.md` §11 is populated
during provisioning.

## 10. Audit trail

Every provision / renew / revoke logs via OBP-API's existing audit mechanism:

- Requesting user + roles
- Bank ID
- CSR sha256 (the CSR itself isn't secret but is bulky — the hash suffices)
- Cert serial issued / revoked
- CA backend used
- Outcome + error code if any

Don't bypass the audit layer. Compliance asks "who issued cert X" needs a clean
answer.

## 11. Status sweeper

Background actor under `code/actorsystem/provisioning/`:

- Runs every hour
- Flips `ACTIVE` → `EXPIRING` for rows hitting `expires_at - renewal_window_days`
- Flips `ACTIVE`/`EXPIRING` → `EXPIRED` for rows past `expires_at`
- Emits an alert (operator notification surface, TBD) per bank entering EXPIRING
- Optionally publishes `obp.cert.expiring` on the bank's vhost so the Bank Node
  itself can warn its operator

Without this, certs silently expire and banks page their on-call at 3am. Banks
expect proactive renewal nudges.

## 12. Tests

OBP-API conventions: ScalaTest / Specs2.

- Unit: every CSR validator rule (one happy + one rejection per row in §7) + a property test that `signCsr` is never called when validation fails
- Mocked CA happy-path: provision → cert parses → CN matches → expiry equals `validity_days` → `bank_node_certificates` row written
- Renewal: existing cert remains `ACTIVE` until natural expiry; new cert overlaps
- Revocation: CA `revoke` called; broker user removed; CRL contains the serial
- Idempotency: same CSR submitted twice → first issues, second returns 409 with the existing serial unless an explicit `force = true`
- Negative paths: malformed PEM, wrong CN, weak key, oversized body — each returns its specific error code
- Integration: stand up a Vault dev server in CI; run a real provision against it (compose-driven, gated behind a `CI_HAS_VAULT` flag)

Per the robustness rule — not "a couple of happy paths". This module ships with
the same coverage bar as the rest of OBP-API.

## 13. Rollout / opt-in

- Schema migration applied unconditionally (additive, nullable — no behaviour change without `provisioning.enabled`)
- `provisioning.enabled = false` by default → endpoints return 404, sweeper doesn't start
- Existing password-mode RabbitMQ continues to work indefinitely — cert mode is purely additive
- Per-bank migration: bank generates keypair → calls `/provision-bank-node` → swaps their config from password to TLS block → restarts Bank Node → validates `/health`. Operator disables the password user once comfortable.
- No legacy data to backfill — greenfield.

## Hard parts called out specifically

| Concern | Why it bites |
|---|---|
| **The private key never crosses the API.** | Tempting to "help" by generating and shipping the keypair. Don't. Reject any request body containing a private-key PEM block; document this explicitly. The whole security model rests on the bank holding the private key. |
| **Idempotency under retry.** | Bank's curl retries on a 502; must not double-issue. Key on `(bank_id, csr_sha256)` and return the existing cert if seen before. |
| **Vhost-name collision across deployments.** | If two TESOBE deployments share a broker, `bank.ke.01.kcs` collides. Vhost name must include `provisioning.broker_admin.deployment_prefix`. Pre-register the prefix in the bank ID space. |
| **Renewal race.** | Two concurrent renewals for the same bank — must serialise. DB advisory lock keyed on `bank_id`; otherwise broker permissions can map to a CN whose cert was just superseded. |
| **CRL distribution lag.** | Broker re-reads CRL on a schedule; worst-case window between revoke and broker-rejecting the cert is one CRL refresh. Document the value; consider OCSP stapling later. |
| **CA disaster recovery.** | If the CA root key is lost, every bank's chain breaks. Vault auto-unseal + offline root + intermediate-only signing. Out of scope for this doc but called out to operators on day one. |
| **Bank ID in CN is load-bearing.** | Path bank_id ↔ CSR CN ↔ vhost name ↔ broker permissions all derive from one value. A typo anywhere creates a cert that authenticates to the wrong vhost. The CN-mismatch validator (§7) is the only thing standing between that typo and a silent cross-tenant access bug. |

## Rough sequence

The order to land changes in OBP-API:

1. Schema migration: `bank_node_certificates` (additive, ships immediately, no behaviour without `provisioning.enabled`)
2. `CertificateAuthority` trait + `MockedCertificateAuthority` + property tests
3. CSR validator with the §7 rules + unit tests
4. `BrokerAdmin` trait + `RabbitMqHttpAdmin` impl + integration test against a CI broker
5. `provisionBankNode` connector method (LocalMappedConnector + Mocked) + DTOs + audit hooks
6. `POST /provision-bank-node` endpoint + ApiRoles + `ResourceDoc`
7. `VaultPkiCertificateAuthority` impl + integration test against a CI Vault dev server
8. Renewal endpoint + connector method + advisory-lock plumbing
9. Revocation endpoint + CRL retrieval + broker user removal
10. Status endpoint + per-bank read view
11. Public CA-chain endpoint (`GET /provision-bank-node/ca-chain`)
12. Status sweeper actor (ACTIVE → EXPIRING → EXPIRED)
13. End-to-end test: bank-side `openssl` + `curl` walkthrough script under `tests/provisioning/` — exercises the documented flow exactly as a bank would run it

Steps 1–6 give you initial provisioning working against the mocked CA. 7–9
turn it into the real operator surface. 10–13 productionise it.

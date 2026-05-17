# Certificate-based Auth for OBP Bank Node

Migrate per-bank authentication from username/password to **X.509 client certificates (mTLS)**. Applies to RabbitMQ at minimum, ideally also to OBP API and any other per-bank interface.

## Why

- The bank generates its own keypair locally; the private key never leaves their environment.
- TESOBE only signs CSRs — no secret material to transmit, no delivery-window risk.
- Hardware-backed keys (HSM / PKCS#11) are a drop-in if the bank wants them.
- Cert revocation (CRL / short-lived certs) gives a cleaner rotation story than password rotation.
- One cert can authenticate every interface the bank touches (RabbitMQ, OBP API, managed Cardano-node provider, admin UIs).

## Self-service flow

```
Bank side                              TESOBE side
─────────                              ───────────
1. openssl genkey                 →    bank holds private key locally
2. openssl req -new ...           →    CSR file (subject CN=bank.ke.01.kcs)
3. POST /provision-bank-node      →    OBP API:
   { "csr": "-----BEGIN..." }            • verifies the bank
                                         • signs CSR with TESOBE intermediate CA
                                         • creates RabbitMQ vhost
                                         • sets permissions on cert subject CN
                                         • does NOT need the private key
4. ← returns:
     bank.crt
     ca-chain.crt
     vhost: /bank.ke.01.kcs
     (plus the rest of obp-bank-node-config.yaml fields)
5. Connect to amqps://...:5671
   with client cert            →       broker validates cert against CA,
                                       extracts CN, looks up permissions
                                       on /bank.ke.01.kcs → connection up
```

## What the broker needs

Enable the `rabbitmq-auth-mechanism-ssl` plugin and configure TLS:

```bash
rabbitmq-plugins enable rabbitmq_auth_mechanism_ssl
```

```ini
listeners.ssl.default = 5671
ssl_options.cacertfile = /etc/rabbitmq/ssl/ca-chain.crt
ssl_options.certfile   = /etc/rabbitmq/ssl/broker.crt
ssl_options.keyfile    = /etc/rabbitmq/ssl/broker.key
ssl_options.verify     = verify_peer
ssl_options.fail_if_no_peer_cert = true
auth_mechanisms.1 = EXTERNAL
ssl_cert_login_from = common_name
```

`auth_mechanisms.1 = EXTERNAL` tells RabbitMQ to take identity from the TLS layer rather than a SASL exchange. `ssl_cert_login_from = common_name` maps the cert's CN to a RabbitMQ username — so `set_permissions` is keyed on the CN, no password ever exists.

## What the Bank Node needs

New optional TLS block in `obp-bank-node-config.yaml`:

```yaml
rabbitmq:
  protocol: "amqps"            # was implicit "amqp"
  host: "rmq.openbankproject.com"
  port: 5671
  virtual_host: "/bank.ke.01.kcs"
  request_queue: "obp_rpc_queue"
  tls:
    client_cert: "/secrets/bank.crt"
    client_key:  "/secrets/bank.key"
    ca_chain:    "/secrets/ca-chain.crt"
  # username/password absent — identity comes from the cert
```

Code change in `internal/messaging/consumer.go`: branch on `protocol` to call `amqp.DialTLS` with a `*tls.Config` populated from the TLS block. Roughly 30 lines.

## The CA tax

TESOBE has to run a CA. Options ranked by self-service ergonomics:

1. **HashiCorp Vault PKI** — built-in `/sign` endpoint that takes a CSR and returns a signed cert. Designed for this exact flow.
2. **step-ca** — small footprint, OAuth-integrated, good ACME story.
3. **AWS Private CA** — fully managed, expensive at scale, AWS-locked.
4. **Roll your own with `openssl ca`** — works but not recommended for production.

Plus the lifecycle bits: cert expiry monitoring, CRL or OCSP for revocation, renewal flow before expiry. Banks need automated renewal or expect 3am calls.

## Unified identity bonus

The same cert can secure every interface the bank touches:

- mTLS to RabbitMQ (Interface C)
- mTLS to OBP API (Interface B) — replaces OAuth2 entirely
- mTLS to a managed Cardano node provider (e.g. Demeter.run)
- mTLS to admin / observability dashboards

One credential to issue, rotate, revoke.

## Work list

| Item | Where | Notes |
|---|---|---|
| Stand up CA | TESOBE infra | Vault PKI recommended |
| Enable `rabbitmq-auth-mechanism-ssl` plugin | Broker | One-line plugin enable + config |
| Issue a broker server cert from the CA | TESOBE | Standard TLS server cert |
| Add `tls:` block to `RabbitMQConfig` | `internal/config/config.go` | Optional; absent = current password mode |
| Implement `amqps://` dialer | `internal/messaging/consumer.go` | Branch on `protocol`; build `*tls.Config`, call `amqp.DialTLS` |
| `POST /obp/v5.1.0/banks/{bank_id}/provision-bank-node` (cert mode) | OBP-API | Accepts CSR, signs against CA, creates vhost + maps permissions to CN, returns signed cert + chain + config bundle |
| Renewal endpoint | OBP-API | `POST /provision-bank-node/renew` accepts a fresh CSR, returns new cert; old cert valid until it naturally expires |
| Revocation | OBP-API + CA | Add cert serial to CRL; broker re-reads CRL on schedule |
| Spec update | `docs/OBP-Bank-Node-Spec.md` §10 + §12 | Document the cert-based flow as the recommended auth model; mark password mode as legacy/dev-only |

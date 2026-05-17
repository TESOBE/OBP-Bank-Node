# Architecture

Source-of-truth for the OBP Bank Node's implementation choices. Decisions
captured here are committed — re-open with reason, not on a whim.

For *what* the node does and *why* it exists, see `TLDR.md` and
`docs/OBP-Bank-Node-Spec.md`. This file is *how* it is built.

## Language: Rust

The OBP Bank Node is implemented in Rust. A prior Go skeleton existed and was
deleted — it was tracer-bullet code, never tested in anger.

**Why Rust over Go:**

- **Cardano is the hardest part.** The Cardano ecosystem's serious tooling is
  Rust: `pallas`, `cardano-multiplatform-lib`, `cardano-serialization-lib`,
  `oura`, `mithril`, `aiken`. Used in production by every Cardano-native infra
  company. The Go ports (`echovl/cardano-go`,
  `fivebinaries/go-cardano-serialization`) are community projects with real
  maturity and maintenance risk.
- **No polyglot tax.** Doing Cardano natively in Rust avoids running a Rust
  sidecar from a Go node (gRPC contract management, two CI pipelines, two
  security reviews).
- **Hiring pool concern doesn't apply here.** Target deployers are African
  banks where Java / .NET / COBOL dominate; Go is not significantly more
  familiar than Rust. Operators run the container, they don't read source.
  For TESOBE/OBP maintainers, Rust is overrepresented in fintech/blockchain.

**What we accept as the cost:**

- Slower compile times (mitigated by workspace structure and crate split).
- Steeper iteration in early development (front-loaded; we eat it once).
- Cross-compilation friction (managed via `cross` + musl static linking).
- Larger supply-chain surface (one-time crate audit at v1).

## Crate layout

Cargo workspace, two crates:

```
OBP-Bank-Node/
├── Cargo.toml                  # workspace root
├── Cargo.lock
├── crates/
│   ├── obp-bank-node/          # main binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── config.rs       # figment-based layered config
│   │       ├── rest/           # axum south-side API (v5.0.0, v5.1.0)
│   │       ├── amqp/           # lapin RabbitMQ consumer (Interface C)
│   │       ├── outbox/         # sqlx SQLite outbox (durability)
│   │       ├── delivery/       # 4 CBS delivery modes
│   │       │   ├── webhook_obp.rs
│   │       │   ├── webhook_iso20022.rs
│   │       │   ├── database.rs
│   │       │   └── file_drop.rs
│   │       ├── obp_api/        # reqwest client + OAuth2 (Interface B)
│   │       └── health.rs
│   └── obp-blockchain/         # connector trait + impls (Interface D)
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs          # BlockchainConnector trait, chain-agnostic types
│           ├── cardano/        # CardanoConnector impl (pallas + CML + Ogmios)
│           ├── mock.rs         # MockConnector for tests
│           └── ethereum/       # placeholder for future
├── docker/
│   ├── docker-compose.cardano.yml
│   └── cardano-bootstrap.sh
├── docs/
└── obp-bank-node-config.yaml.example
```

Two crates (not more) because Cardano dependencies are heavy and slow to
compile. Isolating them in `obp-blockchain` lets `cargo build -p obp-bank-node`
skip recompiling chain code when iterating on REST/AMQP. Five+ crates would
add ceremony without changing compile times meaningfully at this size.

## Library picks

| Concern             | Crate                                          | Notes                                       |
|---------------------|------------------------------------------------|---------------------------------------------|
| Async runtime       | `tokio` (multi-thread)                         | Forced by every other async crate           |
| HTTP server         | `axum` + `tower-http`                          | Tokio-native, mainstream                    |
| HTTP client         | `reqwest`                                      | For OBP API client + webhook delivery       |
| AMQP                | `lapin`                                        | Tokio-native; replaces Go `amqp091-go`      |
| SQL (SQLite + PG)   | `sqlx`                                         | Async + compile-time query checking         |
| Config              | `figment`                                      | YAML + env + CLI layered                    |
| Logging / tracing   | `tracing` + `tracing-subscriber`               | JSON output for telemetry                   |
| Metrics             | `metrics` + `metrics-exporter-prometheus`      | Prometheus scrape endpoint                  |
| Errors (library)    | `thiserror`                                    | Typed error enums                           |
| Errors (binary)     | `anyhow`                                       | Main / glue layer                           |
| Cardano codecs      | `pallas`                                       | Mini-protocols, CBOR, address handling      |
| Cardano tx build    | `cardano-multiplatform-lib`                    | Successor to `cardano-serialization-lib`    |
| Ogmios client       | custom thin client over `tokio-tungstenite`    | No mature off-the-shelf Rust Ogmios client  |
| File-drop delivery  | `notify` (optional)                            | Filesystem watcher if we need it            |

## Connector layer pattern

Blockchain interaction sits behind a Rust trait:

```rust
#[async_trait]
pub trait BlockchainConnector: Send + Sync {
    async fn write_promise(&self, p: &PromiseRecord) -> Result<TxReference>;
    async fn write_settlement(&self, s: &SettlementRecord) -> Result<TxReference>;
    async fn write_exception(&self, e: &ExceptionRecord) -> Result<TxReference>;
    async fn confirm(&self, r: &TxReference) -> Result<ConfirmationStatus>;
}
```

Selected by config:

```yaml
blockchain:
  type: cardano   # cardano | ethereum | bitcoin | mock
  cardano:
    ogmios_url: "ws://localhost:1337"
    network: "preprod"
    wallet_signing_key_path: "./secrets/cardano.skey"
    wallet_address: "addr_test1q..."
```

**Rules for the trait:**

- Keep chain-agnostic — `TxReference`, `PromiseRecord`, etc. are not allowed
  to leak chain-specific types like `pallas::TxHash`.
- Each new chain is real implementation work (UTxO vs account, metadata vs
  smart contract). The trait keeps the *contract* uniform; it doesn't make
  chain-swapping cheap.
- `MockConnector` is the first impl built — used for unit tests and for
  developing the node without a chain dependency.

## Cardano backend

The `CardanoConnector` impl talks to a **real `cardano-node`** running on the
**preprod testnet**, with **Ogmios** in front of it for JSON-RPC over
WebSocket. Cardano-node syncs from genesis (one-time, ~2–4 h on preprod),
then resumes in seconds on every restart.

Preprod (not preview) was chosen because preview's Mithril aggregator
(`pre-release-preview`) requires pre-release client builds that aren't
published with per-version Docker tags — only `latest` (released) and
`unstable` (developer-only) tags exist on GHCR. Preprod uses released
tooling, mirrors mainnet config, and is the recommended staging environment
for anything heading to mainnet. The only material difference vs preview is
block time (~20s instead of ~2s), which doesn't affect OBP Bank Node's
metadata-only transaction flow.

Mithril (the fast-bootstrap snapshot protocol) was attempted but ran into
client/aggregator version skew that the published Docker tags didn't cover.
Genesis sync is the reliable path for now. Mithril can be revisited later
when versioning is pinned to a known-good pair; the wrestling isn't worth it
for a one-time-per-developer cost.

```
Bank Node (Rust)
   │ WebSocket / JSON-RPC
   ▼
Ogmios
   │ Unix socket / Ouroboros mini-protocols
   ▼
cardano-node (preprod testnet)
```

Promise / Settlement / Exception records are written as **metadata-only
transactions** — the simplest possible Cardano tx (one UTxO input, one change
output back to self, metadata block carrying the record JSON, signed locally
with the bank's signing key, submitted via Ogmios).

**Why not Blockfrost.** Blockfrost is a SaaS in the critical settlement
path — fine for prototypes but a non-starter for a bank's production
deployment (audit, SLA, counterparty risk). The path is local
`cardano-node`-or-equivalent (banks can also use managed providers like
Demeter.run later).

**Why Ogmios, not `cardano-cli` subprocess.** `cardano-cli` is file-oriented
(tempfile juggling per call), one-shot (no streaming, no subscriptions),
errors-as-stderr-text. Ogmios gives typed JSON-RPC, real chain-tip
subscriptions for tx confirmation tracking, no subprocess overhead, and the
signing key stays in the Rust process.

## Reserved future moves

These are explicit decisions to **not do** now, with the conditions for
revisiting:

- **Promote `CardanoConnector` to a sidecar process.** Justified when (a)
  signing-key isolation in a separate address space is required for security
  review, or (b) a second blockchain implementation needs independent release
  cadence. Contract should be **gRPC over Unix domain socket** (typed proto,
  server-streaming for chain-tip subscriptions).
- **Drop Ogmios, talk Ouroboros mini-protocols directly via `pallas`.** Cuts
  one process. Defer until Ogmios proves to be a bottleneck or operational
  burden.
- **Add Kupo (UTxO indexer).** Not needed for Promise writes; consider when
  query patterns get more complex (multi-address aggregation, historic
  scans).

## Deployment shape

A bank deploys via Docker Compose. Three services in the minimum stack:

1. **`obp-bank-node`** — the single Rust binary.
2. **`cardano-node` + Ogmios** — combined image (`cardanosolutions/cardano-node-ogmios`).
3. **`rabbitmq`** — message broker for Interface C.

Plus, optionally, the bank's own Postgres if using the database delivery
mode. Mithril bootstrap runs as a one-shot init step before `cardano-node`
first starts.

## Open architecture decisions

Not yet decided; flag in PRs when these get touched:

- Workspace member layout if/when a third crate is justified (e.g., shared
  types between `obp-bank-node` and `obp-blockchain`).
- Whether to embed the OBP API client as a fourth crate so it can be reused
  by other TESOBE Rust services.
- HSM integration path for the Cardano signing key (probably PKCS#11 — but
  hardware vendor not chosen).

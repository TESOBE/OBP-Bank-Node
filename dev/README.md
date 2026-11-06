# Two Bank Nodes on localhost

Everything in `dev/` is for running the full Open Corridor round trip on one machine: two banks,
each with its own Bank Node and UI, one OBP-API between them, RabbitMQ, and a real `cardano-node`
on the Cardano **preprod** testnet. The only stand-in is the banks' core banking system
(`cbs_stub.py`).

```
 UI A :8091 ──► Node A :8088 ─┐                              ┌─ Node B :8089 ◄── UI B :8092
 (bank A)       (debtor)      │  REST (DirectLogin)          │  (creditor)        (bank B)
                              ├────────► OBP-API :8080 ◄─────┤
                              │  RabbitMQ :5672, one vhost   │
                              │  per bank (/bank.<bank_id>)  │
                              │                              │
                              └──► Ogmios :1337 ◄────────────┘──► CBS stub :9009
                                   cardano-node (preprod)         (credit notifications)
```

Before running it, read [`../DOCS/TLDR.md`](../DOCS/TLDR.md), then
[`ARCHITECTURE.md`](../DOCS/ARCHITECTURE.md) and [`INTERFACES.md`](../DOCS/INTERFACES.md).
[`FIVE_STEPS.md`](../DOCS/FIVE_STEPS.md) explains, in plain language, what the UI shows at each step.

## What runs where

| What | Where | Notes |
|---|---|---|
| OBP-API | `http://localhost:8080` | one instance for both banks, `open_corridor_enabled=true`, on Postgres |
| OBP-OIDC | `http://localhost:9000` | only for logging in to the UIs' `/setup` pages |
| RabbitMQ | `:5672`, management `:15672` (guest/guest) | vhosts `/bank.<bank_id>`, users `bank_a_node` / `bank_b_node` |
| cardano-node + Ogmios | `ws://localhost:1337` | preprod; `docker/cardano-bootstrap.sh` |
| Node A | `http://localhost:8088` | `dev/node-a.yaml`, bank `rt.bank.a3`, OBP user `rt.node.a`, wallet `secrets/cardano.skey` |
| Node B | `http://localhost:8089` | `dev/node-b.yaml`, bank `rt.bank.b3`, OBP user `rt.node.b`, wallet `secrets/creditor.skey` |
| UI A / UI B | `http://localhost:8091` / `:8092` | `dev/ui-a.yaml` / `ui-b.yaml`; each fronts its own node only |
| CBS stub | `http://localhost:9009` | writes what it receives to `dev/data/cbs_received.jsonl` |

Both nodes run on Cardano: either side can be the debtor and settle. The bank ids end in a
generation number (`a3`, `b3`); `dev/bump_banks.sh` moves to a fresh pair (see below).

The nodes log in to OBP-API with DirectLogin, as the service users `rt.node.a` and `rt.node.b`.
They will move to OAuth2 client credentials (one Consumer per node) once OBP-API can grant
account access to a Consumer.

## One-time setup

### 1. Tools

Rust ([rustup](https://rustup.rs)), Python 3 with `cryptography` (`pip install cryptography`),
`curl`, `psql`, and Docker or Podman with compose.

### 2. OBP-API and OBP-OIDC

Run [OBP-API](https://github.com/OpenBankProject/OBP-API) (`develop`) on Postgres, and
[OBP-OIDC](https://github.com/OpenBankProject/OBP-OIDC) next to it. In OBP-API's props
(`obp-api/src/main/resources/props/default.props`):

- `open_corridor_enabled=true`
- `db.url=jdbc:postgresql://...` (not H2: `setup_obp.sh` marks the node users' emails as
  validated directly in Postgres)
- the `oidc_operator_*` props that OBP-OIDC's README describes; `create_consumers.sh` logs in
  with them
- your own user in `super_admin_user_ids`; `setup_obp.sh` logs in as that user

Restart OBP-API after changing props. A props change made after the build is not in a jar
that was built before it.

### 3. RabbitMQ

With the management plugin, which `setup_rabbitmq.sh` uses:

```sh
docker run -d --name rabbitmq -p 5672:5672 -p 15672:15672 rabbitmq:3-management
```

(or a system RabbitMQ with `sudo rabbitmq-plugins enable rabbitmq_management`).

### 4. Cardano: cardano-node + Ogmios from a Mithril snapshot

```sh
docker/cardano-bootstrap.sh up            # create the stack
docker/cardano-bootstrap.sh mithril -y    # replace its chain DB with a certified snapshot
docker/cardano-bootstrap.sh status        # repeat until networkSynchronization is 1.0
```

[Mithril](https://mithril.network) downloads a signed snapshot of the preprod chain (about 20 GB,
verified against Mithril's keys) instead of replaying the chain from genesis, which takes 12 to
20 hours or more. After the download the node only catches up the last few hours. The snapshot is
made by a newer cardano-node than our image runs. If the logs say the ledger is being replayed,
the node is rebuilding its ledger state from the downloaded blocks. That is slower, but it is
not stuck and needs no network sync. Allow 30–50 GB of disk.

The chain DB stays in a Docker volume, so after `docker/cardano-bootstrap.sh down` (which frees
about 5 GB of memory), the next `up` resumes in seconds. More in
[`../docker/README.md`](../docker/README.md).

### 5. Wallets and test ADA

```sh
python3 dev/wallets.py              # creates secrets/cardano.skey + creditor.skey, prints both addresses
python3 dev/wallets.py --balance    # check the balances (works while Ogmios is still syncing)
```

Paste each address into the [Cardano testnet faucet](https://docs.cardano.org/cardano-testnets/tools/faucet)
(network **Preprod**). With an IOG faucet API key in `CARDANO_FAUCET_API_KEY`, `wallets.py`
requests the ADA itself. Keys never leave `secrets/` (gitignored), and an existing key is never
overwritten.

### 6. `dev/local.env` and the OBP Consumers

```sh
cp dev/local.env.example dev/local.env    # set RT_OBP_PROPS_FILE, RT_SA_USER, RT_SA_PASS
dev/create_consumers.sh                   # creates the 3 Consumers, writes their values to dev/local.env
```

The three Consumers are `obp-bank-node-roundtrip` (the DirectLogin consumer key for the node
users), and `obp-bank-node-app-a` / `-b` (the UIs' `/setup` login, with redirect URLs
`http://localhost:8091/setup/callback` and `:8092/setup/callback`). `dev/local.env` is gitignored.

## Every time

```sh
dev/up.sh check     # what is missing
dev/up.sh           # set up OBP-API + RabbitMQ, start the CBS stub, both nodes and both UIs
dev/up.sh status
dev/up.sh stop
```

`dev/up.sh` runs, in order:

1. Waits for OBP-API and RabbitMQ.
2. Runs `setup_rabbitmq.sh`: the per-bank vhosts and users.
3. Runs `setup_obp.sh`: banks, routing schemes, the node users and their roles, the corridor and
   settlement accounts, FX rates and broker registrations. It writes fresh DirectLogin tokens to
   `dev/env.sh`.
4. Starts `cbs_stub.py`.
5. Builds the nodes and UIs, then starts all four in the background, with logs in `dev/data/logs/`.

Everything is idempotent, so re-running is safe. The tokens in `dev/env.sh` expire, so run
`dev/up.sh setup` again if a node starts failing with OBP-API authentication errors.

Prefer one terminal per process? Run `dev/up.sh setup` and `dev/up.sh cbs` first, then
`dev/start_node_a.sh`, `dev/start_node_b.sh`, `dev/start_ui_a.sh` and `dev/start_ui_b.sh`, each
in its own terminal.

Then, the first time for each bank generation:

1. Open `http://localhost:8091/setup`, log in as your OBP-API super admin, and **Apply** the
   missing items until everything is green. These are the per-currency corridor accounts and the
   bank's CARDANO settlement address on `OBP-INCOMING-SETTLEMENT-ACCOUNT`. Do the same on
   `http://localhost:8092/setup`. Each page sets up its own bank only, as two real banks would.
2. Send a payment from UI A (`http://localhost:8091`), and watch it arrive in UI B.

`dev/run_roundtrip.sh [amount]` does the same from the shell. It sends KES from A to B, waits
for the promise on chain, settles the pair, waits for finality (2 blocks, about 40 s), and shows
what node B and the CBS stub received. It prints a cardanoscan link for each transaction.

## Files

| File | What it does |
|---|---|
| `up.sh` | brings everything up in order, and has the `check` / `setup` / `cbs` / `status` / `stop` steps |
| `create_consumers.sh` | creates the three OBP Consumers and writes them to `local.env` |
| `setup_rabbitmq.sh`, `setup_obp.sh` | RabbitMQ and OBP-API data for the current bank pair, and `env.sh` tokens |
| `start_node_a.sh` / `_b.sh`, `start_ui_a.sh` / `_b.sh` | start one process each (`cargo run`) |
| `node-a.yaml` / `node-b.yaml`, `ui-a.yaml` / `ui-b.yaml` | the nodes' and UIs' config |
| `wallets.py` | Cardano wallets, addresses, faucet, balances |
| `cbs_stub.py` | stand-in core banking system: books credits, or refuses a beneficiary whose name contains `_invalid` |
| `run_roundtrip.sh` | one full payment, from the shell |
| `bump_banks.sh` | moves to a fresh bank pair (`rt.bank.a3` to `a4`), so a demo starts with empty tables |
| `STATUS.md` | history: the log of the first round trip (July 2026), not current instructions |

Runtime state is in `dev/data/` (outbox, evidence and settlement databases per node, logs,
CBS receipts). It is gitignored. The databases hold commitment salts and originator data, so
don't share them.

## Starting over with fresh banks

```sh
dev/up.sh stop
dev/bump_banks.sh          # rt.bank.aN/bN -> aN+1/bN+1, new dev/data/node-*N+1 dirs, RabbitMQ vhosts
dev/up.sh                  # then Apply on both /setup pages again
```

Nothing is deleted: the old banks stay in OBP-API and their data stays in the old
`dev/data/node-*N` directories. The wallets and node users carry over.

## When something is wrong

- `dev/up.sh check` first.
- A node's log is in `dev/data/logs/node-a.log` / `node-b.log`. The node's own status:
  `curl localhost:8088/health`.
- "Invalid client" on a `/setup` login: the UI Consumer is missing or has a different redirect URL.
  Run `dev/create_consumers.sh` again.
- Credit notifications stuck in PENDING: the bank's RabbitMQ vhost was missing when the node
  started. Run `dev/setup_rabbitmq.sh`, then restart the node.
- Settlement waits forever: Ogmios is not synced (`docker/cardano-bootstrap.sh status`), or the
  debtor's wallet has no tADA (`python3 dev/wallets.py --balance`).

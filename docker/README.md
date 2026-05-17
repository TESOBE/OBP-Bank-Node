# Local Cardano stack (`docker/`)

Runs a `cardano-node` + Ogmios container for OBP Bank Node development.

- `docker-compose.cardano.yml` — the compose definition
- `cardano-bootstrap.sh` — convenience wrapper (`up`, `status`, `logs`, `down`, `nuke`)

Network: **preprod** testnet. See `../ARCHITECTURE.md` for why preprod and not preview, and why no Mithril fast-bootstrap.

## What the sync is doing

On first `up`, cardano-node starts with an empty chain DB and walks the chain
from genesis, downloading and validating every block since June 2022. It
progresses through the historical Cardano eras in order:

`Byron → Shelley → Allegra → Mary → Alonzo → Babbage → Conway`

This is the **safe, no-trust-required** way to bring a node up: every block
is verified against the consensus rules, every UTxO transition checked. The
node trusts no one — it derives chain state from genesis.

**Time:** typically **2–4 hours** end-to-end on preprod with an SSD and
reasonable bandwidth. Most of the time is CPU-bound block validation rather
than network download.

**Disk:** the chain DB grows to roughly **30–50 GB** by the time it's
caught up to tip.

**RAM:** ~2–4 GB during sync, settles around 2 GB once at tip.

**CPU:** heavy during sync (one or two cores pegged). Light once caught up —
just enough to validate ~1 new block per 20 seconds.

After the first sync, the chain DB persists in a Docker named volume
(`docker_cardano-db`). Subsequent `up` commands resume in seconds; the node
catches up from wherever it left off, which is usually trivial (minutes or
less of missed chain).

## Checking progress

```bash
./cardano-bootstrap.sh status
```

Reports container state plus the Ogmios `/health` payload. The interesting
field is `networkSynchronization` — a float between 0 and 1. `0.0179` means
1.79% synced.

```bash
./cardano-bootstrap.sh logs
```

Tails the cardano-node + Ogmios logs. During sync you'll see continuous
`Chain extended, new tip: ...` lines (each one a block validated and added).
The `currentEra` in the Ogmios health output also climbs through the eras
listed above — a useful coarse progress indicator.

## Pausing the sync

You have two options, depending on whether you want to keep the work done
so far.

### Stop, keep the chain DB (recommended)

```bash
./cardano-bootstrap.sh down
```

Stops the container gracefully (cardano-node flushes its state to disk),
removes the container, but **keeps the `docker_cardano-db` volume on disk**.
Disk usage stays where it was. Next `./cardano-bootstrap.sh up` picks up
exactly where it left off — usually within seconds of starting, it'll be
validating blocks from your previous position.

This is the right command for "I want my laptop fan to stop / I need the
CPU for something else / I'm going on holiday."

### Stop AND delete everything

```bash
./cardano-bootstrap.sh nuke
```

Stops the container *and* deletes the `docker_cardano-db` volume, reclaiming
the 30–50 GB of disk. Next `up` re-syncs from genesis (~2–4 h again).

Use this when:
- You want to reclaim the disk space permanently.
- The chain DB has gotten into a weird state.
- You're switching networks (preprod ↔ mainnet etc.) — different chains
  can't share a DB.

The script will prompt for `y/N` confirmation before nuking.

### Interrupting safely is fine

You can `down` (or even `Ctrl-C` the docker daemon) at any point during
sync without corrupting anything. Cardano-node has crash-recovery built in:
each block is committed atomically, so the worst case on interruption is
losing the partially-validated block in flight — a few seconds of work,
re-done on next start. No filesystem-level repair tools are needed.

## Connecting code to the running node

The Bank Node (when it exists) and any `cardano-cli` / library calls reach
the node through **Ogmios** on the host:

- WebSocket / JSON-RPC: `ws://localhost:1337`
- HTTP health endpoint:  `http://localhost:1337/health`

Don't connect to cardano-node directly — its Unix socket is inside the
container, and Ogmios is the documented public surface anyway.

## Reset checklist for a clean state

If anything looks wedged, the fast reset is:

```bash
./cardano-bootstrap.sh nuke   # confirm with y
./cardano-bootstrap.sh up
```

That's a ~2–4 h cost; the alternative is debugging volume / config drift,
which is usually worse.

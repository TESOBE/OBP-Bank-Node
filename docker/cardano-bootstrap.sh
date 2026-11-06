#!/usr/bin/env bash
#
# Manage the local Cardano stack (cardano-node + Ogmios) for OBP Bank Node
# development. Network: PREPROD. Sync: from genesis (12-20 h or more on a
# first run) or from a Mithril snapshot (`mithril`, about an hour); resumes in
# seconds on every restart afterwards.
#
# Usage:
#   ./docker/cardano-bootstrap.sh up       # start the stack and return
#   ./docker/cardano-bootstrap.sh status   # show sync progress
#   ./docker/cardano-bootstrap.sh logs     # tail cardano-node-ogmios logs
#   ./docker/cardano-bootstrap.sh down     # stop, keep chain DB
#   ./docker/cardano-bootstrap.sh nuke     # stop and DELETE chain DB
#   ./docker/cardano-bootstrap.sh mithril  # replace the chain DB with a certified
#                                          # Mithril snapshot (fast bootstrap)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.cardano.yml"
OGMIOS_HEALTH_URL="http://localhost:1337/health"

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "ERROR: '$1' is required but not installed." >&2; exit 1; }
}

# Podman and Docker both work; override with CONTAINER_ENGINE=podman|docker.
if [ -z "${CONTAINER_ENGINE:-}" ]; then
  if command -v docker >/dev/null 2>&1; then CONTAINER_ENGINE=docker
  elif command -v podman >/dev/null 2>&1; then CONTAINER_ENGINE=podman
  else echo "ERROR: neither 'docker' nor 'podman' is installed." >&2; exit 1
  fi
fi
"$CONTAINER_ENGINE" compose version >/dev/null 2>&1 || { echo "ERROR: '$CONTAINER_ENGINE compose' is required (compose v2 / podman-compose)." >&2; exit 1; }
require curl

compose() {
  "$CONTAINER_ENGINE" compose -f "$COMPOSE_FILE" "$@"
}

# Mithril (https://mithril.network) certifies snapshots of the preprod chain DB,
# so a node can start from one instead of syncing from genesis.
MITHRIL_CLIENT_IMAGE="${MITHRIL_CLIENT_IMAGE:-ghcr.io/intersectmbo/mithril-client:2642.0-e85df3d}"
MITHRIL_AGGREGATOR="${MITHRIL_AGGREGATOR:-https://aggregator.release-preprod.api.mithril.network/aggregator}"
MITHRIL_KEYS_URL="https://raw.githubusercontent.com/IntersectMBO/mithril/main/mithril-infra/configuration/release-preprod"

# The Docker volume behind the compose file's `cardano-db` (named after the
# compose project, `docker_cardano-db` by default).
db_volume() {
  local name
  name=$(compose config --format json 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["volumes"]["cardano-db"]["name"])' 2>/dev/null || true)
  echo "${name:-$(basename "$SCRIPT_DIR")_cardano-db}"
}

cmd="${1:-up}"

case "$cmd" in
  up)
    echo "==> Starting Cardano stack (preprod testnet)"
    compose up -d
    echo
    echo "==> Stack is running in the background."
    echo "    Initial sync from genesis takes 12-20 hours or more;"
    echo "    '$0 mithril' starts from a certified snapshot instead."
    echo
    echo "    Check progress:"
    echo "      $0 status"
    echo
    echo "    Tail logs:"
    echo "      $0 logs"
    echo
    echo "    Stop:"
    echo "      $0 down"
    ;;

  status)
    if ! compose ps --quiet cardano-node-ogmios | grep -q .; then
      echo "Stack is not running. Start with: $0 up"
      exit 1
    fi
    echo "==> Container state"
    compose ps
    echo
    echo "==> Ogmios health"
    if response=$(curl -fsS "$OGMIOS_HEALTH_URL" 2>/dev/null); then
      echo "$response" | python3 -m json.tool 2>/dev/null || echo "$response"
      # Try to extract networkSynchronization for a clear summary line
      sync=$(echo "$response" | grep -oE '"networkSynchronization":[[:space:]]*[0-9.]+' | grep -oE '[0-9.]+$' || true)
      if [ -n "$sync" ]; then
        echo
        LC_NUMERIC=C printf "==> Network sync: %.2f%%\n" "$(echo "$sync * 100" | bc -l 2>/dev/null || echo "$sync")"
      fi
    else
      echo "Ogmios not yet reachable on $OGMIOS_HEALTH_URL"
      echo "(cardano-node may still be starting up — check logs)"
    fi
    ;;

  logs)
    compose logs -f --tail=100 cardano-node-ogmios
    ;;

  down)
    echo "==> Stopping Cardano stack (chain DB preserved)"
    compose down
    ;;

  nuke)
    echo "==> Stopping stack AND deleting chain DB volume"
    read -r -p "    Are you sure? Next 'up' will re-sync from genesis (12-20 h; or run 'mithril', about an hour). [y/N] " ans
    case "$ans" in
      y|Y|yes|YES)
        compose down -v
        echo "    Chain DB deleted."
        ;;
      *)
        echo "    Aborted."
        exit 1
        ;;
    esac
    ;;

  mithril)
    # Download into a folder inside the chain DB volume, and swap it in only
    # once the download is verified: a failed download keeps the current DB.
    vol=$(db_volume)
    echo "==> Fast bootstrap from a Mithril snapshot (preprod)"
    echo
    echo "    This will:"
    echo "      1. stop the Cardano stack"
    echo "      2. download the latest certified preprod snapshot (about 20 GB) into"
    echo "         volume $vol, and check it against Mithril's signatures"
    echo "      3. replace the chain DB in that volume with it (the current DB is deleted)"
    echo "      4. start the stack again"
    echo
    if [ "${2:-}" != "-y" ]; then
      read -r -p "    Continue? [y/N] " ans
      case "$ans" in
        y|Y|yes|YES) ;;
        *) echo "    Aborted."; exit 1 ;;
      esac
    fi

    echo
    echo "==> Fetching Mithril's preprod verification keys"
    genesis_key=$(curl -fsSL "$MITHRIL_KEYS_URL/genesis.vkey")
    ancillary_key=$(curl -fsSL "$MITHRIL_KEYS_URL/ancillary.vkey")

    echo "==> 1/4 Stopping the Cardano stack"
    compose down

    echo "==> 2/4 Downloading the snapshot into $vol (this takes a while)"
    "$CONTAINER_ENGINE" volume create "$vol" >/dev/null
    # A download interrupted earlier leaves .mithril behind; start it afresh.
    "$CONTAINER_ENGINE" run --rm -v "$vol:/data" docker.io/library/busybox:1.36 rm -rf /data/.mithril
    "$CONTAINER_ENGINE" run --rm --user 0 -v "$vol:/data" \
      -e AGGREGATOR_ENDPOINT="$MITHRIL_AGGREGATOR" \
      -e GENESIS_VERIFICATION_KEY="$genesis_key" \
      -e ANCILLARY_VERIFICATION_KEY="$ancillary_key" \
      "$MITHRIL_CLIENT_IMAGE" \
      cardano-db download --include-ancillary --download-dir /data/.mithril latest

    echo "==> 3/4 Replacing the chain DB with the snapshot"
    "$CONTAINER_ENGINE" run --rm -v "$vol:/data" docker.io/library/busybox:1.36 sh -euc '
      cd /data
      if [ -d .mithril/db/immutable ]; then src=.mithril/db
      elif [ -d .mithril/immutable ]; then src=.mithril
      else echo "ERROR: no immutable/ folder in the download; the current chain DB is untouched." >&2; exit 1
      fi
      find . -mindepth 1 -maxdepth 1 ! -name .mithril -exec rm -rf {} +
      mv "$src"/* .
      rm -rf .mithril
      ls'

    echo "==> 4/4 Starting the Cardano stack"
    compose up -d
    echo
    echo "==> Done. The node now catches up the last few hours of the chain."
    echo "    Check progress:  $0 status   (networkSynchronization reaches 1.0)"
    echo "    Watch the node:  $0 logs"
    echo
    echo "    The snapshot is made by cardano-node 11.1.3; this image runs 11.0.1. If the"
    echo "    logs say the ledger is being replayed from genesis, the node could not"
    echo "    read the snapshot's ledger state and rebuilds it from the downloaded"
    echo "    blocks: slower, but no network sync."
    ;;

  *)
    echo "Usage: $0 [up|status|logs|down|nuke|mithril [-y]]" >&2
    exit 2
    ;;
esac

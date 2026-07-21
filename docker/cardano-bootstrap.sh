#!/usr/bin/env bash
#
# Manage the local Cardano stack (cardano-node + Ogmios) for OBP Bank Node
# development. Network: PREPROD. Sync: from genesis (2-4 h on first run,
# resumes in seconds on every restart afterwards).
#
# Usage:
#   ./docker/cardano-bootstrap.sh up       # start the stack and return
#   ./docker/cardano-bootstrap.sh status   # show sync progress
#   ./docker/cardano-bootstrap.sh logs     # tail cardano-node-ogmios logs
#   ./docker/cardano-bootstrap.sh down     # stop, keep chain DB
#   ./docker/cardano-bootstrap.sh nuke     # stop and DELETE chain DB

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

cmd="${1:-up}"

case "$cmd" in
  up)
    echo "==> Starting Cardano stack (preprod testnet)"
    compose up -d
    echo
    echo "==> Stack is running in the background."
    echo "    Initial sync from genesis takes ~2-4 hours."
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
    read -r -p "    Are you sure? Next 'up' will re-sync from genesis (~2-4 h). [y/N] " ans
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

  *)
    echo "Usage: $0 [up|status|logs|down|nuke]" >&2
    exit 2
    ;;
esac

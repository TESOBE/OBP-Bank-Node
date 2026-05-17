#!/usr/bin/env bash
#
# Bring up the local Cardano stack (cardano-node + Ogmios) for OBP Bank Node
# development, bootstrapping from a Mithril snapshot.
#
# First run: ~30 min (Mithril snapshot download + node replay).
# Subsequent runs: seconds (snapshot already on disk, node resumes).
#
# Usage:
#   ./docker/cardano-bootstrap.sh            # start the stack
#   ./docker/cardano-bootstrap.sh down       # stop and keep the chain DB
#   ./docker/cardano-bootstrap.sh nuke       # stop and DELETE the chain DB
#                                            #   (forces a full re-bootstrap)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.cardano.yml"
OGMIOS_HEALTH_URL="http://localhost:1337/health"
WAIT_TIMEOUT_SECS=2400  # 40 min — covers Mithril download + node replay

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "ERROR: '$1' is required but not installed." >&2; exit 1; }
}

require docker
docker compose version >/dev/null 2>&1 || { echo "ERROR: 'docker compose' (v2) is required." >&2; exit 1; }
require curl

cmd="${1:-up}"

case "$cmd" in
  up)
    echo "==> Starting Cardano stack (preview testnet)"
    docker compose -f "$COMPOSE_FILE" up -d

    echo "==> Streaming Mithril bootstrap progress (Ctrl-C is safe — stack stays up)"
    docker compose -f "$COMPOSE_FILE" logs -f mithril-bootstrap &
    LOGS_PID=$!

    echo "==> Waiting for Ogmios health endpoint (timeout: ${WAIT_TIMEOUT_SECS}s)"
    deadline=$(( $(date +%s) + WAIT_TIMEOUT_SECS ))
    until curl -fsS "$OGMIOS_HEALTH_URL" >/dev/null 2>&1; do
      if [ "$(date +%s)" -gt "$deadline" ]; then
        echo "ERROR: Ogmios did not become healthy within ${WAIT_TIMEOUT_SECS}s." >&2
        echo "Check logs: docker compose -f $COMPOSE_FILE logs cardano-node-ogmios" >&2
        kill "$LOGS_PID" 2>/dev/null || true
        exit 1
      fi
      sleep 5
    done

    kill "$LOGS_PID" 2>/dev/null || true
    echo
    echo "==> READY"
    echo "    Ogmios WebSocket: ws://localhost:1337"
    echo "    Ogmios HTTP:      $OGMIOS_HEALTH_URL"
    echo
    echo "    Verify chain tip:"
    echo '      curl -sS http://localhost:1337/health | jq .'
    echo
    echo "    Stop stack:"
    echo "      $0 down"
    ;;

  down)
    echo "==> Stopping Cardano stack (chain DB preserved)"
    docker compose -f "$COMPOSE_FILE" down
    ;;

  nuke)
    echo "==> Stopping stack AND deleting chain DB volume"
    read -r -p "    Are you sure? Next 'up' will re-download the Mithril snapshot (~30 min). [y/N] " ans
    case "$ans" in
      y|Y|yes|YES)
        docker compose -f "$COMPOSE_FILE" down -v
        echo "    Chain DB deleted."
        ;;
      *)
        echo "    Aborted."
        exit 1
        ;;
    esac
    ;;

  *)
    echo "Usage: $0 [up|down|nuke]" >&2
    exit 2
    ;;
esac

#!/usr/bin/env bash
# Build and run the OBP Bank Node for local development.
#
# Usage:
#   ./start.sh                       use ./obp-bank-node-config.yaml
#   ./start.sh -c path/to/config     use a different config file
#   ./start.sh -h                    show this help
#
# On first run, if no config file exists, this script bootstraps one from
# obp-bank-node-config.yaml.example with dev-friendly local paths
# (./outbox/obp-bank-node.db instead of /app/outbox/obp-bank-node.db) and
# tells you to edit it — at minimum, set obp_bank_node.local_secret.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

CONFIG="./obp-bank-node-config.yaml"
EXAMPLE="./obp-bank-node-config.yaml.example"
BIN="./bin/obp-bank-node"

usage() {
    cat <<'EOF'
Build and run the OBP Bank Node for local development.

Usage:
  ./start.sh                       use ./obp-bank-node-config.yaml
  ./start.sh -c path/to/config     use a different config file
  ./start.sh -h                    show this help

On first run, if no config file exists, this script bootstraps one from
obp-bank-node-config.yaml.example with dev-friendly local paths
(./outbox/obp-bank-node.db instead of /app/outbox/obp-bank-node.db) and
tells you to edit it — at minimum, set obp_bank_node.local_secret.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -c|--config) CONFIG="$2"; shift 2 ;;
        -h|--help)   usage; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done

# First-run bootstrap: copy the example, rewrite Docker-container paths to
# local equivalents, then ask the operator to edit the secret before running.
if [[ ! -f "$CONFIG" ]]; then
    if [[ ! -f "$EXAMPLE" ]]; then
        echo "Error: $CONFIG not found and no $EXAMPLE to bootstrap from" >&2
        exit 1
    fi
    sed \
        -e 's|/app/outbox/|./outbox/|g' \
        -e 's|/secrets/cardano.skey|./secrets/cardano.skey|g' \
        "$EXAMPLE" > "$CONFIG"
    echo "Created $CONFIG from $EXAMPLE."
    echo
    echo "Edit it before running again — at minimum:"
    echo "  - obp_bank_node.local_secret  (change from the placeholder)"
    echo "  - bank.bank_id / bank.account_id (your bank's values)"
    echo "  - obp_api / rabbitmq / cardano credentials (from registration)"
    exit 1
fi

# Warn (but don't block) if the secret is still the example placeholder.
if grep -q 'local_secret: "change-me-on-first-run"' "$CONFIG"; then
    echo "WARNING: obp_bank_node.local_secret in $CONFIG is still the default placeholder."
    echo
fi

# Make sure the local outbox dir exists so the SQLite store can open.
OUTBOX_PATH="$(awk '/^outbox:/{found=1; next} found && /path:/{gsub(/[" ]/,"",$2); print $2; exit}' "$CONFIG")"
if [[ -n "$OUTBOX_PATH" ]]; then
    mkdir -p "$(dirname "$OUTBOX_PATH")"
fi

echo "Building obp-bank-node..."
mkdir -p ./bin
go build -o "$BIN" ./cmd/obp-bank-node

echo "Starting OBP Bank Node (config=$CONFIG)"
exec "$BIN" -config "$CONFIG"

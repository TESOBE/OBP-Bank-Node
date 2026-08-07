#!/usr/bin/env bash
# Start the Bank Node App fronting node A — http://localhost:8091
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
exec env OBP_BANK_NODE_APP_CONFIG=dev/app-a.yaml \
    cargo run -p obp-bank-node-app

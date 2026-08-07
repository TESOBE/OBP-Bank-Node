#!/usr/bin/env bash
# Start the Bank Node App fronting node B — http://localhost:8092
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
exec env OBP_BANK_NODE_APP_CONFIG=dev/app-b.yaml \
    cargo run -p obp-bank-node-app

#!/usr/bin/env bash
# Start the Bank Node App fronting node A — http://localhost:8091
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
echo "Demo beneficiaries reachable from bank A (out-of-band knowledge, as a"
echo "real sender would have it — the app cannot and must not enumerate the"
echo "other bank's accounts):"
echo "  bank rt.bank.b2 (scheme OBP) — account settlement-b (scheme OBP)"
echo ""
exec env OBP_BANK_NODE_APP_CONFIG=dev/app-a.yaml \
    cargo run -p obp-bank-node-app

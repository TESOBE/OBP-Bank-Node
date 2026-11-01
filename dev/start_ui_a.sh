#!/usr/bin/env bash
# Start the Bank Node UI fronting node A — http://localhost:8091
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
echo "Demo beneficiaries reachable from bank A (out-of-band knowledge, as a"
echo "real sender would have it — the app cannot and must not enumerate the"
echo "other bank's accounts):"
echo "  bank rt.bank.b3 (scheme OBP) — account settlement-b (scheme OBP)"
echo ""
exec env OBP_BANK_NODE_UI_CONFIG=dev/ui-a.yaml \
    cargo run -p obp-bank-node-ui

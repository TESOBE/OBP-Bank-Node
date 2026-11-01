#!/usr/bin/env bash
# Start the Bank Node UI fronting node B — http://localhost:8092
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
echo "Demo beneficiaries reachable from bank B (out-of-band knowledge, as a"
echo "real sender would have it — the app cannot and must not enumerate the"
echo "other bank's accounts):"
echo "  bank rt.bank.a3 (scheme OBP) — account settlement-a (scheme OBP)"
echo ""
exec env OBP_BANK_NODE_UI_CONFIG=dev/ui-b.yaml \
    cargo run -p obp-bank-node-ui

#!/usr/bin/env bash
# Start the Bank Node UI fronting node B — http://localhost:8092
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
# This machine's OIDC client for the setup page, from commands/_helpers/sync_obp_consumers.sh.
# It overrides the one in dev/ui-b.yaml, which is registered only on the machine that wrote it.
[ -f "$DIR/local.env" ] && source "$DIR/local.env"
if [ -n "${UI_B_OAUTH_CLIENT_ID:-}" ]; then
    export OBP_BN_APP_OBP_API__OAUTH_CLIENT_ID="$UI_B_OAUTH_CLIENT_ID"
    export OBP_BN_APP_OBP_API__OAUTH_CLIENT_SECRET="${UI_B_OAUTH_CLIENT_SECRET:-}"
else
    echo "WARN: no UI_B_OAUTH_CLIENT_ID in dev/local.env: the setup login uses dev/ui-b.yaml's client."
    echo "      Run commands/_helpers/sync_obp_consumers.sh if it says 'Invalid client'."
fi
echo "Demo beneficiaries reachable from bank B (out-of-band knowledge, as a"
echo "real sender would have it — the app cannot and must not enumerate the"
echo "other bank's accounts):"
echo "  bank rt.bank.a3 (scheme OBP) — account settlement-a (scheme OBP)"
echo ""
exec env OBP_BANK_NODE_UI_CONFIG=dev/ui-b.yaml \
    cargo run -p obp-bank-node-ui

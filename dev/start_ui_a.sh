#!/usr/bin/env bash
# Copyright (C) 2025-2026 TESOBE GmbH
# SPDX-License-Identifier: AGPL-3.0-or-later
# Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.
# Start the Bank Node UI fronting node A — http://localhost:8091
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
# This machine's OIDC client for the setup page, from commands/_helpers/sync_obp_consumers.sh.
# It overrides the one in dev/ui-a.yaml, which is registered only on the machine that wrote it.
[ -f "$DIR/local.env" ] && source "$DIR/local.env"
if [ -n "${UI_A_OAUTH_CLIENT_ID:-}" ]; then
    export OBP_BN_APP_OBP_API__OAUTH_CLIENT_ID="$UI_A_OAUTH_CLIENT_ID"
    export OBP_BN_APP_OBP_API__OAUTH_CLIENT_SECRET="${UI_A_OAUTH_CLIENT_SECRET:-}"
else
    echo "WARN: no UI_A_OAUTH_CLIENT_ID in dev/local.env: the setup login uses dev/ui-a.yaml's client."
    echo "      Run commands/_helpers/sync_obp_consumers.sh if it says 'Invalid client'."
fi
echo "Demo beneficiaries reachable from bank A (out-of-band knowledge, as a"
echo "real sender would have it — the app cannot and must not enumerate the"
echo "other bank's accounts):"
echo "  bank rt.bank.b4 (scheme OBP) — account settlement-b (scheme OBP)"
echo ""
exec env OBP_BANK_NODE_UI_CONFIG=dev/ui-a.yaml \
    cargo run -p obp-bank-node-ui

#!/usr/bin/env bash
# Copyright (C) 2025-2026 TESOBE GmbH
# SPDX-License-Identifier: AGPL-3.0-or-later
# Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.
# Start the Bank Node UI fronting node C, on the second OBP host B (localhost:8081) — http://localhost:8094
# The setup page logs in through OBP host B's own OBP-OIDC (:9001) with Consumer 'obp-bank-node-app-c',
# whose key and secret commands/_helpers/obp_b_bank_node_setup.sh writes to dev/local-b.env.
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

cd "$REPO"
[ -f "$DIR/local-b.env" ] && source "$DIR/local-b.env"
if [ -n "${UI_C_OAUTH_CLIENT_ID:-}" ]; then
    export OBP_BN_APP_OBP_API__OAUTH_CLIENT_ID="$UI_C_OAUTH_CLIENT_ID"
    export OBP_BN_APP_OBP_API__OAUTH_CLIENT_SECRET="${UI_C_OAUTH_CLIENT_SECRET:-}"
else
    echo "WARN: no UI_C_OAUTH_CLIENT_ID in dev/local-b.env: the setup page cannot log in."
    echo "      Run commands/_helpers/obp_b_bank_node_setup.sh to create it. The rest of the UI works without it."
fi
exec env OBP_BANK_NODE_UI_CONFIG=dev/ui-c.yaml \
    cargo run -p obp-bank-node-ui

#!/usr/bin/env bash
# Copyright (C) 2025-2026 TESOBE GmbH
# SPDX-License-Identifier: AGPL-3.0-or-later
# Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.
# Start Bank Node C (rt.bank.c4, :8093, Cardano preprod) on the second OBP host, B (localhost:8081).
# The node is a Platform App: it calls OBP-API B as itself with OAuth2 client credentials from
# B's own OBP-OIDC, Consumer 'obp-bank-node-c' (dev/create_consumers.sh writes NODE_C_CLIENT_ID /
# NODE_C_CLIENT_SECRET to dev/local-b.env). Prereqs: OBP-API B on :8081, its OBP-OIDC on :9001,
# RabbitMQ with the /bank.rt.bank.c4 vhost, cardano-node + Ogmios on :1337, and dev/setup_obp.sh
# run once against B (Scopes, the node's view and its access). The helper
# commands/_helpers/obp_b_bank_node_setup.sh does that setup.
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

# shellcheck disable=SC1091
[ -f "$DIR/local-b.env" ] && source "$DIR/local-b.env"
[ -n "${NODE_C_CLIENT_ID:-}" ] && [ -n "${NODE_C_CLIENT_SECRET:-}" ] \
  || { echo "FATAL: no NODE_C_CLIENT_ID / NODE_C_CLIENT_SECRET in dev/local-b.env: run commands/_helpers/obp_b_bank_node_setup.sh" >&2; exit 1; }
cd "$REPO"
# dev/node-c.yaml names these variables (client_id_env, client_secret_env); the node reads its login from them.
export NODE_C_CLIENT_ID NODE_C_CLIENT_SECRET
exec env OBP_BANK_NODE_CONFIG=dev/node-c.yaml cargo run -p obp-bank-node

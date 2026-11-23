#!/usr/bin/env bash
# Copyright (C) 2025-2026 TESOBE GmbH
# SPDX-License-Identifier: AGPL-3.0-or-later
# Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.
# Start Bank Node B (rt.bank.b4, :8089, Cardano preprod; the creditor side of the demo).
# Bank B is on both local OBP hosts, so it is the hub between them: on OBP host A (localhost:8080)
# with Bank A, and on OBP host B (localhost:8081) with Bank C.
# The node is a Platform App on each OBP host, with its own Consumer there, 'obp-bank-node-b': it
# calls OBP-API as itself with OAuth2 client credentials from that OBP host's OBP-OIDC. Its login
# on host A comes from dev/local.env (NODE_B_CLIENT_ID / _SECRET, written by dev/create_consumers.sh),
# on host B from dev/local-b.env (NODE_B_HOST_B_CLIENT_ID / _SECRET, written by
# commands/_helpers/obp_b_add_bank_b.sh).
# Prereqs: OBP-API on :8080 with OBP-OIDC on :9000, RabbitMQ with the /bank.rt.bank.b4 and
# /host-b.bank.rt.bank.b4 vhosts, cardano-node + Ogmios on :1337, and dev/setup_obp.sh run once
# (Scopes, the node's view and its access). OBP host B may start later: the node keeps retrying it.
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

# shellcheck disable=SC1091
[ -f "$DIR/local.env" ] && source "$DIR/local.env"
# shellcheck disable=SC1091
[ -f "$DIR/local-b.env" ] && source "$DIR/local-b.env"
[ -n "${NODE_B_CLIENT_ID:-}" ] && [ -n "${NODE_B_CLIENT_SECRET:-}" ] \
  || { echo "FATAL: no NODE_B_CLIENT_ID / NODE_B_CLIENT_SECRET in dev/local.env: run dev/create_consumers.sh" >&2; exit 1; }
[ -n "${NODE_B_HOST_B_CLIENT_ID:-}" ] && [ -n "${NODE_B_HOST_B_CLIENT_SECRET:-}" ] \
  || { echo "FATAL: no NODE_B_HOST_B_CLIENT_ID / NODE_B_HOST_B_CLIENT_SECRET in dev/local-b.env: run commands/_helpers/obp_b_add_bank_b.sh (with OBP host B running)" >&2; exit 1; }
cd "$REPO"
# dev/node-b.yaml names these variables (client_id_env, client_secret_env); the node reads its two logins from them.
export NODE_B_CLIENT_ID NODE_B_CLIENT_SECRET NODE_B_HOST_B_CLIENT_ID NODE_B_HOST_B_CLIENT_SECRET
exec env OBP_BANK_NODE_CONFIG=dev/node-b.yaml cargo run -p obp-bank-node

#!/usr/bin/env bash
# Start Bank Node A (rt.bank.a3, :8088, Cardano preprod; the debtor side of the demo).
# The node is a Platform App: it calls OBP-API as itself with OAuth2 client credentials from
# OBP-OIDC, Consumer 'obp-bank-node-a' (dev/create_consumers.sh writes NODE_A_CLIENT_ID /
# NODE_A_CLIENT_SECRET to dev/local.env). Prereqs: OBP-API on :8080, OBP-OIDC on :9000,
# RabbitMQ with the /bank.rt.bank.a3 vhost, cardano-node + Ogmios on :1337, and
# dev/setup_obp.sh run once (Scopes, the node's view and its access).
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

# shellcheck disable=SC1091
[ -f "$DIR/local.env" ] && source "$DIR/local.env"
[ -n "${NODE_A_CLIENT_ID:-}" ] && [ -n "${NODE_A_CLIENT_SECRET:-}" ] \
  || { echo "FATAL: no NODE_A_CLIENT_ID / NODE_A_CLIENT_SECRET in dev/local.env: run dev/create_consumers.sh" >&2; exit 1; }
cd "$REPO"
exec env OBP_BANK_NODE_CONFIG=dev/node-a.yaml \
    OBP_BN_OBP_API__CLIENT_ID="$NODE_A_CLIENT_ID" \
    OBP_BN_OBP_API__CLIENT_SECRET="$NODE_A_CLIENT_SECRET" \
    cargo run -p obp-bank-node

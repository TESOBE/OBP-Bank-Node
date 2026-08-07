#!/usr/bin/env bash
# Start Bank Node B (rt.bank.b, :8089, mock rail — the creditor side).
# Prereqs: OBP-API on :8080, RabbitMQ with the /bank.rt.bank.b vhost
# (setup_rabbitmq.sh), cbs_stub.py on :9009, and a fresh env.sh
# (setup_obp.sh writes the DirectLogin tokens).
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/../.." && pwd)"

source "$DIR/env.sh"
cd "$REPO"
exec env OBP_BANK_NODE_CONFIG=WIP/roundtrip/node-b.yaml \
    OBP_BN_OBP_API__DIRECT_LOGIN_TOKEN="$RT_NODE_B_TOKEN" \
    cargo run -p obp-bank-node

#!/usr/bin/env bash
# Start Bank Node A (rt.bank.a2, :8088, Cardano preprod rail).
# Prereqs: OBP-API on :8080, RabbitMQ with the /bank.rt.bank.a2 vhost
# (setup_rabbitmq.sh), cardano-node + Ogmios on :1337 (docker/), and a fresh
# env.sh (setup_obp.sh writes the DirectLogin tokens).
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"

source "$DIR/env.sh"
cd "$REPO"
exec env OBP_BANK_NODE_CONFIG=dev/node-a.yaml \
    OBP_BN_OBP_API__DIRECT_LOGIN_TOKEN="$RT_NODE_A_TOKEN" \
    cargo run -p obp-bank-node

#!/usr/bin/env bash
# Copyright (C) 2025-2026 TESOBE GmbH
# SPDX-License-Identifier: AGPL-3.0-or-later
# Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.
# Bring up the localhost two-bank round trip: node A + node B, their two UIs, and the CBS stub,
# against an OBP-API, RabbitMQ and cardano-node + Ogmios that are already running.
#
#   dev/up.sh           check, set up OBP-API + RabbitMQ, start everything in the background
#   dev/up.sh check     check the prerequisites only
#   dev/up.sh setup     wait for OBP-API + RabbitMQ, then run setup_rabbitmq.sh + setup_obp.sh
#   dev/up.sh cbs       start the CBS stub (cbs_stub.py, :9009) if it isn't running
#   dev/up.sh status    show what is up
#   dev/up.sh stop      stop the nodes, UIs and CBS stub that dev/up.sh started
#
# Logs: dev/data/logs/<name>.log. The processes keep running after this script exits.
# Starting each piece in its own terminal works too: dev/start_node_a.sh, start_node_b.sh,
# start_ui_a.sh, start_ui_b.sh (after `dev/up.sh setup` and `dev/up.sh cbs`).
set -uo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"
LOGS="$DIR/data/logs"
RUN="$DIR/data/run"
mkdir -p "$LOGS" "$RUN"

# shellcheck disable=SC1091
[ -f "$DIR/local.env" ] && source "$DIR/local.env"
OBP="${RT_OBP_URL:-http://localhost:8080}"
OGMIOS_HEALTH="http://localhost:1337/health"
BANK_A="$(grep -oP '^\s*bank_id:\s*"\K[^"]+' "$DIR/node-a.yaml" | head -1)"
BANK_B="$(grep -oP '^\s*bank_id:\s*"\K[^"]+' "$DIR/node-b.yaml" | head -1)"

ok()   { echo "  ok    $*"; }
bad()  { echo "  MISSING $*"; FAILED=1; }
warn() { echo "  warn  $*"; }

up_http() { curl -sf -o /dev/null --max-time 3 "$1"; }
up_port() { (exec 3<>"/dev/tcp/localhost/$1") 2>/dev/null; }

check() {
  FAILED=0
  echo "== tools"
  for t in cargo python3 curl; do command -v "$t" >/dev/null && ok "$t" || bad "$t"; done
  command -v psql >/dev/null && ok "psql" || warn "psql not found (create_consumers.sh uses it to recover an existing Consumer's secret)"
  python3 -c 'import cryptography' 2>/dev/null && ok "python3 cryptography" || bad "python3 cryptography (pip install cryptography; wallets.py needs it)"
  command -v docker >/dev/null || command -v podman >/dev/null && ok "docker or podman" || bad "docker or podman (for cardano-node + Ogmios)"

  echo "== dev/local.env"
  if [ -f "$DIR/local.env" ]; then
    for v in RT_OBP_PROPS_FILE RT_SA_USER RT_SA_PASS RT_CONSUMER_KEY \
             NODE_A_CLIENT_ID NODE_A_CLIENT_SECRET NODE_A_CONSUMER_ID NODE_B_CLIENT_ID NODE_B_CLIENT_SECRET NODE_B_CONSUMER_ID \
             UI_A_OAUTH_CLIENT_ID UI_A_OAUTH_CLIENT_SECRET UI_B_OAUTH_CLIENT_ID UI_B_OAUTH_CLIENT_SECRET; do
      [ -n "${!v:-}" ] && ok "$v" || bad "$v (see dev/local.env.example; dev/create_consumers.sh writes the Consumer values)"
    done
    if [ -n "${RT_OBP_PROPS_FILE:-}" ] && [ -f "$RT_OBP_PROPS_FILE" ]; then
      grep -qE '^open_corridor_enabled=true' "$RT_OBP_PROPS_FILE" && ok "open_corridor_enabled=true in OBP-API props" \
        || bad "open_corridor_enabled=true in $RT_OBP_PROPS_FILE (then restart OBP-API)"
    fi
  else
    bad "dev/local.env (cp dev/local.env.example dev/local.env, then dev/create_consumers.sh)"
  fi

  echo "== wallets (python3 dev/wallets.py)"
  for f in cardano.skey cardano.addr creditor.skey creditor.addr; do
    [ -s "$REPO/secrets/$f" ] && ok "secrets/$f" || bad "secrets/$f"
  done

  echo "== services"
  up_http "$OBP/obp/v6.0.0/root" && ok "OBP-API $OBP" || bad "OBP-API $OBP"
  up_http "http://localhost:9000/obp-oidc/.well-known/openid-configuration" && ok "OBP-OIDC :9000" \
    || bad "OBP-OIDC :9000 (the nodes get their client-credentials tokens from it, the UIs' /setup login uses it)"
  up_port 5672 && ok "RabbitMQ :5672" || bad "RabbitMQ :5672"
  up_http "http://localhost:15672/" && ok "RabbitMQ management :15672" || bad "RabbitMQ management :15672 (rabbitmq-plugins enable rabbitmq_management)"
  if health="$(curl -sf --max-time 3 "$OGMIOS_HEALTH")"; then
    sync="$(python3 -c 'import json,sys;print(json.load(sys.stdin).get("networkSynchronization",0))' <<<"$health" 2>/dev/null)"
    python3 -c "import sys; sys.exit(0 if float('$sync') >= 0.9999 else 1)" 2>/dev/null \
      && ok "Ogmios :1337 (synced)" || warn "Ogmios :1337 answering but still syncing ($sync); see docker/cardano-bootstrap.sh status"
  else
    bad "Ogmios :1337 (docker/cardano-bootstrap.sh up, then mithril; see dev/README.md)"
  fi
  return "$FAILED"
}

setup() {
  echo "== waiting for OBP-API ($OBP) and RabbitMQ (:5672, :15672)"
  until up_http "$OBP/obp/v6.0.0/root" && up_port 5672 && up_http "http://localhost:15672/"; do
    echo "   still waiting..."; sleep 5
  done
  echo "== RabbitMQ vhosts (setup_rabbitmq.sh)"
  bash "$DIR/setup_rabbitmq.sh" || return 1
  echo "== OBP-API banks, users, accounts, roles, tokens (setup_obp.sh)"
  bash "$DIR/setup_obp.sh" || return 1
}

cbs() {
  if up_port 9009; then echo "CBS stub already on :9009"; return 0; fi
  (cd "$DIR" && nohup python3 cbs_stub.py >"$LOGS/cbs_stub.log" 2>&1 & echo $! >"$RUN/cbs_stub.pid")
  sleep 1
  up_port 9009 && echo "CBS stub started on :9009 (log dev/data/logs/cbs_stub.log)" \
    || { echo "CBS stub did not start, see dev/data/logs/cbs_stub.log"; return 1; }
}

start_bg() { # start_bg <name> <script>
  local name="$1" script="$2" pidf="$RUN/$1.pid"
  if [ -f "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; then echo "  $name already running"; return 0; fi
  (cd "$REPO" && setsid nohup "$DIR/$script" >"$LOGS/$name.log" 2>&1 & echo $! >"$pidf")
  echo "  $name started (log dev/data/logs/$name.log)"
}

wait_health() { # wait_health <name> <url>
  for _ in $(seq 1 60); do up_http "$2" && { echo "  $1 up: $2"; return 0; }; sleep 2; done
  echo "  $1 not answering at $2 yet, see dev/data/logs/$1.log"
}

status() {
  for s in "node-a|http://localhost:8088/health" "node-b|http://localhost:8089/health" \
           "ui-a|http://localhost:8091/" "ui-b|http://localhost:8092/"; do
    up_http "${s#*|}" && echo "  up    ${s%%|*}  ${s#*|}" || echo "  down  ${s%%|*}  ${s#*|}"
  done
  up_port 9009 && echo "  up    cbs-stub  :9009" || echo "  down  cbs-stub  :9009"
}

stop() {
  for name in ui-b ui-a node-b node-a cbs_stub; do
    pidf="$RUN/$name.pid"
    [ -f "$pidf" ] || continue
    pid="$(cat "$pidf")"
    if kill -0 "$pid" 2>/dev/null; then
      kill -- "-$pid" 2>/dev/null || kill "$pid" 2>/dev/null
      echo "  stopped $name"
    fi
    rm -f "$pidf"
  done
}

case "${1:-all}" in
  check)  check ;;
  setup)  setup ;;
  cbs)    cbs ;;
  status) status ;;
  stop)   stop ;;
  all)
    echo "Banks: A=$BANK_A (node :8088, UI :8091)  B=$BANK_B (node :8089, UI :8092)"
    check || { echo; echo "Fix the MISSING items above first (dev/README.md)."; exit 1; }
    setup || exit 1
    cbs || exit 1
    echo "== building (once, so the four processes don't queue on cargo's lock)"
    (cd "$REPO" && cargo build -p obp-bank-node -p obp-bank-node-ui) || exit 1
    echo "== starting"
    start_bg node-a start_node_a.sh
    start_bg node-b start_node_b.sh
    start_bg ui-a start_ui_a.sh
    start_bg ui-b start_ui_b.sh
    wait_health node-a http://localhost:8088/health
    wait_health node-b http://localhost:8089/health
    wait_health ui-a http://localhost:8091/
    wait_health ui-b http://localhost:8092/
    cat <<EOF

Next:
  1. Open http://localhost:8091/setup and http://localhost:8092/setup, log in, and Apply the
     missing items until all are green (corridor accounts per currency, CARDANO settlement routing).
  2. Send a payment in UI A (http://localhost:8091), or run dev/run_roundtrip.sh.
  Stop with: dev/up.sh stop
EOF
    ;;
  *) sed -n '2,15p' "$0"; exit 2 ;;
esac

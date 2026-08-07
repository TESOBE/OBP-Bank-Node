#!/usr/bin/env bash
# Round-trip driver: fires one payment at Node A and watches every stage.
# Prereqs: OBP-API + RabbitMQ + cardano/ogmios up; setup scripts run;
#          node A (:8088), node B (:8089), cbs_stub.py (:9009) running.
set -uo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
source "$DIR/env.sh"

OBP="http://localhost:8080"
NODE_A="http://localhost:8088"
AMOUNT="${1:-500.00}"

sq() { # sq <db-file> <sql>  (sqlite3 CLI not installed; use python's module)
  python3 -c '
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
for row in con.execute(sys.argv[2]):
    print("|".join("" if c is None else str(c) for c in row))
' "$1" "$2" 2>/dev/null
}

echo "=== 1. POST payment to Node A (KES $AMOUNT, rt.bank.a -> rt.bank.b)"
RESP=$(curl -s -X POST "$NODE_A/obp-bank-node/v5.1.0/transaction-requests" \
  -H 'Content-Type: application/json' -d @- <<EOF
{
  "value": {"currency": "KES", "amount": "$AMOUNT"},
  "description": "Round-trip test payment",
  "to": {
    "name": "Beneficiary at rt.bank.b",
    "description": "settlement-b at rt.bank.b",
    "other_bank_routing_scheme": "OBP",
    "other_bank_routing_address": "rt.bank.b",
    "other_account_routing_scheme": "OBP",
    "other_account_routing_address": "settlement-b",
    "other_account_secondary_routing_scheme": "",
    "other_account_secondary_routing_address": "",
    "other_branch_routing_scheme": "",
    "other_branch_routing_address": ""
  },
  "originator": {
    "name": "Alice Sender",
    "address": "1 Sender Street, Nairobi",
    "account_routing": {"scheme": "IBAN", "address": "KE93 0000 1234 5678 9012 34"}
  },
  "charge_policy": "SHARED"
}
EOF
)
echo "$RESP" | python3 -m json.tool 2>/dev/null || echo "$RESP"
TR_ID=$(echo "$RESP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["transaction_request_id"])') || exit 1
echo "node TR id: $TR_ID"

echo "=== 2. Waiting for outbox: INITIATED -> SUBMITTED -> PROMISE_WRITTEN -> REPORTED"
for i in $(seq 1 60); do
  ST=$(curl -s "$NODE_A/obp-bank-node/v5.1.0/transaction-requests/$TR_ID" \
    | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("status","?"), d.get("promise_id") or "")')
  echo "  [$i] $ST"
  case "$ST" in REPORTED*) break;; EXCEPTION*) echo "FAILED — outbox row hit EXCEPTION"; exit 1;; esac
  sleep 5
done
[[ "$ST" == REPORTED* ]] || { echo "TIMEOUT waiting for REPORTED"; exit 1; }
PROMISE_TX=$(sq "$DIR/data/node-a/outbox.db" \
  "SELECT promise_tx_id FROM outbox WHERE transaction_request_id='$TR_ID'")
echo "Promise on-chain tx: $PROMISE_TX"
echo "  https://preprod.cardanoscan.io/transaction/$PROMISE_TX"

echo "=== 3. Settle the pair (admin, from bank A's side)"
SETTLE=$(curl -s -X POST "$OBP/obp/v7.0.0/banks/rt.bank.a/open-corridor/settlements" \
  -H "Authorization: DirectLogin token=\"$RT_ADMIN_TOKEN\"" \
  -H 'Content-Type: application/json' \
  -d '{"other_bank_id":"rt.bank.b","currency":"KES"}')
echo "$SETTLE" | python3 -m json.tool 2>/dev/null || echo "$SETTLE"
SETTLEMENT_ID=$(echo "$SETTLE" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("settlement_id",""))')
[ -n "$SETTLEMENT_ID" ] || { echo "FAILED — settle returned no settlement_id"; exit 1; }

echo "=== 4. Waiting for Node A settlement: SETTLING -> SUBMITTED -> FINAL"
for i in $(seq 1 60); do
  ROW=$(sq "$DIR/data/node-a/settlements.db" \
    "SELECT status||' depth='||last_depth||' tx='||COALESCE(tx_id,'') FROM settlements WHERE idempotency_key='$SETTLEMENT_ID'")
  echo "  [$i] ${ROW:-<no row yet>}"
  case "$ROW" in FINAL*) break;; ERROR*) echo "FAILED — settlement ERROR"; exit 1;; esac
  sleep 10
done
case "${ROW:-}" in FINAL*) ;; *) echo "TIMEOUT waiting for FINAL"; exit 1;; esac

echo "=== 5. Beneficiary side (Node B)"
echo "-- evidence store:"
sq "$DIR/data/node-b/evidence.db" \
  "SELECT transaction_request_id, verified, substr(promise_commitment,1,16)||'…' FROM evidence"
echo "-- CBS credits received:"
tail -n 3 "$DIR/data/cbs_received.jsonl" 2>/dev/null || echo "  (none!)"

echo "=== ROUND TRIP COMPLETE ==="
echo "promise tx:    $PROMISE_TX"
echo "settlement id: $SETTLEMENT_ID"

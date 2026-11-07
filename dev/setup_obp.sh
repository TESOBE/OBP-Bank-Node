#!/usr/bin/env bash
# Copyright (C) 2025-2026 TESOBE GmbH
# SPDX-License-Identifier: AGPL-3.0-or-later
# Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.
# Round-trip test: OBP-API data setup (idempotent — safe to re-run).
#
# The nodes run as Platform Apps: each calls OBP-API as itself, with its own Consumer and OAuth2
# client credentials (no service user). This script, as the OBP-API super admin, creates:
#   - banks rt.bank.a4 / rt.bank.b4 and the routing schemes
#   - each bank's corridor accounts (one per currency in node-*.yaml) and settlement accounts
#   - on each corridor account, the custom view the node creates Transaction Requests through
#     (bank.view_id in node-*.yaml), with only can_add_transaction_request_to_any_account,
#     and access to it for the node's Consumer (Consumer Account Access)
#   - each node's Consumer marked as a Platform App, with its Scopes at its own bank:
#     CanAttachOpenCorridorPromise and CanSettleOpenCorridor
#   - FX rates and the per-bank AMQP broker registrations
# and writes the admin's DirectLogin token to env.sh (run_roundtrip.sh settles with it).
set -uo pipefail

# Per-machine settings come from dev/local.env (gitignored; see dev/local.env.example):
# RT_CONSUMER_KEY, NODE_A_CONSUMER_ID, NODE_B_CONSUMER_ID (written by dev/create_consumers.sh),
# RT_SA_USER, RT_SA_PASS, and optionally RT_OBP_URL.
SETUP_DIR="$(cd "$(dirname "$0")" && pwd)"
[ -f "$SETUP_DIR/local.env" ] && source "$SETUP_DIR/local.env"

OBP="${RT_OBP_URL:-http://localhost:8080}"
CK="${RT_CONSUMER_KEY:-}"   # consumer 'obp-bank-node-roundtrip', for the admin's DirectLogin
SA_USER="${RT_SA_USER:-}"
SA_PASS="${RT_SA_PASS:-}"
missing=""
[ -n "$CK" ] || missing+=" RT_CONSUMER_KEY"
[ -n "${NODE_A_CONSUMER_ID:-}" ] && [ -n "${NODE_B_CONSUMER_ID:-}" ] || missing+=" NODE_A_CONSUMER_ID/NODE_B_CONSUMER_ID"
[ -n "$SA_USER" ] && [ -n "$SA_PASS" ] || missing+=" RT_SA_USER/RT_SA_PASS"
[ -z "$missing" ] || { echo "FATAL: set in dev/local.env (dev/create_consumers.sh writes the Consumer values):$missing" >&2; exit 1; }

BANK_A="rt.bank.a4"; BANK_B="rt.bank.b4"
# Cardano addresses (preprod) of the nodes' wallets, written by dev/wallets.py next to the keys.
REPO_DIR="$(cd "$SETUP_DIR/.." && pwd)"
read_addr() { # read_addr <file> -> address
  [ -s "$REPO_DIR/secrets/$1" ] || { echo "FATAL: secrets/$1 not found — run python3 dev/wallets.py" >&2; exit 1; }
  tr -d '[:space:]' < "$REPO_DIR/secrets/$1"
}
ADDR_A=$(read_addr cardano.addr) || exit 1
ADDR_B=$(read_addr creditor.addr) || exit 1
# The UI setup pages apply the same address as the CARDANO routing on OBP-INCOMING-SETTLEMENT-ACCOUNT.
for pair in "ui-a.yaml:$ADDR_A" "ui-b.yaml:$ADDR_B"; do
  grep -q "address: \"${pair#*:}\"" "$SETUP_DIR/${pair%%:*}" \
    || echo "WARN: dev/${pair%%:*} has a different CARDANO settlement address than secrets/ (${pair#*:})"
done
echo "settlement addresses: A=$ADDR_A B=$ADDR_B"

# From a node config: its corridor accounts ("CURRENCY ACCOUNT_ID" lines) and its view.
corridor_accounts() {
  awk '/^  corridor_accounts:/{f=1;next} f&&/^    [A-Z]{3}:/{gsub(/[":]/,""); print $1, $2; next} f&&!/^    /{f=0}' "$SETUP_DIR/$1"
}
node_view() { grep -oP '^\s*view_id:\s*"\K[^"]+' "$SETUP_DIR/$1" | head -1; }

dl() { # dl <user> <pass> -> token
  curl -s -X POST "$OBP/my/logins/direct" \
    -H "Authorization: DirectLogin username=\"$1\",password=\"$2\",consumer_key=\"$CK\"" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("token",""))'
}

TOK=$(dl "$SA_USER" "$SA_PASS")
[ -n "$TOK" ] || { echo "FATAL: super admin login failed"; exit 1; }
AUTH="Authorization: DirectLogin token=\"$TOK\""

api() { # api METHOD PATH [JSON]
  local m="$1" p="$2" d="${3:-}"
  if [ -n "$d" ]; then
    curl -s -X "$m" "$OBP$p" -H "$AUTH" -H 'Content-Type: application/json' -d "$d"
  else
    curl -s -X "$m" "$OBP$p" -H "$AUTH"
  fi
}

SA_ID=$(api GET /obp/v5.1.0/users/current | python3 -c 'import json,sys;print(json.load(sys.stdin)["user_id"])')
echo "super admin user_id: $SA_ID"

grant() { # grant <user_id> <bank_id-or-empty> <role>
  api POST "/obp/v5.1.0/users/$1/entitlements" \
    "{\"bank_id\":\"$2\",\"role_name\":\"$3\"}" \
    | grep -qE 'entitlement_id|OBP-30216' && echo "  role $3 @ '$2' ok" \
    || echo "  role $3 @ '$2' — check manually"
}

echo "== granting admin roles"
grant "$SA_ID" ""        CanCreateBank
grant "$SA_ID" ""        CanCreatePlatformApp
grant "$SA_ID" ""        CanCreateCustomView

echo "== creating banks"
for B in "$BANK_A" "$BANK_B"; do
  api POST /obp/v5.0.0/banks \
    "{\"id\":\"$B\",\"bank_code\":\"$B\",\"full_name\":\"Round-trip $B\",\"logo\":\"\",\"website\":\"\",\"bank_routings\":[]}" \
    | grep -qE '"id"|OBP-34000|already exists' && echo "  bank $B ok" || echo "  bank $B — check"
  grant "$SA_ID" "$B" CanCreateAccount
  grant "$SA_ID" "$B" CanConfigureAmqpBankBroker
  # Bank-scoped since 2026-08-07: the settle POST is addressed to a bank's URL
  # and the role must be held at that bank.
  grant "$SA_ID" "$B" CanSettleOpenCorridor
done

echo "== registering routing schemes (node validates beneficiary routing against these)"
grant "$SA_ID" "" CanCreateRoutingScheme
register_scheme() { # register_scheme <json-body> <name>
  api POST /obp/v7.0.0/routing-schemes "$1" \
    | grep -qE '"scheme"|OBP-30515' && echo "  scheme $2 ok" \
    || echo "  scheme $2 — check manually"
}
# The three allow-listed global (unprefixed) schemes; country must be INT.
register_scheme '{"scheme":"OBP","country":"INT","category":"ACCOUNT",
  "address_pattern":"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$","example_address":"rt.bank.b4",
  "description":"OBP bank id or account id, as used in OBP account routings.",
  "downstream_rails":[],"status":"ACTIVE"}' OBP
register_scheme '{"scheme":"IBAN","country":"INT","category":"ACCOUNT",
  "address_pattern":"^[A-Z]{2}[0-9]{2}[A-Z0-9]{1,30}$","example_address":"GB29NWBK60161331926819",
  "description":"International Bank Account Number (ISO 13616), no spaces.",
  "downstream_rails":[],"status":"ACTIVE"}' IBAN
register_scheme '{"scheme":"BIC","country":"INT","category":"BANK",
  "address_pattern":"^[A-Z]{6}[A-Z0-9]{2}([A-Z0-9]{3})?$","example_address":"NWBKGB2LXXX",
  "description":"ISO 9362 Business Identifier Code, 8 or 11 characters.",
  "downstream_rails":[],"status":"ACTIVE"}' BIC

acct() { # acct BANK ACCOUNT CURRENCY LABEL  (owned by the super admin)
  api PUT "/obp/v5.1.0/banks/$1/accounts/$2" \
    "{\"user_id\":\"$SA_ID\",\"label\":\"$4\",\"product_code\":\"OPEN_CORRIDOR\",\"balance\":{\"currency\":\"$3\",\"amount\":\"0\"},\"branch_id\":\"\",\"account_routings\":[]}" \
    | grep -qE 'account_id|OBP-30208|already exists' && echo "  $1/$2 ($3) ok" || echo "  $1/$2 ($3) — check"
}

# The view a node creates Transaction Requests through, on one corridor account: a custom view
# with only that permission (created with the management endpoint, role CanCreateCustomView),
# granted to the node's Consumer through the ManageCustomViews system view, which the admin first
# gives themself through owner. The owner view itself can't be granted to a Consumer.
node_view_access() { # node_view_access BANK ACCOUNT VIEW_ID CONSUMER_ID
  local name="$3"
  api POST "/obp/v5.1.0/banks/$1/accounts/$2/views/owner/account-access/grant" \
    "{\"user_id\":\"$SA_ID\",\"view_id\":\"ManageCustomViews\"}" \
    | grep -q '"id"' || echo "  ManageCustomViews on $1/$2 for the admin — check"
  api POST "/obp/v6.0.0/management/banks/$1/accounts/$2/views" \
    "{\"name\":\"$name\",\"description\":\"The Bank Node creates OPEN_CORRIDOR_PROMISE Transaction Requests through this view\",\"metadata_view\":\"$name\",\"is_public\":false,\"which_alias_to_use\":\"\",\"hide_metadata_if_alias_used\":false,\"allowed_actions\":[\"can_add_transaction_request_to_any_account\"]}" \
    | grep -qE '"id"|"view_id"|OBP-30266' && echo "  view $1/$2/$3 ok" || echo "  view $1/$2/$3 — check"
  api POST "/obp/v7.0.0/banks/$1/accounts/$2/views/ManageCustomViews/consumer-account-access" \
    "{\"consumer_id\":\"$4\",\"view_id\":\"$3\"}" \
    | grep -qE 'consumer_account_access_id|OBP-30627' && echo "  Consumer access to $1/$2/$3 ok" \
    || echo "  Consumer access to $1/$2/$3 — check"
}

platform_app() { # platform_app CONSUMER_ID BANK LABEL
  api POST /obp/v7.0.0/management/platform-apps "{\"consumer_id\":\"$1\",\"label\":\"$3\"}" \
    | grep -qE '"consumer_id"|OBP-35045' && echo "  Platform App $3 ok" || echo "  Platform App $3 — check"
  for role in CanAttachOpenCorridorPromise CanSettleOpenCorridor; do
    api POST "/obp/v7.0.0/consumers/$1/scopes" "{\"bank_id\":\"$2\",\"role_name\":\"$role\"}" \
      | grep -qE 'scope_id|OBP-30216|already' && echo "  Scope $role @ $2 ok" || echo "  Scope $role @ $2 — check"
  done
}

for side in "a|$BANK_A|$NODE_A_CONSUMER_ID" "b|$BANK_B|$NODE_B_CONSUMER_ID"; do
  IFS='|' read -r S B CONSUMER <<<"$side"
  VIEW="$(node_view "node-$S.yaml")"
  [ -n "$VIEW" ] || { echo "FATAL: no bank.view_id in dev/node-$S.yaml"; exit 1; }
  echo "== bank $B: corridor accounts, the node's view and its Consumer's access"
  while read -r CUR ACC; do
    acct "$B" "$ACC" "$CUR" "Node ${S^^} corridor account ($CUR)"
    node_view_access "$B" "$ACC" "$VIEW" "$CONSUMER"
  done < <(corridor_accounts "node-$S.yaml")
  # The internal net-posting accounts settlePair expects (per-bank, fixed ids):
  acct "$B" "OBP-OUTGOING-SETTLEMENT-ACCOUNT" KES "OC outgoing settlement"
  acct "$B" "OBP-INCOMING-SETTLEMENT-ACCOUNT" KES "OC incoming settlement"
  echo "== bank $B: node ${S^^} as a Platform App, with its Scopes"
  platform_app "$CONSUMER" "$B" "OBP Bank Node ${S^^} ($B)"
done

echo "== creating EUR<->KES FX rates (challenge threshold conversion needs them)"
for B in "$BANK_A" "$BANK_B"; do
  grant "$SA_ID" "$B" CanCreateFxRate
  api PUT "/obp/v2.2.0/banks/$B/fx" \
    "{\"bank_id\":\"$B\",\"from_currency_code\":\"EUR\",\"to_currency_code\":\"KES\",\"conversion_value\":150.0,\"inverse_conversion_value\":0.006667,\"effective_date\":\"2026-07-31T00:00:00Z\"}" \
    | grep -q from_currency_code && echo "  fx EUR->KES @ $B ok" || echo "  fx EUR->KES @ $B — check"
  api PUT "/obp/v2.2.0/banks/$B/fx" \
    "{\"bank_id\":\"$B\",\"from_currency_code\":\"KES\",\"to_currency_code\":\"EUR\",\"conversion_value\":0.006667,\"inverse_conversion_value\":150.0,\"effective_date\":\"2026-07-31T00:00:00Z\"}" \
    | grep -q from_currency_code && echo "  fx KES->EUR @ $B ok" || echo "  fx KES->EUR @ $B — check"
done

echo "== registering AMQP brokers"
api PUT "/obp/v7.0.0/banks/$BANK_A/amqp-broker" \
  "{\"host\":\"localhost\",\"port\":5672,\"virtual_host\":\"/bank.$BANK_A\",\"username\":\"bank_a_node\",\"password\":\"rtpass-a\",\"use_ssl\":false,\"settlement_address\":\"$ADDR_A\"}" \
  | grep -q '"bank_id"' && echo "  broker $BANK_A ok" || echo "  broker $BANK_A — check"
api PUT "/obp/v7.0.0/banks/$BANK_B/amqp-broker" \
  "{\"host\":\"localhost\",\"port\":5672,\"virtual_host\":\"/bank.$BANK_B\",\"username\":\"bank_b_node\",\"password\":\"rtpass-b\",\"use_ssl\":false,\"settlement_address\":\"$ADDR_B\"}" \
  | grep -q '"bank_id"' && echo "  broker $BANK_B ok" || echo "  broker $BANK_B — check"

echo "== writing the admin token to env.sh"
cat > "$SETUP_DIR/env.sh" <<EOF
# generated by setup_obp.sh $(date -Is)
export RT_ADMIN_TOKEN="$TOK"
EOF
echo "setup complete."

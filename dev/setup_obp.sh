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
#
# Another OBP host (for example a second local OBP-API, on :8081) is set up with the same
# script and these environment variables, all optional. Unset, the script sets up the
# round-trip pair above on dev/local.env's OBP host, as it always has.
#   RT_LOCAL_ENV  the env file to read, instead of dev/local.env
#   RT_NODES      the banks that have a node, as space-separated rows of
#                 side|bank_id|BIC|address file in secrets/|consumer_id variable|broker user|broker password[|vhost]
#                 The node's config is dev/node-<side>.yaml. The vhost defaults to /bank.<bank_id>; a bank on
#                 several local OBP hosts needs a different one on each, because they share one RabbitMQ.
#   RT_HUB_BANKS  banks with no node yet, as space-separated rows of bank_id|BIC. Each gets its BIC
#                 and FX rates, but no participant mark: with no node, a payment to it could not be
#                 delivered. A hub bank gets its node once a node can be on several OBP hosts.
#   RT_ENV_SH     where to write the admin's token, instead of dev/env.sh
set -uo pipefail

# Per-machine settings come from dev/local.env (gitignored; see dev/local.env.example):
# RT_CONSUMER_KEY, NODE_A_CONSUMER_ID, NODE_B_CONSUMER_ID (written by dev/create_consumers.sh),
# RT_SA_USER, RT_SA_PASS, and optionally RT_OBP_URL.
SETUP_DIR="$(cd "$(dirname "$0")" && pwd)"
LOCAL_ENV="${RT_LOCAL_ENV:-$SETUP_DIR/local.env}"
[ -f "$LOCAL_ENV" ] && source "$LOCAL_ENV"
LOCAL_ENV_NAME="dev/$(basename "$LOCAL_ENV")"
ENV_SH="${RT_ENV_SH:-$SETUP_DIR/env.sh}"

# The banks with a node: side|bank_id|BIC|address file|consumer_id variable|broker user|broker password.
# Each demo bank has a made-up BIC, so a payment can name the beneficiary bank by BIC instead of by
# OBP bank id. ZZ is not assigned to any country, so these can never be a real bank's BIC.
DEFAULT_NODES=(
  "a|rt.bank.a4|DEMAZZ22|cardano.addr|NODE_A_CONSUMER_ID|bank_a_node|rtpass-a"
  "b|rt.bank.b4|DEMBZZ22|creditor.addr|NODE_B_CONSUMER_ID|bank_b_node|rtpass-b"
)
if [ -n "${RT_NODES:-}" ]; then read -r -a NODES <<<"$RT_NODES"; else NODES=("${DEFAULT_NODES[@]}"); fi
# The banks with no node yet: bank_id|BIC.
if [ -n "${RT_HUB_BANKS:-}" ]; then read -r -a HUB_BANKS <<<"$RT_HUB_BANKS"; else HUB_BANKS=(); fi

OBP="${RT_OBP_URL:-http://localhost:8080}"
CK="${RT_CONSUMER_KEY:-}"   # consumer 'obp-bank-node-roundtrip', for the admin's DirectLogin
SA_USER="${RT_SA_USER:-}"
SA_PASS="${RT_SA_PASS:-}"
missing=""
[ -n "$CK" ] || missing+=" RT_CONSUMER_KEY"
for row in "${NODES[@]}"; do
  IFS='|' read -r _ _ _ _ CONSUMER_VAR _ _ <<<"$row"
  [ -n "${!CONSUMER_VAR:-}" ] || missing+=" $CONSUMER_VAR"
done
[ -n "$SA_USER" ] && [ -n "$SA_PASS" ] || missing+=" RT_SA_USER/RT_SA_PASS"
[ -z "$missing" ] || { echo "FATAL: set in $LOCAL_ENV_NAME (dev/create_consumers.sh writes the Consumer values):$missing" >&2; exit 1; }

# Cardano addresses (preprod) of the nodes' wallets, written by dev/wallets.py next to the keys.
REPO_DIR="$(cd "$SETUP_DIR/.." && pwd)"
read_addr() { # read_addr <file> -> address
  [ -s "$REPO_DIR/secrets/$1" ] || { echo "FATAL: secrets/$1 not found — run python3 dev/wallets.py" >&2; exit 1; }
  tr -d '[:space:]' < "$REPO_DIR/secrets/$1"
}
# Each node's settlement address, by side.
declare -A ADDR
for row in "${NODES[@]}"; do
  IFS='|' read -r S _ _ ADDR_FILE _ _ _ <<<"$row"
  ADDR[$S]=$(read_addr "$ADDR_FILE") || exit 1
  # The UI setup pages apply the same address as the CARDANO routing on OBP-INCOMING-SETTLEMENT-ACCOUNT.
  if [ -f "$SETUP_DIR/ui-$S.yaml" ]; then
    grep -q "address: \"${ADDR[$S]}\"" "$SETUP_DIR/ui-$S.yaml" \
      || echo "WARN: dev/ui-$S.yaml has a different CARDANO settlement address than secrets/ (${ADDR[$S]})"
  fi
  echo "settlement address ${S^^}: ${ADDR[$S]}"
done

# From a node config: its corridor accounts ("CURRENCY ACCOUNT_ID" lines) and its view.
# The node configs list their banks under hosts:, so the block may sit at any indentation: the currency lines are the ones indented deeper than corridor_accounts:.
corridor_accounts() {
  awk 'match($0, /^ *corridor_accounts:/) {f=1; depth=index($0, "c"); next}
       f && /^ *[A-Z][A-Z][A-Z]:/ && match($0, /[^ ]/) > depth {gsub(/[":]/,""); print $1, $2; next}
       f {f=0}' "$SETUP_DIR/$1" | awk '!seen[$0]++'   # once each: a hub node lists the same accounts on each OBP host
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
# bank_id|BIC|has a node, for the banks with a node and then the hub banks.
BANK_ROWS=()
for row in "${NODES[@]}"; do IFS='|' read -r _ B BIC _ _ _ _ <<<"$row"; BANK_ROWS+=("$B|$BIC|node"); done
for row in "${HUB_BANKS[@]}"; do IFS='|' read -r B BIC <<<"$row"; BANK_ROWS+=("$B|$BIC|hub"); done
for bank_row in "${BANK_ROWS[@]}"; do
  IFS='|' read -r B BIC KIND <<<"$bank_row"
  BANK_JSON="{\"id\":\"$B\",\"bank_code\":\"$B\",\"full_name\":\"Round-trip $B\",\"logo\":\"\",\"website\":\"\",\"bank_routings\":[{\"scheme\":\"BIC\",\"address\":\"$BIC\"}]}"
  api POST /obp/v5.1.0/banks "$BANK_JSON" \
    | grep -qE '"id"|OBP-34000|already exists' && echo "  bank $B ok" || echo "  bank $B — check"
  # A bank created before the BICs were added gets its BIC here. Update Bank is defined in v5.0.0.
  api PUT /obp/v5.1.0/banks "$BANK_JSON" \
    | grep -q "\"$BIC\"" && echo "  bank $B BIC $BIC ok" || echo "  bank $B BIC — check"
  # Mark the bank as an Open Corridor participant. getBanks lists every bank on the OBP host, and the
  # Bank Node UI's bank pickers show only those carrying this attribute. Create Bank Attribute adds a
  # new attribute on every call, so it is only called when the bank does not carry it yet.
  # A hub bank with no node yet is not marked: a payment to it could not be delivered.
  grant "$SA_ID" "$B" CanCreateBankAttribute
  if [ "$KIND" = "hub" ]; then
    echo "  bank $B is a hub bank with no node yet: not marked as a participant"
  elif api GET "/obp/v5.1.0/banks/$B" | grep -q '"OPEN_CORRIDOR_PARTICIPANT"'; then
    echo "  bank $B participant ok"
  else
    api POST "/obp/v5.1.0/banks/$B/attribute" \
      '{"name":"OPEN_CORRIDOR_PARTICIPANT","type":"BOOLEAN","value":"true","is_active":true}' \
      | grep -q bank_attribute_id && echo "  bank $B participant set" || echo "  bank $B participant — check"
  fi
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

platform_app() { # platform_app CONSUMER_ID BANK LABEL [hub]
  api POST /obp/v7.0.0/management/platform-apps "{\"consumer_id\":\"$1\",\"label\":\"$3\"}" \
    | grep -qE '"consumer_id"|OBP-35045' && echo "  Platform App $3 ok" || echo "  Platform App $3 — check"
  local roles="CanAttachOpenCorridorPromise CanSettleOpenCorridor"
  # A node on two or more OBP hosts is a hub: it also states its onward routes.
  [ "${4:-}" = hub ] && roles="$roles CanUpdateOnwardRoutes"
  for role in $roles; do
    api POST "/obp/v7.0.0/consumers/$1/scopes" "{\"bank_id\":\"$2\",\"role_name\":\"$role\"}" \
      | grep -qE 'scope_id|OBP-30216|already' && echo "  Scope $role @ $2 ok" || echo "  Scope $role @ $2 — check"
  done
}

for row in "${NODES[@]}"; do
  IFS='|' read -r S B _ _ CONSUMER_VAR _ _ <<<"$row"
  CONSUMER="${!CONSUMER_VAR}"
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
  # A node whose config lists two or more OBP hosts is a hub.
  HUB=""; [ "$(grep -cE '^ *(- )?obp_api:' "$SETUP_DIR/node-$S.yaml")" -gt 1 ] && HUB=hub
  platform_app "$CONSUMER" "$B" "OBP Bank Node ${S^^} ($B)" $HUB
done

echo "== creating EUR<->KES FX rates (challenge threshold conversion needs them)"
for bank_row in "${BANK_ROWS[@]}"; do
  B="${bank_row%%|*}"
  grant "$SA_ID" "$B" CanCreateFxRate
  api PUT "/obp/v5.1.0/banks/$B/fx" \
    "{\"bank_id\":\"$B\",\"from_currency_code\":\"EUR\",\"to_currency_code\":\"KES\",\"conversion_value\":150.0,\"inverse_conversion_value\":0.006667,\"effective_date\":\"2026-07-31T00:00:00Z\"}" \
    | grep -q from_currency_code && echo "  fx EUR->KES @ $B ok" || echo "  fx EUR->KES @ $B — check"
  api PUT "/obp/v5.1.0/banks/$B/fx" \
    "{\"bank_id\":\"$B\",\"from_currency_code\":\"KES\",\"to_currency_code\":\"EUR\",\"conversion_value\":0.006667,\"inverse_conversion_value\":150.0,\"effective_date\":\"2026-07-31T00:00:00Z\"}" \
    | grep -q from_currency_code && echo "  fx KES->EUR @ $B ok" || echo "  fx KES->EUR @ $B — check"
done

echo "== registering AMQP brokers"
for row in "${NODES[@]}"; do
  IFS='|' read -r S B _ _ _ BROKER_USER BROKER_PASS VHOST <<<"$row"
  # The vhost from the row, or /bank.<bank_id>.
  VHOST="${VHOST:-/bank.$B}"
  api PUT "/obp/v7.0.0/banks/$B/amqp-broker" \
    "{\"host\":\"localhost\",\"port\":5672,\"virtual_host\":\"$VHOST\",\"username\":\"$BROKER_USER\",\"password\":\"$BROKER_PASS\",\"use_ssl\":false,\"settlement_address\":\"${ADDR[$S]}\"}" \
    | grep -q '"bank_id"' && echo "  broker $B ok" || echo "  broker $B — check"
done

echo "== writing the admin token to dev/$(basename "$ENV_SH")"
cat > "$ENV_SH" <<EOF
# generated by setup_obp.sh $(date -Is)
export RT_ADMIN_TOKEN="$TOK"
EOF
echo "setup complete."

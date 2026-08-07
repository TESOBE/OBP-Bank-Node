#!/usr/bin/env bash
# Round-trip test: OBP-API data setup (idempotent — safe to re-run).
# Creates: banks rt.bank.a / rt.bank.b, service users, settlement + customer
# accounts, roles, and the per-bank Open Corridor broker registrations.
# Writes DirectLogin tokens to env.sh for the node processes.
set -uo pipefail

OBP="http://localhost:8080"
CK="rtkey6a33e2a63adba1658290ecac776ff155"   # consumer 'obp-bank-node-roundtrip'
SA_USER="TheSuperUserAForTesting"
SA_PASS="alk24ahf8au98uropifa8aba"

BANK_A="rt.bank.a"; BANK_B="rt.bank.b"
NODE_A_USER="rt.node.a"; NODE_B_USER="rt.node.b"; NODE_PASS="RtNodePass2026!"
# Cardano addresses (preprod): A = bank wallet (funded), B = receive-only.
ADDR_A="addr_test1vz233470pc30zzsceh9k2qmzkj4gvj7n976ulvdmed5w9mg0a37ns"
ADDR_B="addr_test1vqn7sn79x6k9a2353l458mk2gccmwqk7nza93zydpuvl7lquy6jcl"

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
grant "$SA_ID" ""        CanCreateUser

echo "== creating banks"
for B in "$BANK_A" "$BANK_B"; do
  api POST /obp/v5.0.0/banks \
    "{\"id\":\"$B\",\"bank_code\":\"$B\",\"full_name\":\"Round-trip $B\",\"logo\":\"\",\"website\":\"\",\"bank_routings\":[]}" \
    | grep -qE '"id"|OBP-34000|already exists' && echo "  bank $B ok" || echo "  bank $B — check"
  grant "$SA_ID" "$B" CanCreateAccount
  grant "$SA_ID" "$B" CanConfigureOpenCorridorBroker
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
  "address_pattern":"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$","example_address":"rt.bank.b",
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

echo "== creating node service users"
for U in "$NODE_A_USER" "$NODE_B_USER"; do
  api POST /obp/v5.1.0/users \
    "{\"email\":\"$U@example.com\",\"username\":\"$U\",\"password\":\"$NODE_PASS\",\"first_name\":\"Node\",\"last_name\":\"$U\"}" \
    >/dev/null
done
# DirectLogin refuses unvalidated emails (OBP-20073); no API path for it, so
# flip the flag in Postgres (password from default.props db.url).
echo "  (validating emails in Postgres — needed for DirectLogin)"
PGPASS=$(grep -E '^db.url' ~/Documents/workspace_2024/OBP-API-Simon/OBP-API/obp-api/src/main/resources/props/default.props | head -1 | grep -oP 'password=\K[^&\s]+')
PGPASSWORD="$PGPASS" psql -h localhost -U obp -d sandbox -q \
  -c "UPDATE authuser SET validated=true WHERE username IN ('$NODE_A_USER','$NODE_B_USER');"
UA_ID=$(dl "$NODE_A_USER" "$NODE_PASS" >/dev/null; api GET "/obp/v5.1.0/users/username/$NODE_A_USER" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("user_id",""))')
UB_ID=$(api GET "/obp/v5.1.0/users/username/$NODE_B_USER" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("user_id",""))')
echo "  $NODE_A_USER=$UA_ID  $NODE_B_USER=$UB_ID"
[ -n "$UA_ID" ] && [ -n "$UB_ID" ] || { echo "FATAL: node users missing"; exit 1; }

echo "== granting node A the report-back role"
grant "$UA_ID" "$BANK_A" CanAttachOpenCorridorPromise

echo "== granting the node service users the settle role at their own bank"
# The node settle-request endpoint (POST /obp-bank-node/v5.1.0/settlements)
# calls OBP-API's settlement resource as the node's own M2M user; the
# bank-scoped CanSettleOpenCorridor must be held at the node's bank. Either
# side of a corridor may trigger, so both nodes get it.
grant "$UA_ID" "$BANK_A" CanSettleOpenCorridor
grant "$UB_ID" "$BANK_B" CanSettleOpenCorridor

echo "== creating accounts (KES, zero initial balance)"
acct() { # acct BANK ACCOUNT OWNER LABEL
  api PUT "/obp/v5.1.0/banks/$1/accounts/$2" \
    "{\"user_id\":\"$3\",\"label\":\"$4\",\"product_code\":\"OPEN_CORRIDOR\",\"balance\":{\"currency\":\"KES\",\"amount\":\"0\"},\"branch_id\":\"\",\"account_routings\":[]}" \
    | grep -qE 'account_id|OBP-30208|already exists' && echo "  $1/$2 ok" || echo "  $1/$2 — check"
}
acct "$BANK_A" "settlement-a" "$UA_ID" "Node A corridor account"
acct "$BANK_B" "settlement-b" "$UB_ID" "Node B beneficiary account"
# The internal net-posting accounts settlePair expects (per-bank, fixed ids):
acct "$BANK_A" "OBP-OUTGOING-SETTLEMENT-ACCOUNT" "$SA_ID" "OC outgoing settlement"
acct "$BANK_A" "OBP-INCOMING-SETTLEMENT-ACCOUNT" "$SA_ID" "OC incoming settlement"
acct "$BANK_B" "OBP-OUTGOING-SETTLEMENT-ACCOUNT" "$SA_ID" "OC outgoing settlement"
acct "$BANK_B" "OBP-INCOMING-SETTLEMENT-ACCOUNT" "$SA_ID" "OC incoming settlement"

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

echo "== registering Open Corridor brokers"
api PUT "/obp/v7.0.0/banks/$BANK_A/open-corridor/broker" \
  "{\"host\":\"localhost\",\"port\":5672,\"virtual_host\":\"/bank.$BANK_A\",\"username\":\"bank_a_node\",\"password\":\"rtpass-a\",\"use_ssl\":false,\"settlement_address\":\"$ADDR_A\"}" \
  | grep -q '"bank_id"' && echo "  broker $BANK_A ok" || echo "  broker $BANK_A — check"
api PUT "/obp/v7.0.0/banks/$BANK_B/open-corridor/broker" \
  "{\"host\":\"localhost\",\"port\":5672,\"virtual_host\":\"/bank.$BANK_B\",\"username\":\"bank_b_node\",\"password\":\"rtpass-b\",\"use_ssl\":false,\"settlement_address\":\"$ADDR_B\"}" \
  | grep -q '"bank_id"' && echo "  broker $BANK_B ok" || echo "  broker $BANK_B — check"

echo "== writing tokens to env.sh"
TOK_A=$(dl "$NODE_A_USER" "$NODE_PASS")
TOK_B=$(dl "$NODE_B_USER" "$NODE_PASS")
DIR="$(cd "$(dirname "$0")" && pwd)"
cat > "$DIR/env.sh" <<EOF
# generated by setup_obp.sh $(date -Is)
export RT_ADMIN_TOKEN="$TOK"
export RT_NODE_A_TOKEN="$TOK_A"
export RT_NODE_B_TOKEN="$TOK_B"
EOF
[ -n "$TOK_A" ] && [ -n "$TOK_B" ] && echo "setup complete." || echo "WARN: node token(s) empty — check user creation"

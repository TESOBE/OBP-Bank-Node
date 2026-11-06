#!/usr/bin/env bash
# Create the OBP-API Consumers the localhost round trip needs, and write their
# credentials into dev/local.env (idempotent: safe to re-run).
#
#   obp-bank-node-a          no redirect   -> NODE_A_CLIENT_ID / _SECRET / NODE_A_CONSUMER_ID
#   obp-bank-node-b          no redirect   -> NODE_B_CLIENT_ID / _SECRET / NODE_B_CONSUMER_ID
#                            (the nodes, as Platform Apps: OAuth2 client credentials at OBP-OIDC)
#   obp-bank-node-roundtrip  no redirect   -> RT_CONSUMER_KEY (DirectLogin for the setup scripts' admin)
#   obp-bank-node-app-a      :8091/setup/callback -> UI_A_OAUTH_CLIENT_ID / _SECRET
#   obp-bank-node-app-b      :8092/setup/callback -> UI_B_OAUTH_CLIENT_ID / _SECRET
#
# Logs in to OBP-API as the OIDC operator from OBP-API's props (oidc_operator_username,
# oidc_operator_initial_password, oidc_operator_consumer_key: the same login OBP-OIDC uses;
# Boot gives that user CanCreateConsumer and CanGetConsumers). Override with
# RT_OPERATOR_USER / RT_OPERATOR_PASS / RT_OPERATOR_CONSUMER_KEY in dev/local.env.
#
# OBP-API returns a Consumer's secret only when it is created. For a Consumer that already
# exists, the key and secret are read from OBP-API's Postgres (db.url in the props file);
# if that fails, delete the Consumer (e.g. in API Manager) and run this again.
#
# Secrets and consumer keys are never printed; Consumers are shown by consumer_id.
#
# Usage: dev/create_consumers.sh [--dry-run]
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
LOCAL_ENV="$DIR/local.env"
DRY_RUN=false
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true

if [ ! -f "$LOCAL_ENV" ]; then
  if $DRY_RUN; then
    echo "dev/local.env does not exist yet (would be created from local.env.example)"
  else
    cp "$DIR/local.env.example" "$LOCAL_ENV"
    chmod 600 "$LOCAL_ENV"
    echo "created dev/local.env from local.env.example"
  fi
fi
# shellcheck disable=SC1090
[ -f "$LOCAL_ENV" ] && source "$LOCAL_ENV"

OBP="${RT_OBP_URL:-http://localhost:8080}"
PROPS_FILE="${RT_OBP_PROPS_FILE:-}"
[ -n "$PROPS_FILE" ] && [ -f "$PROPS_FILE" ] || {
  echo "FATAL: set RT_OBP_PROPS_FILE in dev/local.env to OBP-API's props file (default.props)" >&2; exit 1; }

TMP="$(mktemp -d)"; chmod 700 "$TMP"; trap 'rm -rf "$TMP"' EXIT

# Value of KEY=... in a props / env file (first uncommented match, quotes stripped).
get_value() {
  [ -f "$1" ] || return 0
  KEY="$2" awk 'BEGIN{k=ENVIRON["KEY"] "="} index($0,k)==1{v=substr($0,length(k)+1); gsub(/^["\047]|["\047]$/,"",v); print v; exit}' "$1"
}

# Replace KEY=... in dev/local.env, or append it. The file is backed up once per run.
BACKED_UP=false
set_local() {
  local key="$1" value="$2"
  [ "$(get_value "$LOCAL_ENV" "$key")" = "$value" ] && { echo "   $key already set"; return 0; }
  if $DRY_RUN; then echo "   would set $key in dev/local.env"; return 0; fi
  if ! $BACKED_UP; then cp -p "$LOCAL_ENV" "$LOCAL_ENV.bak.$(date +%Y%m%d-%H%M%S)"; BACKED_UP=true; fi
  local tmp; tmp="$(mktemp "$LOCAL_ENV.XXXXXX")"
  KEY="$key" VALUE="$value" awk 'BEGIN{k=ENVIRON["KEY"]; v=ENVIRON["VALUE"]; d=0}
    index($0,k "=")==1{ if(!d) print k "=" v; d=1; next } {print} END{ if(!d) print k "=" v }' "$LOCAL_ENV" > "$tmp"
  chmod 600 "$tmp"; mv "$tmp" "$LOCAL_ENV"
  echo "   set $key in dev/local.env"
}

# key_c or secret of an existing Consumer, from OBP-API's Postgres. Empty if unreadable.
db_consumer_value() {
  local name="$1" column="$2" url host port db user pass
  url="$(get_value "$PROPS_FILE" db.url)"
  [[ "$url" =~ ^jdbc:postgresql://([^:/]+)(:([0-9]+))?/([^?]+)\?(.*)$ ]] || return 0
  host="${BASH_REMATCH[1]}"; port="${BASH_REMATCH[3]:-5432}"; db="${BASH_REMATCH[4]}"
  user="$(tr '&' '\n' <<<"${BASH_REMATCH[5]}" | sed -n 's/^user=//p')"
  pass="$(tr '&' '\n' <<<"${BASH_REMATCH[5]}" | sed -n 's/^password=//p')"
  PGPASSWORD="$pass" psql -h "$host" -p "$port" -U "$user" -d "$db" -tAX -v name="$name" 2>/dev/null \
    <<<"select $column from consumer where name = :'name' order by createdat desc limit 1" || true
}

json() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {}, {"d": d}) or "")' "$1" 2>/dev/null || true; }

# --- log in as the operator (credentials go through a header file, not the process list)
OP_USER="${RT_OPERATOR_USER:-$(get_value "$PROPS_FILE" oidc_operator_username)}"
OP_PASS="${RT_OPERATOR_PASS:-$(get_value "$PROPS_FILE" oidc_operator_initial_password)}"
OP_CK="${RT_OPERATOR_CONSUMER_KEY:-$(get_value "$PROPS_FILE" oidc_operator_consumer_key)}"
[ -n "$OP_USER" ] && [ -n "$OP_PASS" ] && [ -n "$OP_CK" ] || {
  echo "FATAL: no oidc_operator_username / _initial_password / _consumer_key in $PROPS_FILE" >&2
  echo "       (set them as OBP-OIDC's README describes, or RT_OPERATOR_* in dev/local.env)" >&2; exit 1; }
curl -sf -o /dev/null "$OBP/obp/v6.0.0/root" || { echo "FATAL: OBP-API is not answering at $OBP" >&2; exit 1; }

printf 'DirectLogin: username="%s",password="%s",consumer_key="%s"\n' "$OP_USER" "$OP_PASS" "$OP_CK" > "$TMP/login"
TOKEN="$(curl -s -X POST "$OBP/obp/v6.0.0/my/logins/direct" -H 'Content-Type: application/json' -H @"$TMP/login" | json 'd.get("token")')"
[ -n "$TOKEN" ] || { echo "FATAL: DirectLogin as $OP_USER failed" >&2; exit 1; }
printf 'Authorization: DirectLogin token="%s"\n' "$TOKEN" > "$TMP/auth"
echo "logged in to OBP-API as $OP_USER"

CONSUMERS_JSON="$(curl -s "$OBP/obp/v6.0.0/management/consumers?limit=1000" -H @"$TMP/auth")"
echo "$CONSUMERS_JSON" | python3 -c 'import json,sys; json.load(sys.stdin)["consumers"]' 2>/dev/null \
  || { echo "FATAL: could not list Consumers (needs CanGetConsumers)" >&2; exit 1; }
OP_EMAIL="$(get_value "$PROPS_FILE" oidc_operator_email)"

problems=0
# name | redirect | key variable | secret variable (empty: key only) | consumer_id variable (optional)
for row in \
  "obp-bank-node-a||NODE_A_CLIENT_ID|NODE_A_CLIENT_SECRET|NODE_A_CONSUMER_ID" \
  "obp-bank-node-b||NODE_B_CLIENT_ID|NODE_B_CLIENT_SECRET|NODE_B_CONSUMER_ID" \
  "obp-bank-node-roundtrip||RT_CONSUMER_KEY|" \
  "obp-bank-node-app-a|http://localhost:8091/setup/callback|UI_A_OAUTH_CLIENT_ID|UI_A_OAUTH_CLIENT_SECRET" \
  "obp-bank-node-app-b|http://localhost:8092/setup/callback|UI_B_OAUTH_CLIENT_ID|UI_B_OAUTH_CLIENT_SECRET"; do
  IFS='|' read -r NAME REDIRECT KEY_VAR SECRET_VAR ID_VAR <<<"$row"
  echo ""
  echo "== $NAME"
  ID="$(echo "$CONSUMERS_JSON" | NAME="$NAME" python3 -c 'import json,os,sys
c=[x for x in json.load(sys.stdin)["consumers"] if x.get("app_name")==os.environ["NAME"]]
print(c[0]["consumer_id"] if c else "")')"
  if [ -n "$ID" ]; then
    echo "   exists (consumer_id $ID)"
    KEY="$(db_consumer_value "$NAME" key_c)"; SECRET="$(db_consumer_value "$NAME" secret)"
    if [ -z "$KEY" ] || { [ -n "$SECRET_VAR" ] && [ -z "$SECRET" ]; }; then
      if [ -n "$(get_value "$LOCAL_ENV" "$KEY_VAR")" ]; then
        echo "   could not read it from Postgres; keeping the values already in dev/local.env"
        [ -n "$ID_VAR" ] && set_local "$ID_VAR" "$ID"
      else
        echo "   could not read its key/secret from Postgres: delete this Consumer and run again"; problems=1
      fi
      continue
    fi
  else
    if $DRY_RUN; then echo "   would create (redirect: ${REDIRECT:-none})"; continue; fi
    RESP="$(NAME="$NAME" REDIRECT="$REDIRECT" EMAIL="${OP_EMAIL:-dev@example.com}" python3 -c 'import json,os
print(json.dumps({"app_name": os.environ["NAME"], "app_type": "Confidential",
  "description": "Local dev Consumer for OBP-Bank-Node (dev/create_consumers.sh)",
  "developer_email": os.environ["EMAIL"], "company": "Local dev",
  "redirect_url": os.environ["REDIRECT"], "enabled": True}))' \
      | curl -s -X POST "$OBP/obp/v5.1.0/management/consumers" -H @"$TMP/auth" -H 'Content-Type: application/json' --data-binary @-)"
    KEY="$(echo "$RESP" | json 'd.get("consumer_key")')"
    SECRET="$(echo "$RESP" | json 'd.get("consumer_secret")')"
    ID="$(echo "$RESP" | json 'd.get("consumer_id")')"
    [ -n "$KEY" ] || { echo "   create failed: $(echo "$RESP" | json 'd.get("message")')"; problems=1; continue; }
    echo "   created (consumer_id $ID)"
  fi
  set_local "$KEY_VAR" "$KEY"
  [ -n "$SECRET_VAR" ] && set_local "$SECRET_VAR" "$SECRET"
  [ -n "$ID_VAR" ] && set_local "$ID_VAR" "$ID"
done

echo ""
[ "$problems" -eq 0 ] && echo "Consumers in place." || { echo "Finished with problems, see above."; exit 1; }

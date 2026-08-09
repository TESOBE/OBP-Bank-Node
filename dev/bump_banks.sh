#!/usr/bin/env bash
# Bump the demo bank generation: rt.bank.aN / rt.bank.bN  ->  aN+1 / bN+1.
#
# Rewrites every dev config and script to a fresh pair of bank ids and fresh
# node data directories, so demos start with empty tables and no history —
# nothing is deleted: previous generations stay archived in OBP-API and in
# their old dev/data/node-*N directories.
#
# Carried over unchanged on purpose: the Cardano wallets and settlement
# addresses (bank-agnostic), the node service users rt.node.a / rt.node.b
# (they just get grants at the new banks), and all system-level OBP state
# (routing schemes, admin roles).
#
# Usage:
#   dev/bump_banks.sh            # bump N -> N+1
#   dev/bump_banks.sh --dry-run  # show what would change
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"

DRY_RUN=false
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true

FILES=(
  node-a.yaml node-b.yaml app-a.yaml app-b.yaml
  run_roundtrip.sh setup_rabbitmq.sh setup_obp.sh
  start_node_a.sh start_node_b.sh start_app_a.sh start_app_b.sh
)

# Current generation, read from app-a.yaml's bank id (rt.bank.aN).
N="$(grep -oP 'id: "rt\.bank\.a\K[0-9]+' "$DIR/app-a.yaml" | head -1)"
if [[ -z "$N" ]]; then
  echo "ERROR: could not read a numeric generation from app-a.yaml (expected id: \"rt.bank.a<N>\")" >&2
  exit 1
fi
NEXT=$((N + 1))

echo "bumping bank generation: rt.bank.a$N / rt.bank.b$N  ->  rt.bank.a$NEXT / rt.bank.b$NEXT"
echo "node data dirs:          dev/data/node-{a,b}$N      ->  dev/data/node-{a,b}$NEXT"

for f in "${FILES[@]}"; do
  path="$DIR/$f"
  hits="$(grep -c "rt\.bank\.[ab]$N\b\|data/node-[ab]$N/" "$path" || true)"
  if $DRY_RUN; then
    echo "  $f: $hits line(s) would change"
    continue
  fi
  sed -i \
    -e "s/rt\.bank\.a$N\b/rt.bank.a$NEXT/g" \
    -e "s/rt\.bank\.b$N\b/rt.bank.b$NEXT/g" \
    -e "s|data/node-a$N/|data/node-a$NEXT/|g" \
    -e "s|data/node-b$N/|data/node-b$NEXT/|g" \
    "$path"
  echo "  $f: updated ($hits line(s))"
done

$DRY_RUN && exit 0

leftover="$(grep -rn "rt\.bank\.[ab]$N\b" "$DIR"/*.yaml "$DIR"/*.sh --exclude=bump_banks.sh || true)"
if [[ -n "$leftover" ]]; then
  echo "WARNING: old-generation references remain:" >&2
  echo "$leftover" >&2
fi

cat <<EOF

Done. Bring-up for the new generation:
  1. dev/setup_rabbitmq.sh                  # new vhosts /bank.rt.bank.{a,b}$NEXT
  2. (restart OBP-API only if its build changed)
  3. dev/setup_obp.sh                       # grants, broker registrations, fresh env.sh
  4. restart nodes + apps (start_node_*.sh, start_app_*.sh)
  5. each /setup page: log in, Apply the missing items to green
EOF

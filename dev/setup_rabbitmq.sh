#!/usr/bin/env bash
# Copyright (C) 2025-2026 TESOBE GmbH
# SPDX-License-Identifier: AGPL-3.0-or-later
# Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.
# Round-trip test: per-bank RabbitMQ vhosts + users (idempotent).
# Convention: vhost /bank.{bank_id}, one service user per bank's Bank Node.
set -euo pipefail

MGMT="http://localhost:15672/api"
ADMIN="guest:guest"

# vhost, user, password
declare -a ROWS=(
  "/bank.rt.bank.a4 bank_a_node rtpass-a"
  "/bank.rt.bank.b4 bank_b_node rtpass-b"
)

for row in "${ROWS[@]}"; do
  read -r VHOST USER PASS <<<"$row"
  ENC=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "$VHOST")
  curl -sf -u "$ADMIN" -X PUT "$MGMT/vhosts/$ENC" >/dev/null
  curl -sf -u "$ADMIN" -X PUT "$MGMT/users/$USER" \
    -H 'content-type: application/json' \
    -d "{\"password\":\"$PASS\",\"tags\":\"\"}" >/dev/null
  curl -sf -u "$ADMIN" -X PUT "$MGMT/permissions/$ENC/$USER" \
    -H 'content-type: application/json' \
    -d '{"configure":".*","write":".*","read":".*"}' >/dev/null
  echo "ok: vhost $VHOST user $USER"
done
echo "RabbitMQ setup complete."

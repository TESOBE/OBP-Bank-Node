#!/usr/bin/env bash
#
# Build the per-network OBP Bank Node images. Each image bakes its Cardano
# network (OBP_BN_BLOCKCHAIN__CARDANO__NETWORK), so banks pick a network by
# pulling a tag, never by config.
#
# Usage:
#   ./docker/build-images.sh                    # preprod, preview, mainnet
#   ./docker/build-images.sh mainnet            # one network
#   ./docker/build-images.sh preprod preview    # a subset
#
# Tags produced per network:
#   obp-bank-node:<network>             (moving tag)
#   obp-bank-node:<version>-<network>   (pinned to the workspace version)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

# Podman and Docker both work; override with CONTAINER_ENGINE=podman|docker.
if [ -z "${CONTAINER_ENGINE:-}" ]; then
  if command -v docker >/dev/null 2>&1; then CONTAINER_ENGINE=docker
  elif command -v podman >/dev/null 2>&1; then CONTAINER_ENGINE=podman
  else echo "ERROR: neither 'docker' nor 'podman' is installed." >&2; exit 1
  fi
fi

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -1)
[ -n "$VERSION" ] || { echo "ERROR: could not read workspace version from Cargo.toml" >&2; exit 1; }

NETWORKS=("$@")
[ ${#NETWORKS[@]} -gt 0 ] || NETWORKS=(preprod preview mainnet)

for net in "${NETWORKS[@]}"; do
  case "$net" in
    preprod|preview|mainnet) ;;
    *) echo "ERROR: unknown network '$net' (expected preprod, preview, or mainnet)" >&2; exit 1 ;;
  esac
done

for net in "${NETWORKS[@]}"; do
  echo "==> Building obp-bank-node:$net (version $VERSION, $CONTAINER_ENGINE)"
  "$CONTAINER_ENGINE" build \
    --file "$SCRIPT_DIR/Dockerfile" \
    --build-arg CARDANO_NETWORK="$net" \
    --tag "obp-bank-node:$net" \
    --tag "obp-bank-node:$VERSION-$net" \
    "$REPO_ROOT"
done

echo
echo "==> Built:"
for net in "${NETWORKS[@]}"; do
  echo "    obp-bank-node:$net  /  obp-bank-node:$VERSION-$net"
done

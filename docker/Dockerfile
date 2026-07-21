# syntax=docker/dockerfile:1
#
# OBP Bank Node image, built per Cardano network.
#
# CARDANO_NETWORK is a REQUIRED build arg (preprod | preview | mainnet). It is
# baked into the image as OBP_BN_BLOCKCHAIN__CARDANO__NETWORK, so a bank never
# sets the network in its config — pulling obp-bank-node:mainnet IS the network
# choice. load_config() refuses to boot if the mounted YAML names a different
# network. There is deliberately no default: an image that doesn't know its
# network must fail at build time, not fall back to a testnet.
#
# Build (from the repo root):
#   ./docker/build-images.sh              # all three networks
#   ./docker/build-images.sh mainnet      # just one

FROM rust:1.89-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p obp-bank-node \
    && cp target/release/obp-bank-node /obp-bank-node

FROM debian:bookworm-slim
ARG CARDANO_NETWORK
RUN case "$CARDANO_NETWORK" in \
      preprod|preview|mainnet) ;; \
      *) echo "ERROR: build arg CARDANO_NETWORK must be preprod, preview, or mainnet (got '${CARDANO_NETWORK:-<unset>}')" >&2; exit 1 ;; \
    esac

# ca-certificates: outbound TLS (OBP-API, CoinGecko). curl: container healthcheck.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --home-dir /app banknode \
    && mkdir -p /app/secrets /app/outbox \
    && chown -R banknode:banknode /app

COPY --from=builder /obp-bank-node /usr/local/bin/obp-bank-node

WORKDIR /app
USER banknode

# The config defaults resolve relative to /app: the bank mounts its YAML at
# /app/obp-bank-node-config.yaml and its signing key at
# /app/secrets/cardano.skey; the outbox DB lands in the /app/outbox volume.
ENV OBP_BANK_NODE_CONFIG=/app/obp-bank-node-config.yaml \
    OBP_BN_BLOCKCHAIN__CARDANO__NETWORK=${CARDANO_NETWORK}

EXPOSE 8088

LABEL org.opencontainers.image.title="OBP Bank Node" \
      org.opencontainers.image.source="https://github.com/OpenBankProject/OBP-Bank-Node" \
      network.cardano="${CARDANO_NETWORK}"

ENTRYPOINT ["/usr/local/bin/obp-bank-node"]

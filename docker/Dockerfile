# syntax=docker/dockerfile:1.6
# Multi-stage build — final image is FROM scratch + ca-certs only, ~20MB target.
FROM golang:1.24 AS build

WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .

ARG VERSION=0.1.0
RUN CGO_ENABLED=0 GOOS=linux GOARCH=amd64 \
    go build -trimpath -ldflags="-s -w -X main.version=${VERSION}" \
        -o /out/obp-bank-node ./cmd/obp-bank-node

FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=build /out/obp-bank-node /app/obp-bank-node
COPY obp-bank-node-config.yaml.example /app/obp-bank-node-config.yaml.example

EXPOSE 8088 9090

USER nonroot:nonroot
ENTRYPOINT ["/app/obp-bank-node"]
CMD ["-config", "/app/obp-bank-node-config.yaml"]

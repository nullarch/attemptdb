# attemptdb-server — one static binary, one volume.
#
# Build:   docker build -f deploy/Dockerfile -t attemptdb-server .
# Run:     see deploy/docker-compose.yml and docs/deploy.md
#
# The builder is Alpine so the default Rust target is musl and the result is
# fully static (the same shape as the `linux-*-musl` release assets).
# `zstd-sys` compiles C, hence build-base. The `attempt` CLI is built too:
# operators use it on the host for `attempt verify --db /data/tenants/<t>`
# and `attempt import vibemon-export` backfills.

FROM rust:1.98-alpine AS builder
RUN apk add --no-cache build-base musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY fixtures ./fixtures
COPY spec ./spec
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p attemptdb-server -p attemptdb \
    && mkdir -p /out \
    && cp target/release/attemptdb-server target/release/attempt /out/

FROM alpine:3.21
RUN addgroup -S -g 10001 attemptdb && adduser -S -u 10001 -G attemptdb -h /data attemptdb \
    && mkdir -p /data && chown attemptdb:attemptdb /data
COPY --from=builder /out/attemptdb-server /out/attempt /usr/local/bin/
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh
USER attemptdb
VOLUME ["/data"]
EXPOSE 8787
# Plain HTTP/1.1 on purpose: terminate TLS in front (docs/deploy.md).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD wget -qO- http://127.0.0.1:8787/v1/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

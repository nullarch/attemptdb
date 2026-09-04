# attemptdb-server — the release binary, downloaded and verified.
#
#   fly deploy . -c deploy/fly.toml --dockerfile deploy/Dockerfile \
#       --build-arg ATTEMPTDB_VERSION=0.2.3            (or deploy/fly-up.sh)
#
# A deploy is a download: the release workflow builds `attemptdb-server`
# for the static Linux targets and attests it; this image fetches that
# archive, checks it against the release's SHA256SUMS, and installs it.
# Seconds, not the 20-minute source build the host's builder used to do.
# For a change that is not released yet, deploy/Dockerfile.source builds
# from the tree (deploy/fly-up.sh --source).

FROM alpine:3.21
ARG ATTEMPTDB_VERSION
ARG TARGETARCH
ARG ATTEMPTDB_REPO=nullarch/attemptdb
RUN test -n "$ATTEMPTDB_VERSION" || { echo "build-arg ATTEMPTDB_VERSION is required"; exit 1; }
RUN apk add --no-cache ca-certificates curl \
    && case "$TARGETARCH" in \
         amd64) T=x86_64-unknown-linux-musl ;; \
         arm64) T=aarch64-unknown-linux-musl ;; \
         *) echo "unsupported TARGETARCH $TARGETARCH"; exit 1 ;; \
       esac \
    && STEM="attemptdb-server-${ATTEMPTDB_VERSION}-${T}" \
    && BASE="https://github.com/${ATTEMPTDB_REPO}/releases/download/v${ATTEMPTDB_VERSION}" \
    && cd /tmp \
    && curl -fsSL -o "$STEM.tar.gz" "$BASE/$STEM.tar.gz" \
    && curl -fsSL -o SHA256SUMS "$BASE/SHA256SUMS" \
    && grep " $STEM.tar.gz\$" SHA256SUMS | sha256sum -c - \
    && tar -xzf "$STEM.tar.gz" \
    && install -m 0755 "$STEM/attemptdb-server" "$STEM/attempt" /usr/local/bin/ \
    && install -m 0755 "$STEM/entrypoint.sh" /usr/local/bin/entrypoint.sh \
    && rm -rf /tmp/"$STEM"* /tmp/SHA256SUMS \
    && apk del curl \
    && addgroup -S -g 10001 attemptdb && adduser -S -u 10001 -G attemptdb -h /data attemptdb \
    && mkdir -p /data && chown attemptdb:attemptdb /data \
    && /usr/local/bin/attemptdb-server --version
USER attemptdb
VOLUME ["/data"]
EXPOSE 8787
# Plain HTTP/1.1 on purpose: terminate TLS in front (docs/deploy.md).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD wget -qO- http://127.0.0.1:8787/v1/health >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

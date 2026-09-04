#!/bin/sh
# Bring attemptdb-server up on Fly.io — idempotent; run it again after any
# change to deploy/fly.toml or a new release.
#
#   fly auth login                       # once, interactive
#   ATTEMPTDB_ADMIN_TOKEN_FILE=~/.attemptdb-admin-token deploy/fly-up.sh
#
# Steps: app → volume → admin token secret → deploy → certificate → the
# DNS record to add → health. Each step is skipped when already done.
# The admin token is read from ATTEMPTDB_ADMIN_TOKEN_FILE (one line) or, if
# that file does not exist, generated into it (mode 0600): the same value
# goes to the product's backend (vibemon-web: ATTEMPTDB_ADMIN_TOKEN). The
# token is never echoed.
set -eu
cd "$(dirname "$0")/.."

APP="${FLY_APP:-attemptdb-sync}"
REGION="${FLY_REGION:-iad}"
VOLUME="attemptdb_data"
HOST="${SYNC_HOST:-sync.vibemon.dev}"
TOKEN_FILE="${ATTEMPTDB_ADMIN_TOKEN_FILE:-$HOME/.attemptdb-admin-token}"

say() { printf '%s\n' "$*"; }
fail() { printf 'fly-up: %s\n' "$*" >&2; exit 1; }

command -v fly >/dev/null 2>&1 || fail "flyctl is not installed (https://fly.io/docs/flyctl/install/)"
fly auth whoami >/dev/null 2>&1 || fail "not logged in: run 'fly auth login' first"

# 1. The app.
if fly apps list --json 2>/dev/null | grep -q "\"Name\": *\"$APP\""; then
    say "app $APP exists"
else
    fly apps create "$APP" --org "${FLY_ORG:-personal}"
fi

# 2. One volume, in the primary region. Never a second one: two machines
#    would be two silently diverging databases (see fly.toml).
if fly volumes list -a "$APP" --json 2>/dev/null | grep -q "\"name\": *\"$VOLUME\""; then
    say "volume $VOLUME exists"
else
    fly volumes create "$VOLUME" -a "$APP" --region "$REGION" --size 1 --yes
fi

# 3. The admin token, as a secret.
if [ ! -f "$TOKEN_FILE" ]; then
    umask 077
    openssl rand -hex 32 > "$TOKEN_FILE"
    say "generated an admin token into $TOKEN_FILE (keep it; the product's backend needs the same value)"
fi
if fly secrets list -a "$APP" 2>/dev/null | grep -q '^ATTEMPTDB_ADMIN_TOKEN'; then
    say "secret ATTEMPTDB_ADMIN_TOKEN is set (to rotate: fly secrets set ATTEMPTDB_ADMIN_TOKEN=… -a $APP)"
else
    fly secrets set -a "$APP" --stage "ATTEMPTDB_ADMIN_TOKEN=$(tr -d '\n' < "$TOKEN_FILE")" >/dev/null
    say "secret ATTEMPTDB_ADMIN_TOKEN staged"
fi

# 4. Deploy. Default: the released version named in Cargo.toml, downloaded
#    and verified by deploy/Dockerfile (seconds). --source: build the tree
#    with deploy/Dockerfile.source (an unreleased change; ~5 minutes).
VERSION="$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -n 1)"
if [ "${1:-}" = "--source" ]; then
    # `--dockerfile` loses to `[build] dockerfile` in the config, silently:
    # flyctl builds deploy/Dockerfile (the download) and dies on the missing
    # ATTEMPTDB_VERSION build arg. Point a copy of the config at the source
    # image instead. It has to sit next to fly.toml — the path in `[build]`
    # is resolved against the config's own directory.
    SRC_CFG="deploy/.fly.source.toml"
    trap 'rm -f "$SRC_CFG"' EXIT INT TERM
    sed 's|dockerfile = "Dockerfile"|dockerfile = "Dockerfile.source"|' deploy/fly.toml > "$SRC_CFG"
    grep -q 'Dockerfile.source' "$SRC_CFG" || fail "fly.toml no longer names the Dockerfile the way this script rewrites it"
    fly deploy . -c "$SRC_CFG" -a "$APP" --ha=false
else
    if ! curl -fsSLI -o /dev/null "https://github.com/nullarch/attemptdb/releases/download/v${VERSION}/attemptdb-server-${VERSION}-x86_64-unknown-linux-musl.tar.gz"; then
        fail "release v${VERSION} has no server asset yet (tag it and wait for the Release workflow, or deploy the tree with --source)"
    fi
    fly deploy . -c deploy/fly.toml --dockerfile deploy/Dockerfile --build-arg "ATTEMPTDB_VERSION=${VERSION}" -a "$APP" --ha=false
fi

# 4b. Public addresses. The first deploy tries to allocate them itself and
#     can fail on an org-owned app ("org_slug is only supported with
#     private_v6"); a shared IPv4 is free, the IPv6 is dedicated.
# `fly ips list` prints a box-drawn table: VERSION │ IP │ TYPE │ …
ip_of() { fly ips list -a "$APP" 2>/dev/null | awk -F'│' -v v="$1" 'NR>1 && $1 ~ v {gsub(/ /,"",$2); print $2; exit}'; }
[ -n "$(ip_of v6)" ] || fly ips allocate-v6 -a "$APP"
[ -n "$(ip_of v4)" ] || fly ips allocate-v4 --shared -a "$APP"

# 5. The certificate for the product's hostname.
if fly certs list -a "$APP" 2>/dev/null | grep -q "$HOST"; then
    say "certificate for $HOST requested"
else
    fly certs add "$HOST" -a "$APP"
fi

# 6. What DNS needs. With a shared IPv4 the certificate is verified over
#    the AAAA record (or a CNAME, which covers both).
V4="$(ip_of v4)"
V6="$(ip_of v6)"
say ""
say "DNS: add at the zone for ${HOST#*.} — either"
say "  $HOST.   CNAME  $APP.fly.dev."
say "or both"
say "  $HOST.   A      ${V4:-<fly ips list -a $APP>}"
say "  $HOST.   AAAA   ${V6:-<fly ips list -a $APP>}"
say ""

# 7. Health, on the app hostname now and on $HOST once DNS resolves.
if curl -fsS "https://$APP.fly.dev/v1/health"; then
    say ""
    say "up: https://$APP.fly.dev/v1/health"
else
    say "not answering yet; watch: fly logs -a $APP"
fi
if curl -fsS --max-time 5 "https://$HOST/v1/health" >/dev/null 2>&1; then
    say "up: https://$HOST/v1/health"
else
    say "https://$HOST is not resolving or its certificate is not issued yet: fly certs check $HOST -a $APP"
fi

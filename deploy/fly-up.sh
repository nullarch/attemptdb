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

# 4. Deploy the image (remote builder; the Dockerfile builds the workspace).
fly deploy -c deploy/fly.toml -a "$APP" --ha=false

# 5. The certificate for the product's hostname.
if fly certs list -a "$APP" 2>/dev/null | grep -q "$HOST"; then
    say "certificate for $HOST requested"
else
    fly certs add "$HOST" -a "$APP"
fi

# 6. What DNS needs. Fly's anycast address is stable per app.
V4="$(fly ips list -a "$APP" --json 2>/dev/null | sed -n 's/.*"Address": *"\([0-9.]*\)".*/\1/p' | head -n 1)"
say ""
say "DNS: add at the zone for ${HOST#*.}:"
say "  $HOST.   A      ${V4:-<fly ips list -a $APP>}"
say "  (or a CNAME to $APP.fly.dev — A is what fly certs verifies fastest)"
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

#!/bin/sh
# vibemon.dev's one-line install: AttemptDB on this machine, linked to the
# VibeMon sync server with a one-time pairing token from the web.
#
#   curl -fsSL https://vibemon.dev/install.sh | sh -s -- pair_abc123
#
# What it does, in this order — and the order is the safety:
#
#   1. checks the pairing token with the server before touching anything;
#      no token (or a dead one) → nothing on this machine changes
#   2. installs (or upgrades) the `attempt` binary, verified against the
#      release's SHA256SUMS
#   3. creates the local database if there is none — an existing one keeps
#      its capture mode and settings
#   4. pairs: the token plus the database's own device id become a device
#      key, proven by an authenticated handshake, saved only on success
#   5. installs the agent hooks next to any existing ones
#   6. registers the background daemon (launchd / systemd --user)
#   7. uploads once and requires the server to accept it
#   8. only then removes the legacy VibeMon hooks (~/.vibemon/notify.sh)
#   9. shows `attempt doctor`
#
# Run again any time: it upgrades, repairs hooks, re-registers the daemon,
# and keeps the existing connection when no token is given. Without a token
# on a machine that was never connected it exits 0 and changes nothing —
# that is the path the legacy client's daily auto-update takes (detached,
# no terminal). Typed by a person at a terminal on such a machine, it uses
# the older client's stored account key (~/.vibemon/api-key) to pair: the
# app's "update available" command is exactly that.
#
# Options
#   pair_TOKEN, --pair TOKEN   one-time pairing token from vibemon.dev/devices
#   vbm_KEY            the account API key from the older install command:
#                      exchanged for a pairing token at the web first
#   --server URL       sync server (default: https://sync.vibemon.dev, or
#                      $VIBEMON_SYNC_URL; the web's answer to a vbm_ key
#                      names the server too)
#   --web URL          the product web (default: https://vibemon.dev)
#   --profile NAME     what leaves this machine: metadata_only | semantic | full
#                      (default semantic: metadata plus this device's
#                      inferences with evidence — never prompts or output)
#   --local-content    keep prompts / commands / tool output in the LOCAL
#                      encrypted database on a NEW install (off: the machine
#                      keeps the metadata-only promise until you choose)
#   --keep-legacy      leave the ~/.vibemon/notify.sh hook entries in place
#   --purge-legacy     delete ~/.vibemon once nothing references it
#   --dry-run          print the commands instead of running them
set -eu

SERVER="${VIBEMON_SYNC_URL:-https://sync.vibemon.dev}"
WEB="${VIBEMON_WEB_URL:-https://vibemon.dev}"
TOKEN=""
LEGACY_KEY=""
PROFILE="semantic"
NEW_DB_MODE="metadata_only"
KEEP_LEGACY=0
PURGE_LEGACY=0
DRY_RUN=0
# The AttemptDB release this script was written against, pinned: the
# binary installer comes from the same tag, so the two always agree, and a
# machine gets the version the product tested rather than whatever is
# newest. `--pair` needs 0.2.0 or later. A newer `attempt` already on the
# machine is kept.
ATTEMPTDB_VERSION="${ATTEMPTDB_VERSION:-0.2.3}"
ATTEMPTDB_INSTALLER="${ATTEMPTDB_INSTALLER:-https://raw.githubusercontent.com/nullarch/attemptdb/v${ATTEMPTDB_VERSION}/install.sh}"
export ATTEMPTDB_VERSION

while [ $# -gt 0 ]; do
    case "$1" in
        pair_*) TOKEN="$1"; shift ;;
        # The legacy command (`… | bash -s vbm_…`, still on /setup and in the
        # app's wizard) carries the account's API key: exchanged for a
        # pairing token at the web, server side, before anything changes.
        vbm_*) LEGACY_KEY="$1"; shift ;;
        --pair) TOKEN="$2"; shift 2 ;;
        --pair=*) TOKEN="${1#--pair=}"; shift ;;
        --web) WEB="$2"; shift 2 ;;
        --web=*) WEB="${1#--web=}"; shift ;;
        --server) SERVER="$2"; shift 2 ;;
        --server=*) SERVER="${1#--server=}"; shift ;;
        --profile) PROFILE="$2"; shift 2 ;;
        --profile=*) PROFILE="${1#--profile=}"; shift ;;
        --local-content) NEW_DB_MODE="local_semantic"; shift ;;
        --keep-legacy) KEEP_LEGACY=1; shift ;;
        --purge-legacy) PURGE_LEGACY=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) sed -n '2,45p' "$0"; exit 0 ;;
        # Anything else is not ours to act on.
        *) printf 'vibemon: unknown argument %s (expected a pair_… token)\n' "$1" >&2; exit 2 ;;
    esac
done
SERVER="${SERVER%/}"

say() { printf '%s\n' "$*"; }
fail() { printf 'vibemon: %s\n' "$*" >&2; exit 1; }
run() {
    if [ "$DRY_RUN" -eq 1 ]; then say "+ $*"; else "$@"; fi
}
case "$PROFILE" in
    metadata_only|semantic|full) ;;
    *) fail "unknown --profile $PROFILE (metadata_only | semantic | full)" ;;
esac

BIN_DIR="${ATTEMPTDB_BIN_DIR:-$HOME/.local/bin}"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) PATH="$BIN_DIR:$PATH"; export PATH ;; esac

connected=0
if command -v attempt >/dev/null 2>&1 \
   && attempt sync status --json 2>/dev/null | grep -q '"connected": *true'; then
    connected=1
fi

# 0a. A person at a terminal, no argument, a legacy install on this machine:
#     the app's "update available" command is exactly `curl … | bash`, and
#     the older client kept the account key in ~/.vibemon/api-key. Use it —
#     but only when someone is watching: the legacy client's daily poll
#     runs this same command detached with every stream on /dev/null, and
#     that run must keep changing nothing (checked by `-t 2`).
if [ -z "$TOKEN" ] && [ -z "$LEGACY_KEY" ] && [ "$connected" -eq 0 ] \
   && [ -t 2 ] && [ -r "$HOME/.vibemon/api-key" ]; then
    stored="$(grep -o 'vbm_[A-Za-z0-9_-]*' "$HOME/.vibemon/api-key" 2>/dev/null | head -n 1 || true)"
    if [ -n "$stored" ]; then
        say "vibemon: found the account key of the older client in ~/.vibemon/api-key; upgrading this machine to AttemptDB"
        LEGACY_KEY="$stored"
    fi
fi

# 0. A legacy API key becomes a pairing token at the web (server side; the
#    key is looked up there and goes nowhere else). Before anything changes.
if [ -n "$LEGACY_KEY" ] && [ -z "$TOKEN" ]; then
    if [ "$DRY_RUN" -eq 1 ]; then
        say "+ curl -fsS -X POST $WEB/api/attemptdb/pair  (vbm_… → pair_…)"
        TOKEN="pair_dryrun"
    else
        resp="$(curl -sS -X POST -H 'Content-Type: application/json' \
            --data "{\"api_key\":\"$LEGACY_KEY\"}" "$WEB/api/attemptdb/pair" 2>/dev/null || true)"
        TOKEN="$(printf '%s' "$resp" | sed -n 's/.*"token": *"\(pair_[A-Za-z0-9_-]*\)".*/\1/p')"
        if [ -z "$TOKEN" ]; then
            reason="$(printf '%s' "$resp" | sed -n 's/.*"error": *"\([^"]*\)".*/\1/p')"
            fail "the web did not accept this API key: ${reason:-no usable answer from $WEB} (nothing changed; get a command at $WEB/devices)"
        fi
        web_server="$(printf '%s' "$resp" | sed -n 's/.*"sync_url": *"\([^"]*\)".*/\1/p')"
        # The web knows where its sync server is; a --server flag still wins.
        if [ -n "$web_server" ] && [ -z "${VIBEMON_SYNC_URL:-}" ] && [ "$SERVER" = "https://sync.vibemon.dev" ]; then
            SERVER="${web_server%/}"
        fi
    fi
fi

# 1. The gate. No token and never connected: this is not an install, it is
#    the legacy client polling for updates. Do nothing, say so, exit 0.
if [ -z "$TOKEN" ] && [ "$connected" -eq 0 ]; then
    say "vibemon: no pairing token given and this machine is not connected; nothing changed."
    say "         get a one-line command at https://vibemon.dev/devices"
    exit 0
fi
if [ -n "$TOKEN" ]; then
    case "$TOKEN" in pair_*) ;; *) fail "$TOKEN is not a pairing token (pair_…)" ;; esac
    if [ "$DRY_RUN" -eq 1 ]; then
        say "+ curl -fsS $SERVER/v1/pair/$TOKEN"
    else
        code="$(curl -sS -o /dev/null -w '%{http_code}' "$SERVER/v1/pair/$TOKEN" || echo 000)"
        case "$code" in
            200) ;;
            410) fail "the pairing token has expired or was already used; get a new one at https://vibemon.dev/devices" ;;
            404) fail "the server does not know this pairing token; get a new one at https://vibemon.dev/devices" ;;
            000) fail "cannot reach $SERVER; check the network and try again (nothing changed)" ;;
            *)   fail "the server answered $code to the pairing check (nothing changed)" ;;
        esac
    fi
fi

# 2. The binary, verified by the release's checksums (the AttemptDB
#    installer refuses an unverifiable download). Skipped when the machine
#    already has the pinned version or a newer one.
# older_than A B: true when version A sorts before version B (x.y.z).
older_than() {
    a1=${1%%.*}; r=${1#*.}; a2=${r%%.*}; a3=${r#*.}
    b1=${2%%.*}; r=${2#*.}; b2=${r%%.*}; b3=${r#*.}
    [ "$a1" -lt "$b1" ] || { [ "$a1" -eq "$b1" ] && [ "$a2" -lt "$b2" ]; } \
        || { [ "$a1" -eq "$b1" ] && [ "$a2" -eq "$b2" ] && [ "$a3" -lt "$b3" ]; }
}
present="$(attempt --version 2>/dev/null | awk '{print $2}')"
case "$present" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) present="" ;;
esac
if [ -n "$present" ] && ! older_than "$present" "$ATTEMPTDB_VERSION"; then
    say "attempt $present present (need $ATTEMPTDB_VERSION or newer); keeping it"
elif [ "$DRY_RUN" -eq 1 ]; then
    say "+ ATTEMPTDB_VERSION=$ATTEMPTDB_VERSION curl -fsSL $ATTEMPTDB_INSTALLER | sh"
else
    [ -n "$present" ] && say "attempt $present present; installing $ATTEMPTDB_VERSION"
    curl -fsSL "$ATTEMPTDB_INSTALLER" | sh
    command -v attempt >/dev/null 2>&1 || fail "attempt is not on PATH after install; add $BIN_DIR to PATH and re-run"
fi

# 3. The local database. Created metadata-only unless --local-content; an
#    existing database is left exactly as it is (mode, settings, data).
if [ "$DRY_RUN" -eq 0 ] && attempt status >/dev/null 2>&1; then
    run attempt init --source vibemon
else
    run attempt init --capture-mode "$NEW_DB_MODE" --source vibemon
fi

# 4. Pairing. The token and this database's device id become a device key;
#    `attempt sync connect` proves the key with an authenticated handshake
#    and saves it only then. A failure here leaves hooks and legacy alone.
if [ -n "$TOKEN" ]; then
    run attempt sync connect "$SERVER" --pair "$TOKEN" --profile "$PROFILE" \
        --label "$(hostname 2>/dev/null || echo device)" \
        || fail "pairing failed; nothing else was changed — fix the cause and run the command again with a fresh token"
fi

# 5. Hooks, next to whatever is there. The legacy client keeps running
#    until step 8 confirms the new path works.
run attempt hook install

# 6. The daemon: hooks hand events to it, it imports the spool and uploads
#    every few seconds. Re-running re-registers.
run attempt daemon install

# 7. One upload now; the server must accept it before anything is removed.
if [ "$DRY_RUN" -eq 1 ]; then
    say "+ attempt sync now"
elif ! attempt sync now; then
    say "" >&2
    say "vibemon: the first upload did not go through. AttemptDB is installed and hooks are in place," >&2
    say "         but the legacy VibeMon hooks were left untouched so collection continues as before." >&2
    say "         Run 'attempt sync status' for the error, then 'attempt sync now'; once it succeeds," >&2
    say "         re-run this command to finish the switch." >&2
    exit 1
fi

# 8. The legacy client's hook entries — only now, only if asked to keep
#    them is refused.
if [ "$KEEP_LEGACY" -eq 0 ]; then
    run attempt hook install --remove-legacy vibemon
fi
if [ "$PURGE_LEGACY" -eq 1 ] && [ -d "$HOME/.vibemon" ]; then
    still=""
    for f in "$HOME/.claude/settings.json" "$HOME/.codex/hooks.json" \
             "$HOME/.cursor/hooks.json" "$HOME/.gemini/settings.json"; do
        [ -f "$f" ] && grep -q '\.vibemon/notify\.' "$f" && still="$still $f"
    done
    if [ -n "$still" ]; then
        say "keeping ~/.vibemon: still referenced by$still" >&2
    else
        run rm -rf "$HOME/.vibemon"
    fi
elif [ -d "$HOME/.vibemon" ] && [ "$KEEP_LEGACY" -eq 0 ]; then
    say "legacy client left at ~/.vibemon (no hook references it any more); remove it with: rm -rf ~/.vibemon"
fi

# 9. What the user sees. Codex users get told here if /hooks approval is
#    still pending; nothing else needs a command from them.
say ""
run attempt doctor
say ""
say "done. https://vibemon.dev/devices shows this device; 'attempt sync status' shows what left this machine."

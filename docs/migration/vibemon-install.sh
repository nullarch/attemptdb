#!/bin/sh
# vibemon.dev's one-line install: AttemptDB on this machine, linked to the
# VibeMon sync server with a one-time pairing token from the web.
#
#   curl -fsSL https://vibemon.dev/install.sh | sh -s -- pair_abc123
#
# What it does, in this order — and the order is the safety:
#
#   0. selects a background runtime; Windows shells
#      hand off to the native PowerShell installer before pairing
#   1. checks the pairing token with the server before touching anything;
#      no token (or a dead one) → nothing on this machine changes
#   2. installs (or upgrades) the `attempt` binary, verified against the
#      release's SHA256SUMS
#   3. creates the local database if there is none — an existing one keeps
#      its capture mode and settings
#   4. pairs: the token plus the database's own device id become a device
#      key, proven by an authenticated handshake, saved only on success
#   5. installs the agent hooks next to any existing ones
#   6. registers the background daemon (launchd / systemd --user), or
#      starts a Linux session supervisor when no user service is available
#   7. uploads once and requires the server to accept it
#   8. only then removes the legacy VibeMon hooks (~/.vibemon/notify.sh)
#   9. shows `attempt doctor`
#
# Run again any time: it upgrades, repairs hooks, re-registers the daemon,
# and keeps the existing connection when no token is given. Without a token
# on a machine that was never connected it exits 0 and changes nothing —
# that is the path the legacy client's daily auto-update takes (detached,
# no terminal). On a machine that still has the older client's stored
# account key (~/.vibemon/api-key) it uses that key to pair — typed by a
# person (the app's "update available" command is exactly that) or run
# unattended by the older client's poll. Whether that poll runs this at all
# is the web's decision (`install.sh?v`), not this script's.
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
#   --no-report        do not tell vibemon.dev how this run ended. By default
#                      one line goes back when the script exits — ok or
#                      failed, the step it stopped at, OS, versions, the
#                      account key if it was used (resolved to the account
#                      on the web, never stored), and, for an unattended
#                      run, the last 40 lines of its log with home paths and
#                      keys blanked — so a failure on a machine nobody is
#                      watching is still a failure somebody can read.
#                      Unattended runs log to ~/.vibemon/vibemon-install.log
#                      (or ~/.local/state/attemptdb/); a person at a terminal
#                      sees the output instead.
#   --no-commit-msg    the older client's flag; accepted and ignored
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
REPORT=1
STEP="start"
UNATTENDED=0
LAST_ERROR=""
AUTO_MIGRATE=0
RUNTIME=service
INSTALL_TMP=""
# The installer hotfix and the binary have independent immutable pins.
# 0.2.10 configures local OTel collection with the agent hooks.
# A newer `attempt` already on the machine is kept.
ATTEMPTDB_VERSION="${ATTEMPTDB_VERSION:-0.2.10}"
INSTALLER_VERSION="0.2.10+install.1"
INSTALLER_REF="v0.2.10"
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
        --no-report) REPORT=0; shift ;;
        # The older client's command carried these; nothing here reads them.
        --no-commit-msg|--commit-msg) shift ;;
        -h|--help) sed -n '2,45p' "$0"; exit 0 ;;
        # Anything else is not ours to act on.
        *) printf '%s\n' 'vibemon: invalid installation argument; copy a complete command from https://vibemon.dev/devices (nothing paired)' >&2; exit 2 ;;
    esac
done
SERVER="${SERVER%/}"
[ -t 2 ] || UNATTENDED=1

# The run log. Unattended, every stream is /dev/null — the older client's
# poll runs it that way — so the output goes to a file, and the report
# carries its tail. Attended, the person at the terminal is the log.
LOG=""
if [ "$UNATTENDED" -eq 1 ]; then
    # Next to the older client when there is one (never created for it —
    # that directory means "the legacy client is here"), else our state dir.
    if [ -d "$HOME/.vibemon" ] && [ -w "$HOME/.vibemon" ]; then
        LOG="$HOME/.vibemon/vibemon-install.log"
    else
        d="${XDG_STATE_HOME:-$HOME/.local/state}/attemptdb"
        if mkdir -p "$d" 2>/dev/null && [ -w "$d" ]; then LOG="$d/vibemon-install.log"; fi
    fi
    if [ -n "$LOG" ] && printf '\n=== %s vibemon-install (attempt %s) ===\n' \
           "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo now)" "$ATTEMPTDB_VERSION" >>"$LOG" 2>/dev/null; then
        exec >>"$LOG" 2>&1
    else
        LOG=""
    fi
fi

say() { printf '%s\n' "$*"; }
fail() { LAST_ERROR="$*"; printf 'vibemon: %s\n' "$*" >&2; exit 1; }
run() {
    if [ "$DRY_RUN" -eq 1 ]; then say "+ $*"; else "$@"; fi
}
case "$PROFILE" in
    metadata_only|semantic|full) ;;
    *) fail "unknown --profile $PROFILE (metadata_only | semantic | full)" ;;
esac

# One line back to the web when this script exits, however it exits (see
# --no-report). Best effort: five seconds, never a failure of its own.
report() {
    code="$1"
    [ "$REPORT" -eq 1 ] && [ "$DRY_RUN" -eq 0 ] || return 0
    ok=false; [ "$code" -eq 0 ] && ok=true
    unattended=false; [ "$UNATTENDED" -eq 1 ] && unattended=true
    os="$(uname -s 2>/dev/null || echo unknown)"
    arch="$(uname -m 2>/dev/null || echo unknown)"
    av="$(attempt --version 2>/dev/null | sed -n 's/^attempt //p' | head -n 1)"
    if [ "$code" -ne 0 ] && [ -z "$LAST_ERROR" ]; then
        LAST_ERROR="command failed during $STEP (exit $code); see the install log"
    fi
    err="$(printf '%s' "$LAST_ERROR" | head -n 1 | tr -d '"\\' | tr '\t\r' '  ' \
        | sed -E 's#(vbm|pair|atk)_[A-Za-z0-9_-]+#\1_[redacted]#g; s#/(Users|home|private|tmp|var|root|opt|mnt)/[^[:space:]"]*#[path]#g' | cut -c1-300)"
    # The log's tail, made safe for a report: keys and tokens blanked, home
    # and temp paths blanked, JSON-escaped, at most ~4 KB.
    tail_json=""
    if [ -n "$LOG" ] && [ -r "$LOG" ]; then
        tail_json="$(tail -n 40 "$LOG" 2>/dev/null | tr -d '\r' \
            | sed -E 's#(vbm|pair|atk)_[A-Za-z0-9_-]+#\1_…#g; s#/(Users|home|private|tmp|var|root|opt|mnt)/[^[:space:]"]*#…#g' \
            | cut -c1-120 | head -c 3000 \
            | awk 'BEGIN{ORS="\\n"} {gsub(/\\/,"\\\\"); gsub(/"/,"\\\""); gsub(/\t/,"  "); print}')"
    fi
    body="$(printf '{"ok":%s,"step":"%s","os":"%s","arch":"%s","installer_version":"%s","attempt_version":"%s","unattended":%s,"error":"%s","api_key":"%s","log_tail":"%s"}' \
        "$ok" "$STEP" "$os" "$arch" "$INSTALLER_VERSION" "$av" "$unattended" "$err" "$LEGACY_KEY" "$tail_json")"
    curl -fsS --max-time 5 -o /dev/null -X POST -H 'Content-Type: application/json' \
        --data "$body" "$WEB/api/attemptdb/install-report" >/dev/null 2>&1 || true
}
cleanup() { [ -z "$INSTALL_TMP" ] || rm -rf "$INSTALL_TMP"; }
trap 'code=$?; report "$code"; cleanup' EXIT

BIN_DIR="${ATTEMPTDB_BIN_DIR:-$HOME/.local/bin}"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) PATH="$BIN_DIR:$PATH"; export PATH ;; esac

connected=0
if command -v attempt >/dev/null 2>&1 \
   && attempt sync status --json 2>/dev/null | grep -q '"connected": *true'; then
    connected=1
fi

# 0a. No argument, a legacy install on this machine: the app's "update
#     available" command is exactly `curl … | bash`, and the older client
#     kept the account key in ~/.vibemon/api-key. Use it. This is also the
#     path the older client's daily poll takes when the web tells it to
#     (`install.sh?v` changed) — detached, every stream on /dev/null — and
#     the same safety applies: nothing is removed until an upload succeeded,
#     and a failure is reported (see --no-report).
STEP=pair
if [ -z "$TOKEN" ] && [ -z "$LEGACY_KEY" ] && [ "$connected" -eq 0 ] \
   && [ -r "$HOME/.vibemon/api-key" ]; then
    stored="$(grep -o 'vbm_[A-Za-z0-9_-]*' "$HOME/.vibemon/api-key" 2>/dev/null | head -n 1 || true)"
    if [ -n "$stored" ]; then
        say "vibemon: found the account key of the older client in ~/.vibemon/api-key; upgrading this machine to AttemptDB"
        LEGACY_KEY="$stored"
        [ "$UNATTENDED" -eq 0 ] || AUTO_MIGRATE=1
    fi
fi

# Reject foreign credentials without including them in logs or sending them
# to the sync server, even when this machine cannot run a background service.
case "$TOKEN" in
    ""|pair_*) ;;
    *) fail "invalid pairing token; copy a new installation command from https://vibemon.dev/devices (nothing paired)" ;;
esac

# Git Bash/Cygwin are Windows, not Linux. Hand off before minting or
# consuming a pairing token, preserving arguments as argv (never eval).
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        STEP=platform
        command -v powershell.exe >/dev/null 2>&1 || fail "Windows requires powershell.exe; run the PowerShell command at $WEB/devices"
        command -v cygpath >/dev/null 2>&1 || fail "cannot convert the Windows installer path; run the PowerShell command at $WEB/devices"
        [ "$PURGE_LEGACY" -eq 0 ] || fail "--purge-legacy is not supported by the Windows installer; re-run without it"
        if [ "$DRY_RUN" -eq 1 ]; then
            say "+ hand off to the native Windows PowerShell installer (arguments preserved)"
            exit 0
        fi
        INSTALL_TMP="$(mktemp -d)"
        curl -fsSL --max-time 60 "${VIBEMON_WINDOWS_INSTALLER_URL:-https://raw.githubusercontent.com/nullarch/attemptdb/${INSTALLER_REF}/docs/migration/vibemon-install.ps1}" \
            -o "$INSTALL_TMP/install.ps1" || fail "could not download the Windows installer"
        set -- -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$INSTALL_TMP/install.ps1")" -Web "$WEB" -Server "$SERVER" -Profile "$PROFILE"
        [ -z "$TOKEN" ] || set -- "$@" -Pair "$TOKEN"
        [ -z "$LEGACY_KEY" ] || set -- "$@" -ApiKey "$LEGACY_KEY"
        [ "$NEW_DB_MODE" != local_semantic ] || set -- "$@" -LocalContent
        [ "$KEEP_LEGACY" -eq 0 ] || set -- "$@" -KeepLegacy
        [ "$REPORT" -eq 1 ] || set -- "$@" -NoReport
        REPORT=0 # The native installer owns the single outcome report.
        ATTEMPTDB_INSTALLER="${ATTEMPTDB_WINDOWS_INSTALLER:-https://raw.githubusercontent.com/nullarch/attemptdb/v${ATTEMPTDB_VERSION}/install.ps1}" \
            powershell.exe "$@"
        exit $?
        ;;
esac

# No credentials means no installation, including no service requirement.
if [ -z "$TOKEN" ] && [ -z "$LEGACY_KEY" ] && [ "$connected" -eq 0 ]; then
    STEP=noop
    say "vibemon: no pairing token given and this machine is not connected; nothing changed."
    say "         get a one-line command at https://vibemon.dev/devices"
    exit 0
fi

# A Linux environment can run the existing daemon without systemd. Use an
# isolated session supervisor when the user manager is unavailable. It lasts
# only as long as this environment; it does not promise reboot activation.
# Missing runtime tools still stop an explicit install before any pairing.
STEP=environment
service_error=""
if [ "$DRY_RUN" -eq 0 ]; then
    case "$(uname -s)" in
        Linux)
            if ! command -v systemctl >/dev/null 2>&1 || ! systemctl --user show-environment >/dev/null 2>&1; then
                RUNTIME=session
                for tool in nohup setsid flock; do
                    command -v "$tool" >/dev/null 2>&1 || service_error="Linux session sync requires nohup, setsid and flock; missing $tool"
                done
                if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
                    service_error="Linux session sync requires sha256sum or shasum"
                fi
                say "vibemon: systemd user service unavailable; selecting Linux session sync"
            fi
            ;;
        Darwin)
            if ! launchctl print "gui/$(id -u)" >/dev/null 2>&1; then
                service_error="no logged-in macOS GUI service domain; run the installer from your desktop session"
            fi
            ;;
        *) service_error="unsupported operating system" ;;
    esac
fi
if [ -n "$service_error" ]; then
    if [ "$AUTO_MIGRATE" -eq 1 ]; then
        STEP=skipped_environment
        LAST_ERROR="$service_error; automatic migration skipped, legacy hooks unchanged"
        say "vibemon: $LAST_ERROR"
        exit 0
    fi
    fail "$service_error; nothing paired and legacy hooks unchanged"
fi

# 0. A legacy API key becomes a pairing token at the web (server side; the
#    key is looked up there and goes nowhere else). Before anything changes.
if [ -n "$LEGACY_KEY" ] && [ -z "$TOKEN" ] && [ "$connected" -eq 0 ]; then
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
    STEP=noop
    say "vibemon: no pairing token given and this machine is not connected; nothing changed."
    say "         get a one-line command at https://vibemon.dev/devices"
    exit 0
fi
if [ -n "$TOKEN" ]; then
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

STEP=binary
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
    INSTALL_TMP="$(mktemp -d)"
    curl -fsSL --max-time 60 "$ATTEMPTDB_INSTALLER" -o "$INSTALL_TMP/install.sh" || fail "could not download the binary installer"
    sh "$INSTALL_TMP/install.sh" || fail "the binary installer failed"
    command -v attempt >/dev/null 2>&1 || fail "attempt is not on PATH after install; add $BIN_DIR to PATH and re-run"
fi

# Launch outside the installing shell's session and keep only one supervisor
# for this daemon endpoint. The daemon itself still owns the database lock.
# A clean `attempt daemon stop` ends the supervisor; a crash is retried with
# bounded backoff. No subprocess or network work is added to the hook path.
start_session_runtime() {
    if attempt daemon status >/dev/null 2>&1; then
        say "vibemon: capture daemon already responds; keeping the current runtime"
        return 0
    fi
    endpoint="$(attempt daemon status --json 2>/dev/null | sed -n '/"endpoint":/p' || true)"
    [ -n "$endpoint" ] || fail "cannot identify the local daemon endpoint; legacy hooks unchanged"
    if command -v sha256sum >/dev/null 2>&1; then
        scope="$(printf '%s' "$endpoint" | sha256sum | awk '{print $1}')"
    else
        scope="$(printf '%s' "$endpoint" | shasum -a 256 | awk '{print $1}')"
    fi
    session_dir="${XDG_STATE_HOME:-$HOME/.local/state}/attemptdb/session"
    # State is private and independent of an ephemeral or missing user bus.
    (umask 077; mkdir -p "$session_dir") || fail "cannot create the session runtime directory"
    session_log="$session_dir/$scope.log"
    binary="$(command -v attempt)"
    (umask 077
        nohup setsid sh -c '
            binary=$1; lock=$2
            exec 9>"$lock" || exit 1
            flock -n 9 || exit 0
            delay=1; failures=0
            while :; do
                "$binary" daemon status >/dev/null 2>&1 && exit 0
                started=$(date +%s)
                "$binary" daemon run --foreground 9>&-
                code=$?
                [ "$code" -ne 0 ] || exit 0
                now=$(date +%s)
                if [ $((now - started)) -ge 60 ]; then
                    delay=1; failures=0
                fi
                failures=$((failures + 1))
                if [ "$failures" -ge 8 ]; then
                    printf "attemptdb session: repeated startup failures; re-run the installer after checking the log\n"
                    exit "$code"
                fi
                printf "attemptdb session: daemon exited %s; retrying in %ss\n" "$code" "$delay"
                sleep "$delay"
                [ "$delay" -ge 30 ] || delay=$((delay * 2))
                [ "$delay" -le 30 ] || delay=30
            done
        ' attemptdb-session "$binary" "$session_dir/$scope.lock" \
            </dev/null >>"$session_log" 2>&1 &
    )
    tries=0
    while [ "$tries" -lt 15 ]; do
        if attempt daemon status >/dev/null 2>&1; then
            say "vibemon: Linux session sync is running; log: $session_log"
            return 0
        fi
        tries=$((tries + 1))
        sleep 1
    done
    tail -n 10 "$session_log" 2>/dev/null || true
    fail "Linux session daemon did not become ready; legacy hooks unchanged; check the session runtime log"
}

STEP=init
# 3. The local database. Created metadata-only unless --local-content; an
#    existing database is left exactly as it is (mode, settings, data).
if [ "$DRY_RUN" -eq 0 ] && attempt status >/dev/null 2>&1; then
    run attempt init --source vibemon
else
    run attempt init --capture-mode "$NEW_DB_MODE" --source vibemon
fi

# Prove that the fallback can actually start before consuming a pairing
# token or changing hooks. Tool availability alone is not runtime health.
if [ "$RUNTIME" = session ]; then
    STEP=daemon
    start_session_runtime
fi

STEP=connect
# 4. Pairing. The token and this database's device id become a device key;
#    `attempt sync connect` proves the key with an authenticated handshake
#    and saves it only then. A failure here leaves hooks and legacy alone.
if [ -n "$TOKEN" ]; then
    run attempt sync connect "$SERVER" --pair "$TOKEN" --profile "$PROFILE" \
        --label "$(hostname 2>/dev/null || echo device)" \
        || fail "pairing failed; nothing else was changed — fix the cause and run the command again with a fresh token"
fi

STEP=hooks
# 5. Hooks, next to whatever is there. The legacy client keeps running
#    until step 8 confirms the new path works.
run attempt hook install || fail "hook installation failed; legacy hooks unchanged"

STEP=daemon
# 6. The daemon: hooks hand events to it, it imports the spool and uploads
#    every few seconds. Re-running re-registers.
if [ "$RUNTIME" = session ]; then
    attempt daemon status >/dev/null 2>&1 || fail "Linux session daemon stopped; legacy hooks unchanged"
else
    run attempt daemon install || fail "background service registration failed; legacy hooks unchanged (see the install log)"
fi

STEP=upload
# 7. One upload now; the server must accept it before anything is removed.
if [ "$DRY_RUN" -eq 1 ]; then
    say "+ attempt sync now"
elif ! attempt sync now; then
    say "" >&2
    say "vibemon: the first upload did not go through. AttemptDB is installed and hooks are in place," >&2
    say "         but the legacy VibeMon hooks were left untouched so collection continues as before." >&2
    say "         Run 'attempt sync status' for the error, then 'attempt sync now'; once it succeeds," >&2
    say "         re-run this command to finish the switch." >&2
    LAST_ERROR="the first upload did not go through"
    exit 1
fi

if [ "$RUNTIME" = session ]; then
    attempt daemon status >/dev/null 2>&1 || fail "Linux session daemon stopped after upload; legacy hooks unchanged"
fi

STEP=remove_legacy
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

STEP=done
# 9. What the user sees. Codex users get told here if /hooks approval is
#    still pending; nothing else needs a command from them.
say ""
# doctor's exit code grades the machine (an untrusted Codex hook is a 1);
# it is not this script's verdict, which was settled by the upload above.
run attempt doctor || true
say ""
say "done. https://vibemon.dev/devices shows this device; 'attempt sync status' shows what left this machine."
if [ "$RUNTIME" = session ]; then
    say "Linux session sync runs while this environment is alive; it is not a login or reboot service."
    say "After the environment restarts, re-run this installer without a token to resume the saved connection."
    say "Stop this runtime with 'attempt daemon stop'. Keep the data directory if the environment is recreated."
fi

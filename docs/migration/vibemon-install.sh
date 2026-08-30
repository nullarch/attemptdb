#!/bin/sh
# Draft of the next `vibemon.dev/install.sh`: installs AttemptDB, replaces the
# VibeMon legacy hooks (`~/.vibemon/notify.sh`) with `attempt hook`, and links
# this device to the hosted VibeMon sync server.
#
#   curl -fsSL https://vibemon.dev/install.sh | sh -s -- --key atk_...
#
# Options
#   --key KEY         device key issued by VibeMon (required on first run)
#   --server URL      sync server (default: $VIBEMON_SYNC_URL or https://sync.vibemon.dev)
#   --send-content    also upload prompts / tool output (default: metadata only)
#   --keep-legacy     leave the ~/.vibemon/notify.sh entries in place
#   --purge-legacy    delete ~/.vibemon after every agent config is migrated
#   --dry-run         print the commands instead of running them
#
# Everything here is idempotent: re-running upgrades the binary, repairs the
# hook entries, and re-uses the existing connection when no --key is given.
# Nothing in this script reads agent content; the local database on this
# machine stays the source of truth (`attempt sync status` shows what left).
set -eu

SERVER="${VIBEMON_SYNC_URL:-https://sync.vibemon.dev}"
KEY=""
SEND_CONTENT=0
KEEP_LEGACY=0
PURGE_LEGACY=0
DRY_RUN=0
ATTEMPTDB_INSTALLER="https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh"

while [ $# -gt 0 ]; do
    case "$1" in
        --key) KEY="$2"; shift 2 ;;
        --key=*) KEY="${1#--key=}"; shift ;;
        --server) SERVER="$2"; shift 2 ;;
        --server=*) SERVER="${1#--server=}"; shift ;;
        --send-content) SEND_CONTENT=1; shift ;;
        --keep-legacy) KEEP_LEGACY=1; shift ;;
        --purge-legacy) PURGE_LEGACY=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

say() { printf '%s\n' "$*"; }
run() {
    if [ "$DRY_RUN" -eq 1 ]; then
        say "+ $*"
    else
        "$@"
    fi
}

# 1. The binary. `attempt` lives in ~/.local/bin (ATTEMPTDB_BIN_DIR overrides).
BIN_DIR="${ATTEMPTDB_BIN_DIR:-$HOME/.local/bin}"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) PATH="$BIN_DIR:$PATH"; export PATH ;; esac
if command -v attempt >/dev/null 2>&1 && [ "$DRY_RUN" -eq 0 ]; then
    say "attempt $(attempt --version 2>/dev/null | awk '{print $2}') already installed; checking for a newer release"
fi
if [ "$DRY_RUN" -eq 1 ]; then
    say "+ curl -fsSL $ATTEMPTDB_INSTALLER | sh"
else
    curl -fsSL "$ATTEMPTDB_INSTALLER" | sh
fi
command -v attempt >/dev/null 2>&1 || [ "$DRY_RUN" -eq 1 ] || {
    say "attempt is not on PATH after install; add $BIN_DIR to PATH and re-run" >&2
    exit 1
}

# 2. The local database (no-op when it exists).
run attempt init

# 3. Hooks: install ours and, unless asked otherwise, remove the legacy
#    notify.sh entries so the two collectors never run side by side.
if [ "$KEEP_LEGACY" -eq 1 ]; then
    run attempt hook install
else
    run attempt hook install --remove-legacy vibemon
fi

# 4. Link the device. A key is needed once; later runs keep the connection.
connected=0
if [ "$DRY_RUN" -eq 0 ] && attempt sync status --json 2>/dev/null | grep -q '"connected": *true'; then
    connected=1
fi
if [ -n "$KEY" ]; then
    if [ "$SEND_CONTENT" -eq 1 ]; then
        run attempt sync connect "$SERVER" --key "$KEY" --send-content
    else
        run attempt sync connect "$SERVER" --key "$KEY"
    fi
elif [ "$connected" -eq 0 ] && [ "$DRY_RUN" -eq 0 ]; then
    say "no --key given and this device is not linked yet; get a key at https://vibemon.dev/devices and re-run with --key" >&2
    exit 1
fi
run attempt sync now

# 5. The legacy client. Only removed on request, and only when no agent
#    config still references it (otherwise those hooks would start failing).
if [ "$PURGE_LEGACY" -eq 1 ] && [ -d "$HOME/.vibemon" ]; then
    still=""
    for f in "$HOME/.claude/settings.json" "$HOME/.codex/hooks.json" \
             "$HOME/.cursor/hooks.json" "$HOME/.gemini/settings.json"; do
        [ -f "$f" ] && grep -q '\.vibemon/notify\.sh' "$f" && still="$still $f"
    done
    if [ -n "$still" ]; then
        say "keeping ~/.vibemon: still referenced by$still" >&2
    else
        run rm -rf "$HOME/.vibemon"
    fi
elif [ -d "$HOME/.vibemon" ] && [ "$KEEP_LEGACY" -eq 0 ]; then
    say "legacy client left at ~/.vibemon (not referenced by the hooks any more); remove it with: rm -rf ~/.vibemon"
fi

say ""
say "done. Open https://vibemon.dev to see this device; run 'attempt sync status' any time to see what has been uploaded."

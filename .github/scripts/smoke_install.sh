#!/bin/sh
# Smoke test of a PUBLISHED release through the one-line installer, as a user
# would run it. Used by the `smoke-install` job of release.yml, after `publish`.
#
#   .github/scripts/smoke_install.sh v0.2.14
#
# It runs only on a disposable machine (a CI runner, or SMOKE_INSTALL_DISPOSABLE=1
# on a throwaway container or VM). HOME and the data directory are temporary,
# but `attempt uninstall` stops the *current user's* background service by its
# fixed name (launchctl bootout / systemctl --user disable --now) whatever HOME
# says, which on a developer machine would stop the real AttemptDB daemon.
#
# What it asserts, in order:
#   1. the installer script at the tag installs the release it names, with
#      `--yes` and no terminal, and edits no shell profile;
#   2. `attempt --version` is that release;
#   3. a second `attempt setup --dry-run --json` has nothing left to change;
#   4. the user's own agent settings survived, next to ours;
#   5. `attempt uninstall` succeeds and leaves nothing of ours in them.
#
# Environment:
#   GITHUB_REPOSITORY    owner/name (default nullarch/attemptdb)
#   SMOKE_INSTALL_DISPOSABLE=1
#                        you are on a throwaway machine (see above)
#   SMOKE_INSTALL_URL    the installer to run (default: install.sh at the tag,
#                        from raw.githubusercontent.com)
set -eu

TAG="${1:?usage: smoke_install.sh vX.Y.Z}"
REPO="${GITHUB_REPOSITORY:-nullarch/attemptdb}"
VERSION="${TAG#v}"
INSTALL_URL="${SMOKE_INSTALL_URL:-https://raw.githubusercontent.com/$REPO/$TAG/install.sh}"

fail() { printf 'smoke-install: FAIL: %s\n' "$*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

[ "${GITHUB_ACTIONS:-}" = "true" ] || [ "${SMOKE_INSTALL_DISPOSABLE:-}" = "1" ] \
  || fail "refusing to run outside CI: attempt uninstall would stop this user's real background service (set SMOKE_INSTALL_DISPOSABLE=1 on a throwaway machine)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A machine of its own. The agents' own configuration variables are removed so
# nothing here can reach a real configuration; HOME, data and the keyring are
# temporary (the key store would otherwise be the runner's keychain).
unset CLAUDE_CONFIG_DIR CODEX_HOME CURSOR_CONFIG_DIR GEMINI_CONFIG_DIR
unset ATTEMPTDB_NO_SETUP ATTEMPTDB_ASSUME_YES ATTEMPTDB_MODIFY_PATH ATTEMPTDB_NO_MODIFY_PATH ATTEMPTDB_BIN_DIR
HOME="$work/home"
export HOME
mkdir -p "$HOME/.claude"
export ATTEMPTDB_DATA_DIR="$work/data"
export ATTEMPTDB_KEYRING=off
# A runner has no login session to register a background service in.
export ATTEMPTDB_NO_DAEMON=1
export ATTEMPTDB_NO_AUTO_UPDATE=1

settings="$HOME/.claude/settings.json"
printf '%s\n' '{"permissions":{"allow":["Bash(ls:*)"]}}' > "$settings"

step "install $TAG with the published installer (--yes, no terminal)"
printf 'installer: %s\n' "$INSTALL_URL"
curl -fsSL "$INSTALL_URL" | ATTEMPTDB_VERSION="$VERSION" sh -s -- --yes \
  || fail "the installer exited non-zero"

BIN="$HOME/.local/bin"
[ -x "$BIN/attempt" ] || fail "attempt is not at $BIN/attempt"
[ -x "$BIN/attempt-hook" ] || fail "attempt-hook is not at $BIN/attempt-hook"
for profile in .zshrc .bashrc .bash_profile .bash_login .profile .config/fish; do
  [ ! -e "$HOME/$profile" ] || fail "the installer touched ~/$profile without being asked"
done

step "the installed binary is the release"
installed="$("$BIN/attempt" --version)"
printf '%s\n' "$installed"
case "$installed" in
  *"$VERSION"*) ;;
  *) fail "attempt --version says '$installed', expected $VERSION" ;;
esac

step "a second dry run has nothing left to change"
"$BIN/attempt" setup --dry-run --json > "$work/dry-run.json" || fail "attempt setup --dry-run failed"
python3 - "$work/dry-run.json" <<'PY' || fail "setup would still change something (see above)"
import json, sys
report = json.load(open(sys.argv[1]))
problems = []
if not report.get("ok"):
    problems.append(f"ok is false: {report.get('problems')}")
if not report["database"]["existed"]:
    problems.append("the database does not exist after setup")
actions = report["hooks"]["actions"]
if not actions:
    problems.append("no agent was wired (the fake Claude Code config was not detected)")
for action in actions:
    kind = action["outcome"]["kind"]
    if kind != "already_current":
        problems.append(f"{action['agent']}: {kind}")
if problems:
    print("\n".join(problems), file=sys.stderr)
    print(json.dumps(report["hooks"], indent=2), file=sys.stderr)
    sys.exit(1)
print("nothing left to change:", [a["agent"] for a in actions])
PY

step "our hooks are in the agent's settings, next to the user's own"
grep -q 'attempt' "$settings" || fail "no hook entry was written to $settings"
grep -q 'Bash(ls:\*)' "$settings" || fail "the user's own permission was lost"

step "attempt uninstall is clean"
"$BIN/attempt" uninstall || fail "attempt uninstall failed"
if grep -qi 'attempt' "$settings"; then
  cat "$settings" >&2
  fail "uninstall left entries of ours in $settings"
fi
grep -q 'Bash(ls:\*)' "$settings" || fail "uninstall removed the user's own permission"

printf '\nsmoke-install: ok (%s)\n' "$TAG"

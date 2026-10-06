#!/bin/sh
# AttemptDB installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh | sh
#
# Downloads the release archive for this machine, verifies it against the
# release's SHA256SUMS, and installs `attempt` and `attempt-hook`. Then it
# shows what `attempt setup` would change — the local database, hook entries
# in every coding agent found here (Claude Code, Codex, Cursor, Gemini CLI,
# next to whatever is already there), the OpenTelemetry exporter settings of
# Claude Code and Codex, and the background daemon — and asks before it
# touches any of that:
#
#   a terminal is available   shows the preview, asks "Apply these changes? [Y/n]"
#   no terminal (agent, CI)   installs the binary only, changes nothing else,
#                             and prints the exact command that would
#   --yes                     applies without asking
#
# Run it again any time: it upgrades the binary and, with consent, repairs the
# wiring; nothing is created twice. `attempt uninstall` reverses the wiring.
#
# The whole script is wrapped in functions and runs from its last line, so a
# download that is cut off part-way executes nothing.
#
# Environment:
#   ATTEMPTDB_VERSION   version to install (default: the latest release, found
#                       through the release page's redirect, not the rate-limited
#                       GitHub API, which is only the fallback)
#   ATTEMPTDB_BIN_DIR   install directory (default: ~/.local/bin; required when
#                       HOME is not set)
#   ATTEMPTDB_LIBC      linux libc flavour: musl (default) or gnu
#   ATTEMPTDB_NO_SETUP=1
#                       install the binary only; do not touch any coding
#                       agent's configuration (`attempt setup` does that
#                       later, when you choose)
#   ATTEMPTDB_ASSUME_YES=1
#                       same as --yes
#   ATTEMPTDB_MODIFY_PATH=1|0
#                       add the install directory to your shell profile (1) or
#                       leave the profile alone (0), without asking. Unset: ask
#                       when there is a terminal, otherwise only print the line
#   ATTEMPTDB_NO_MODIFY_PATH=1
#                       never touch a shell profile (wins over the above)
#   ATTEMPTDB_VERIFY_ATTESTATION=1
#                       also verify the archive's build provenance with
#                       `gh attestation verify` (needs the GitHub CLI); a failed
#                       or impossible verification stops the install
#   ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1
#                       install without verifying the download. Do not.
#
# Options (everything else after `sh -s --` goes to `attempt setup`, for example
# `--capture-mode metadata_only`, `--no-daemon`, or `--dry-run`):
#   --yes, -y           apply `attempt setup` without asking
#
# Verify a download by hand, with the archive and SHA256SUMS from the release:
#   sha256sum -c SHA256SUMS --ignore-missing
#   gh attestation verify attempt-<version>-<target>.tar.gz --repo nullarch/attemptdb

set -eu

REPO="nullarch/attemptdb"
INSTALL_URL="https://raw.githubusercontent.com/$REPO/main/install.sh"
# The last argument main() receives. Only the final line of this file passes
# it, so a copy cut off anywhere — even right after `main` — refuses to run
# instead of running without the arguments it was started with.
END_MARKER="--end-of-install-script"
# Written on the PATH line this script adds to a shell profile, so a person can
# find and remove it. (Adding it twice is avoided by looking for the directory.)
PATH_MARK="added by the AttemptDB installer"

say() { printf '%s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"; }

# fetch URL FILE: download into FILE. Fails with fetch_status (the downloader's
# exit status) and fetch_error (what it printed), so the caller can say what
# happened: a 404 means the file is not published, anything else means the
# download itself failed (no network, a reset connection, a proxy).
fetch() {
  fetch_error=""
  fetch_status=0
  case "$DOWNLOADER" in
    curl) fetch_error="$(curl -fsSL "$1" -o "$2" 2>&1)" || fetch_status=$? ;;
    *) fetch_error="$(wget -qO "$2" "$1" 2>&1)" || fetch_status=$? ;;
  esac
  return "$fetch_status"
}
fetch_stdout() {
  case "$DOWNLOADER" in
    curl) curl -fsSL "$1" ;;
    *) wget -qO- "$1" ;;
  esac
}
# fetch_headers URL: the response headers of the first answer, redirects not
# followed (curl -I; wget --spider prints them on stderr). wget's own exit
# status is ignored: it calls a redirect it was told not to follow a failure.
fetch_headers() {
  case "$DOWNLOADER" in
    curl) curl -fsSI --connect-timeout 20 --max-time 60 "$1" ;;
    *) wget --server-response --spider --max-redirect=0 "$1" 2>&1 || true ;;
  esac
}

# fetch_not_found: the last fetch failed because the server has no such file.
fetch_not_found() {
  case "$DOWNLOADER" in
    curl)
      [ "$fetch_status" = "22" ] || return 1
      case "$fetch_error" in
        *"error: 404"*) return 0 ;;
      esac
      return 1
      ;;
    # wget -q hides the reason; exit 8 is "the server answered with an error".
    *) [ "$fetch_status" = "8" ] ;;
  esac
}

# not_found_text URL: what is known about a file the server does not have.
not_found_text() {
  case "$DOWNLOADER" in
    curl) printf '%s: not found, HTTP 404' "$1" ;;
    *) printf '%s: the server answered with an error' "$1" ;;
  esac
}

# fetch_error_text: what the downloader said, for a message.
fetch_error_text() {
  if [ -n "$fetch_error" ]; then
    printf '%s' "$fetch_error"
  else
    printf '%s exited with status %s' "$DOWNLOADER" "$fetch_status"
  fi
}

# shellcheck disable=SC2329  # run by the EXIT trap that main installs
cleanup() {
  if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
}

# A controlling terminal exists even when stdin is the pipe this script is
# read from (`curl | sh`) or stdout is redirected, so ask /dev/tty itself.
# `true`, not `:`: a failed redirection on a special builtin makes dash exit.
have_tty() { { true </dev/tty; } 2>/dev/null; }

# ask QUESTION Y|N: prompt on the terminal; the capital letter is the default.
# End of input (Ctrl-D) is never a yes.
ask() {
  if [ "$2" = "Y" ]; then hint="[Y/n]"; else hint="[y/N]"; fi
  { printf '%s %s ' "$1" "$hint" >/dev/tty; } 2>/dev/null || return 1
  if ! { read -r reply </dev/tty; } 2>/dev/null; then
    { printf '\n' >/dev/tty; } 2>/dev/null || true
    return 1
  fi
  case "$reply" in
    "") [ "$2" = "Y" ] ;;
    [Yy] | [Yy][Ee][Ss]) return 0 ;;
    *) return 1 ;;
  esac
}

# shell_quote ARG...: the arguments as one line a shell reads back unchanged.
shell_quote() {
  quoted=""
  for q in "$@"; do
    case "$q" in
      "") q="''" ;;
      *[!A-Za-z0-9_./:=@%+,-]*) q="'$(printf '%s' "$q" | sed "s/'/'\\\\''/g")'" ;;
    esac
    quoted="$quoted $q"
  done
  printf '%s' "${quoted# }"
}

# under_home PATH: PATH is inside the home directory. Never true when HOME is
# unset or empty (an empty prefix would match every absolute path).
under_home() {
  [ -n "$HOME_DIR" ] || return 1
  case "$1" in
    "$HOME_DIR"/*) return 0 ;;
  esac
  return 1
}

on_path() {
  case ":$PATH:" in
    *":$BIN_DIR:"*) return 0 ;;
  esac
  return 1
}

# The installed binary the way a person can type it right now: plain `attempt`
# once the directory is on PATH, otherwise its path.
attempt_cmd() {
  tilde='~'
  if on_path; then
    printf 'attempt'
    return 0
  fi
  if under_home "$BIN_DIR"; then
    rest="${BIN_DIR#"$HOME_DIR"/}"
    case "$rest" in
      *[!A-Za-z0-9_./-]*) shell_quote "$BIN_DIR/attempt" ;;
      *) printf '%s/%s/attempt' "$tilde" "$rest" ;;
    esac
  else
    shell_quote "$BIN_DIR/attempt"
  fi
}

# ---- target detection ------------------------------------------------------

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) err "unsupported architecture: $arch" ;;
  esac

  case "$os" in
    Darwin)
      target="${arch}-apple-darwin"
      ;;
    Linux)
      case "$LIBC" in
        musl) target="${arch}-unknown-linux-musl" ;;
        gnu) target="${arch}-unknown-linux-gnu" ;;
        *) err "ATTEMPTDB_LIBC must be 'musl' or 'gnu', got '$LIBC'" ;;
      esac
      ;;
    *)
      err "unsupported operating system: $os (Windows: use install.ps1)"
      ;;
  esac
}

# ---- version resolution ----------------------------------------------------

# The newest release's tag from the release page's own redirect:
# /releases/latest answers 302 with Location: .../releases/tag/vX.Y.Z. It is
# not the API, which allows an anonymous address 60 requests an hour (an office,
# a CI pool, a shared VPN all share them) and answers 403 beyond that.
# Sets latest_tag (empty when the answer had none) and, on failure,
# redirect_error. Called directly, never inside $( ), so the variables stay.
latest_from_redirect() {
  latest_tag=""
  redirect_error=""
  headers="$(fetch_headers "https://github.com/$REPO/releases/latest" 2>&1)" || {
    redirect_error="$headers"
    return 1
  }
  latest_tag="$(printf '%s\n' "$headers" | tr -d '\r' \
    | sed -n 's/^[[:space:]]*[Ll][Oo][Cc][Aa][Tt][Ii][Oo][Nn]:[[:space:]]*.*\/releases\/tag\/\([^[:space:]?#]*\).*$/\1/p' \
    | head -n 1)"
}

# The same from the API, the fallback when the redirect told us nothing (a proxy
# that strips headers, a wget that cannot show them). Sets latest_tag and, on
# failure, api_error.
latest_from_api() {
  latest_tag=""
  api_error=""
  reply="$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" 2>&1)" || {
    api_error="$reply"
    return 1
  }
  latest_tag="$(printf '%s\n' "$reply" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
}

resolve_version() {
  version="${ATTEMPTDB_VERSION:-}"
  if [ -z "$version" ]; then
    say "Resolving the latest release..."
    redirect_error=""
    api_error=""
    latest_from_redirect || true
    if [ -z "$latest_tag" ]; then
      latest_from_api || true
    fi
    version="$latest_tag"
    if [ -z "$version" ]; then
      case "$api_error" in
        *"error: 404"*)
          err "could not resolve the latest release: the repository has none that can be downloaded. Is one published yet?
Build from source instead:
  git clone https://github.com/$REPO
  cd attemptdb && cargo install --path crates/attempt"
          ;;
      esac
      err "could not resolve the latest release.
  release page: ${redirect_error:-it answered, but with no release tag}
  GitHub API:   ${api_error:-it answered, but with no release tag}
Check your network connection and proxy settings and run the installer again; the
API allows an anonymous address only 60 requests an hour. Or name the version:
  curl -fsSL $INSTALL_URL | ATTEMPTDB_VERSION=<version> sh"
    fi
  fi
  version="${version#v}"
  # The version becomes part of a URL and a file name: only what a release tag is made of.
  case "$version" in
    "" | *[!0-9A-Za-z.+-]*) err "unexpected release version '$version'" ;;
  esac

  stem="attempt-${version}-${target}"
  base="https://github.com/$REPO/releases/download/v${version}"
}

# ---- download and verify ---------------------------------------------------

download_and_verify() {
  tmp="$(mktemp -d)"

  say "Downloading $stem..."
  if ! fetch "$base/${stem}.tar.gz" "$tmp/${stem}.tar.gz"; then
    if fetch_not_found; then
      err "no release asset for $target in v$version ($(not_found_text "$base/${stem}.tar.gz"))"
    fi
    err "could not download $base/${stem}.tar.gz
$(fetch_error_text)
Check your network connection and proxy settings and run the installer again."
  fi

  # Verification is not optional. This script is run as `curl … | sh`, so a
  # missing checksum file or a missing hashing tool must stop the install, not
  # downgrade it to an unverified one: an attacker able to remove SHA256SUMS
  # from a release is an attacker able to replace the tarball beside it. Every
  # release publishes SHA256SUMS, so neither branch below should ever be
  # reached.
  if [ "${ATTEMPTDB_INSECURE_SKIP_CHECKSUM:-0}" = "1" ]; then
    say "warning: ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 — installing WITHOUT verifying the download"
  else
    if ! fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
      if fetch_not_found; then
        err "SHA256SUMS is not published for v$version, so this download cannot be verified.
Refusing to install. Set ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 to override."
      fi
      err "could not download $base/SHA256SUMS, so this download cannot be verified.
$(fetch_error_text)
Refusing to install. Check your network connection and proxy settings and run the
installer again, or set ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 to override."
    fi
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "$tmp/${stem}.tar.gz" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
      actual="$(shasum -a 256 "$tmp/${stem}.tar.gz" | awk '{print $1}')"
    else
      err "no sha256 tool found (looked for sha256sum and shasum), so this download
cannot be verified. Refusing to install. Install coreutils, or set
ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 to override."
    fi
    expected="$(grep " ${stem}.tar.gz\$" "$tmp/SHA256SUMS" | awk '{print $1}' | head -n 1)"
    [ -n "$expected" ] || err "$stem.tar.gz is not listed in SHA256SUMS"
    [ "$actual" = "$expected" ] || err "checksum mismatch
  expected $expected
  actual   $actual"
    say "Checksum verified."
  fi

  # Optional, stronger: who built it. SHA256SUMS only proves the archive is the
  # one the release published; the attestation proves this repository's
  # workflow produced it. Asked for explicitly, so asking and not getting it
  # is a failure, never a silent downgrade.
  if [ "${ATTEMPTDB_VERIFY_ATTESTATION:-0}" = "1" ]; then
    command -v gh >/dev/null 2>&1 \
      || err "ATTEMPTDB_VERIFY_ATTESTATION=1 needs the GitHub CLI (gh), which was not found.
Install it (https://cli.github.com), or unset the variable to rely on the SHA-256 check alone."
    say "Verifying build provenance..."
    gh attestation verify "$tmp/${stem}.tar.gz" --repo "$REPO" \
      || err "build provenance verification failed for ${stem}.tar.gz; refusing to install.
(gh needs a login or GH_TOKEN. To see why, run it by hand:
  gh attestation verify ${stem}.tar.gz --repo $REPO)"
    say "Build provenance verified."
  fi

  tar -xzf "$tmp/${stem}.tar.gz" -C "$tmp"
  [ -f "$tmp/$stem/attempt" ] || err "archive did not contain the attempt binary"
}

# ---- install ---------------------------------------------------------------

install_binaries() {
  mkdir -p "$BIN_DIR" || err "cannot create $BIN_DIR"
  # Replace atomically so a running daemon keeps its open file handle.
  cp "$tmp/$stem/attempt" "$BIN_DIR/.attempt.new"
  chmod 755 "$BIN_DIR/.attempt.new"
  mv -f "$BIN_DIR/.attempt.new" "$BIN_DIR/attempt"
  # The dedicated hook executable (releases from 0.2.0): `attempt hook install`
  # references it when it sits next to `attempt`, which makes each hook a
  # fraction of the cost of paging in the full binary.
  if [ -f "$tmp/$stem/attempt-hook" ]; then
    cp "$tmp/$stem/attempt-hook" "$BIN_DIR/.attempt-hook.new"
    chmod 755 "$BIN_DIR/.attempt-hook.new"
    mv -f "$BIN_DIR/.attempt-hook.new" "$BIN_DIR/attempt-hook"
  fi

  # No quarantine handling is needed on macOS: files fetched with curl or wget
  # do not carry com.apple.quarantine (only browsers and AirDrop add it), so
  # Gatekeeper never sees this binary as a download.

  say ""
  say "Installed attempt $version to $BIN_DIR/attempt"
}

# ---- setup -----------------------------------------------------------------
#
# The binary knows how to wire a machine; this script only downloads it. It
# also does not wire one without consent: `attempt setup` edits other tools'
# configuration files, so it is shown (`--dry-run`) and then applied only when
# the person says yes at a terminal, or asked for it with --yes /
# ATTEMPTDB_ASSUME_YES=1. `attempt setup` is idempotent and reports every step;
# its exit code is 1 when something it tried failed (a daemon that would not
# start), which is worth surfacing but not worth pretending the binary was not
# installed.
#
# Sets setup_status, the script's exit code.

run_setup() {
  if "$BIN_DIR/attempt" setup --source install.sh "$@"; then
    setup_status=0
  else
    setup_status=$?
  fi
}

# The steps that wire a machine by hand, for a release that predates `setup`.
# Carries over the two options that mean something to them.
old_release_steps() {
  mode=""
  want_value=0
  with_daemon=1
  for opt in "$@"; do
    if [ "$want_value" = "1" ]; then
      mode="$opt"
      want_value=0
      continue
    fi
    case "$opt" in
      --capture-mode) want_value=1 ;;
      --capture-mode=*) mode="${opt#--capture-mode=}" ;;
      --no-daemon) with_daemon=0 ;;
    esac
  done
  a="$(attempt_cmd)"
  say ""
  say "attempt $version predates \`attempt setup\`, so nothing was wired. To do it by hand:"
  if [ -n "$mode" ]; then
    say "  $a init --capture-mode $(shell_quote "$mode")"
  else
    say "  $a init"
  fi
  say "  $a hook install"
  if [ "$with_daemon" = "1" ]; then
    say "  $a daemon install"
  fi
  say "  $a doctor"
}

setup_step() {
  setup_status=0

  # Releases before 0.2.14 have no `setup`: ATTEMPTDB_VERSION can ask for one,
  # and this script on `main` can briefly run ahead of the newest release. The
  # steps that exist there are printed, whichever way the person got here
  # (ATTEMPTDB_NO_SETUP=1 included), because `attempt setup` would only answer
  # "unrecognized subcommand".
  if ! "$BIN_DIR/attempt" setup --help >/dev/null 2>&1; then
    old_release_steps "$@"
    return 0
  fi

  if [ "${ATTEMPTDB_NO_SETUP:-0}" = "1" ]; then
    say ""
    say "Next (ATTEMPTDB_NO_SETUP=1 skipped this):"
    say "  $(attempt_cmd) setup --dry-run   # what it would change; writes nothing"
    say "  $(attempt_cmd) setup             # database, agent hooks, background daemon, check"
    return 0
  fi

  # The person already asked to only look: show it, change nothing, ask nothing.
  for arg in "$@"; do
    if [ "$arg" = "--dry-run" ]; then
      say ""
      if "$BIN_DIR/attempt" setup --source install.sh "$@"; then :; else
        status=$?
        [ "$status" -le 1 ] || setup_status=$status
      fi
      return 0
    fi
  done

  if [ "$assume_yes" = "1" ] || [ "${ATTEMPTDB_ASSUME_YES:-0}" = "1" ]; then
    say ""
    run_setup "$@"
    return 0
  fi

  say ""
  say "This is what setup would change on this machine (nothing is written yet):"
  say ""
  if "$BIN_DIR/attempt" setup --dry-run --source install.sh "$@"; then :; else
    status=$?
    # 1 is a report with problems in it, which the person should still see and
    # judge; anything above is `attempt` refusing its arguments.
    if [ "$status" -gt 1 ]; then
      setup_status=$status
      return 0
    fi
  fi

  args_line="$(shell_quote "$@")"
  apply_cmd="$(attempt_cmd) setup${args_line:+ $args_line}"

  if have_tty; then
    say ""
    if ask "Apply these changes?" Y; then
      say ""
      run_setup "$@"
    else
      say ""
      say "Nothing was changed. The attempt binary is installed; apply the changes later with:"
      say "  $apply_cmd"
    fi
    return 0
  fi

  pin=""
  if [ -n "${ATTEMPTDB_VERSION:-}" ]; then pin="ATTEMPTDB_VERSION=$(shell_quote "$version") "; fi
  say ""
  say "There is no terminal to ask on, so nothing was changed beyond installing the"
  say "binary. To apply the changes above:"
  say "  $apply_cmd"
  say "or run this installer again, telling it to go ahead:"
  say "  curl -fsSL $INSTALL_URL | ${pin}sh -s -- --yes${args_line:+ $args_line}"
  return 0
}

# ---- PATH ------------------------------------------------------------------
#
# Hooks and the daemon use the binary's absolute path, so capture works with
# the directory off PATH; PATH only matters for typing `attempt`. Adding it
# means editing a shell profile, which is done with consent only: asked on the
# terminal (default yes, but only when the profile is a plain file we can
# write), or ATTEMPTDB_MODIFY_PATH=1. Never a symlink (dotfiles checkouts),
# never a shell that is not the person's login shell, never twice.

# Sets profile_file and profile_kind (posix | fish); empty when the login shell
# is one this script does not know how to configure.
detect_profile() {
  profile_file=""
  profile_kind=""
  # No home directory, no profile to name.
  [ -n "$HOME_DIR" ] || return 0
  case "${SHELL:-}" in
    */zsh | zsh)
      profile_kind=posix
      profile_file="${ZDOTDIR:-$HOME_DIR}/.zshrc"
      ;;
    */bash | bash)
      profile_kind=posix
      if [ "$os" = "Darwin" ]; then
        # Terminal.app starts login shells, which read the first of these that
        # exists and never .bashrc.
        profile_file="$HOME_DIR/.bash_profile"
        for candidate in .bash_profile .bash_login .profile; do
          if [ -e "$HOME_DIR/$candidate" ]; then
            profile_file="$HOME_DIR/$candidate"
            break
          fi
        done
      else
        profile_file="$HOME_DIR/.bashrc"
      fi
      ;;
    */fish | fish)
      profile_kind=fish
      profile_file="${XDG_CONFIG_HOME:-$HOME_DIR/.config}/fish/conf.d/attemptdb.fish"
      ;;
  esac
}

# A plain file we can write, or a missing one in a directory we can write.
profile_writable() {
  if [ -L "$1" ]; then return 1; fi
  if [ -e "$1" ]; then
    if [ -f "$1" ] && [ -w "$1" ]; then return 0; fi
    return 1
  fi
  dir="${1%/*}"
  while [ ! -d "$dir" ]; do
    dir="${dir%/*}"
    [ -n "$dir" ] || return 1
  done
  [ -w "$dir" ]
}

# The profile already names this directory (our own line, or one written by
# hand), in either spelling.
profile_has_path() {
  [ -f "$profile_file" ] || return 1
  grep -F -q -e "$BIN_DIR" -e "$home_form" "$profile_file" 2>/dev/null
}

# A directory name this script cannot put inside double quotes safely.
unsafe_for_profile() {
  case "$home_form" in
    *'"'* | *'`'* | *\\* | *"$newline"*) return 0 ;;
  esac
  case "${home_form#\$HOME}" in
    *'$'*) return 0 ;;
  esac
  return 1
}

# Append the PATH line (a new file for fish) to $profile_file.
write_profile() {
  mkdir -p "${profile_file%/*}" 2>/dev/null || return 1
  # Never glue our line onto the last line of a file with no final newline.
  if [ -s "$profile_file" ] && [ -n "$(tail -c 1 "$profile_file")" ]; then
    printf '\n' >>"$profile_file" || return 1
  fi
  if [ "$profile_kind" = "fish" ]; then
    # shellcheck disable=SC2016  # $PATH belongs to the profile, not to this script
    {
      printf '# %s\n' "$PATH_MARK"
      printf 'if not contains -- "%s" $PATH\n' "$home_form"
      printf '    set -gx PATH "%s" $PATH\n' "$home_form"
      printf 'end\n'
    } >>"$profile_file" || return 1
  else
    # shellcheck disable=SC2016  # $PATH belongs to the profile, not to this script
    printf 'export PATH="%s:$PATH" # %s\n' "$home_form" "$PATH_MARK" >>"$profile_file" || return 1
  fi
}

path_hint() {
  say ""
  say "$BIN_DIR is not on your PATH, so type $(attempt_cmd) for now. To add it:"
  say "  $show_line      # in ${profile_file:-your shell profile}"
  if [ -n "$profile_file" ] && ! [ -L "$profile_file" ]; then
    say "or run this installer again with ATTEMPTDB_MODIFY_PATH=1 and it adds the line itself."
  fi
}

path_step() {
  if on_path; then return 0; fi

  detect_profile
  # How the directory is written into the profile: under $HOME it stays
  # relative to it, so a moved home directory does not break the line.
  home_form="$BIN_DIR"
  if under_home "$BIN_DIR"; then
    home_form="\$HOME/${BIN_DIR#"$HOME_DIR"/}"
  fi
  newline='
'
  show_line="export PATH=\"$BIN_DIR:\$PATH\""
  if [ "$profile_kind" = "fish" ]; then show_line="fish_add_path \"$BIN_DIR\""; fi

  if [ "${ATTEMPTDB_NO_MODIFY_PATH:-0}" = "1" ] || [ "${ATTEMPTDB_MODIFY_PATH:-}" = "0" ] \
    || [ -z "$profile_file" ] || unsafe_for_profile; then
    path_hint
    return 0
  fi
  if profile_has_path; then
    say ""
    say "$profile_file already mentions $BIN_DIR, so it was not edited; open a new terminal to pick it up."
    return 0
  fi
  if [ -L "$profile_file" ]; then
    say ""
    say "$profile_file is a symlink (a dotfiles checkout?), so it was not edited. Add this line yourself:"
    say "  $show_line"
    return 0
  fi

  consent=0
  if [ "${ATTEMPTDB_MODIFY_PATH:-}" = "1" ]; then
    consent=1
  elif have_tty && profile_writable "$profile_file"; then
    say ""
    say "$BIN_DIR is not on your PATH."
    if ask "Add it in $profile_file?" Y; then consent=1; fi
  fi

  if [ "$consent" != "1" ]; then
    path_hint
    return 0
  fi
  if write_profile; then
    say ""
    say "Added $BIN_DIR to PATH in $profile_file (open a new terminal for it to apply)."
  else
    say ""
    say "Could not write $profile_file."
    path_hint
  fi
  return 0
}

# ---- main ------------------------------------------------------------------

main() {
  # Arguments: --yes/-y and the end marker are ours; the rest is for
  # `attempt setup` and is left in "$@" (each argument is rotated to the back
  # unless it is ours). The marker only counts as the very last argument.
  assume_yes=0
  complete=0
  want_help=0
  remaining=$#
  while [ "$remaining" -gt 0 ]; do
    arg="$1"
    shift
    remaining=$((remaining - 1))
    case "$arg" in
      "$END_MARKER") complete=1 ;;
      --yes | -y)
        assume_yes=1
        complete=0
        ;;
      --help | -h)
        want_help=1
        complete=0
        ;;
      *)
        set -- "$@" "$arg"
        complete=0
        ;;
    esac
  done
  if [ "$complete" != "1" ]; then
    printf 'error: install.sh was cut off or edited before its last line; nothing was installed.\n' >&2
    printf 'Run the one-line command again.\n' >&2
    exit 1
  fi

  if [ "$want_help" = "1" ]; then
    say "AttemptDB installer. Installs the attempt binary, then (asking first) runs \`attempt setup\`."
    say "  curl -fsSL $INSTALL_URL | sh -s -- [--yes] [attempt setup options]"
    say "  --yes, -y   apply \`attempt setup\` without asking (also ATTEMPTDB_ASSUME_YES=1)"
    say "Anything else is passed to \`attempt setup\`, e.g. --capture-mode metadata_only, --no-daemon, --dry-run."
    say "Environment variables are listed at the top of the script: $INSTALL_URL"
    exit 0
  fi

  # HOME can be unset (a minimal container, `env -i`, a cron job): that is only
  # a problem when the install directory was to come from it.
  HOME_DIR="${HOME:-}"
  BIN_DIR="${ATTEMPTDB_BIN_DIR:-}"
  if [ -z "$BIN_DIR" ]; then
    [ -n "$HOME_DIR" ] || err "HOME is not set, so there is no default install directory (\$HOME/.local/bin).
Set ATTEMPTDB_BIN_DIR to the directory to install into, for example:
  curl -fsSL $INSTALL_URL | ATTEMPTDB_BIN_DIR=/usr/local/bin sh"
    BIN_DIR="$HOME_DIR/.local/bin"
  fi
  BIN_DIR="${BIN_DIR%/}"
  LIBC="${ATTEMPTDB_LIBC:-musl}"
  tmp=""
  trap 'cleanup' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  need uname
  need mkdir
  need tar
  need mktemp

  if command -v curl >/dev/null 2>&1; then
    DOWNLOADER=curl
  elif command -v wget >/dev/null 2>&1; then
    DOWNLOADER=wget
  else
    err "neither curl nor wget is available"
  fi

  detect_target
  resolve_version
  download_and_verify
  install_binaries
  setup_step "$@"
  path_step
  say ""
  say "Nothing is uploaded anywhere. There is no account and no telemetry."
  exit "$setup_status"
}

main "$@" "$END_MARKER"

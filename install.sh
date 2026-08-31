#!/bin/sh
# AttemptDB installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/nullarch/attemptdb/main/install.sh | sh
#
# Downloads a signed-by-checksum release archive, verifies it, and installs the
# `attempt` binary. It does NOT touch any coding agent's configuration: hook
# installation is a separate, explicit `attempt hook install`.
#
# Environment:
#   ATTEMPTDB_VERSION   version to install (default: latest release)
#   ATTEMPTDB_BIN_DIR   install directory (default: ~/.local/bin)
#   ATTEMPTDB_LIBC      linux libc flavour: musl (default) or gnu
#   ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1
#                       install without verifying the download. Do not.

set -eu

REPO="nullarch/attemptdb"
BIN_DIR="${ATTEMPTDB_BIN_DIR:-$HOME/.local/bin}"
LIBC="${ATTEMPTDB_LIBC:-musl}"

say() { printf '%s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"; }

need uname
need mkdir
need tar

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
  fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
  fetch_stdout() { wget -qO- "$1"; }
else
  err "neither curl nor wget is available"
fi

# ---- target detection ------------------------------------------------------

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

# ---- version resolution ----------------------------------------------------

version="${ATTEMPTDB_VERSION:-}"
if [ -z "$version" ]; then
  say "Resolving the latest release..."
  version="$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)" || true
  [ -n "$version" ] || err "could not resolve the latest release. Is one published yet?
Build from source instead:
  git clone https://github.com/$REPO
  cd attemptdb && cargo install --path crates/attempt"
fi
version="${version#v}"

stem="attempt-${version}-${target}"
base="https://github.com/$REPO/releases/download/v${version}"

# ---- download and verify ---------------------------------------------------

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT INT TERM

say "Downloading $stem..."
fetch "$base/${stem}.tar.gz" "$tmp/${stem}.tar.gz" \
  || err "no release asset for $target in v$version"

# Verification is not optional. This script is run as `curl … | sh`, so a
# missing checksum file or a missing hashing tool must stop the install, not
# downgrade it to an unverified one: an attacker able to remove SHA256SUMS from
# a release is an attacker able to replace the tarball beside it. Every release
# publishes SHA256SUMS, so neither branch below should ever be reached.
if [ "${ATTEMPTDB_INSECURE_SKIP_CHECKSUM:-0}" = "1" ]; then
  say "warning: ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 — installing WITHOUT verifying the download"
else
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null \
    || err "SHA256SUMS is not published for v$version, so this download cannot be verified.
Refusing to install. Set ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 to override."
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

tar -xzf "$tmp/${stem}.tar.gz" -C "$tmp"
[ -f "$tmp/$stem/attempt" ] || err "archive did not contain the attempt binary"

# ---- install ---------------------------------------------------------------

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

# macOS quarantines anything downloaded by curl; an unsigned binary is then
# refused by Gatekeeper. Clearing the flag here is what Homebrew does too.
if [ "$os" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
  xattr -d com.apple.quarantine "$BIN_DIR/attempt" 2>/dev/null || true
fi

say ""
say "Installed attempt $version to $BIN_DIR/attempt"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    say ""
    say "$BIN_DIR is not on your PATH. Add it:"
    say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc   # or ~/.bashrc"
    ;;
esac

say ""
say "Next:"
say "  attempt init          # create your local database"
say "  attempt hook install  # wire up Claude Code / Codex / Cursor / Gemini CLI"
say "  attempt doctor        # verify each agent is configured and active"
say ""
say "Nothing is uploaded anywhere. There is no account and no telemetry."

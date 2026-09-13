#!/bin/sh
# Put `attempt` and `attempt-hook` where the app bundle expects its sidecars:
# app/src-tauri/binaries/<name>-<target triple>. Tauri copies them into the
# bundle (Contents/MacOS/ on macOS) without the triple.
#
#   app/scripts/sidecars.sh              # build from this checkout, release profile
#   app/scripts/sidecars.sh <dir>        # copy from an extracted release archive
#
#   TARGET_TRIPLE=aarch64-apple-darwin   # cross-build / copy for another triple
#                                        # (default: this machine's host triple)
set -eu

root="$(cd "$(dirname "$0")/../.." && pwd)"
dest="$root/app/src-tauri/binaries"
host="$(rustc -vV | sed -n 's/^host: //p')"
triple="${TARGET_TRIPLE:-$host}"
mkdir -p "$dest"

if [ $# -ge 1 ]; then
  src="$1"
else
  if [ "$triple" = "$host" ]; then
    cargo build --release --locked -p attemptdb -p attempt-hook --manifest-path "$root/Cargo.toml"
    src="$root/target/release"
  else
    cargo build --release --locked -p attemptdb -p attempt-hook --manifest-path "$root/Cargo.toml" --target "$triple"
    src="$root/target/$triple/release"
  fi
fi

for name in attempt attempt-hook; do
  [ -f "$src/$name" ] || { echo "error: $src/$name not found" >&2; exit 1; }
  cp "$src/$name" "$dest/$name-$triple"
  chmod 755 "$dest/$name-$triple"
done
echo "sidecars for $triple:"
"$dest/attempt-$triple" --version
"$dest/attempt-hook-$triple" --version

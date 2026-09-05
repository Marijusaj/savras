#!/bin/sh
# Build Savras and install it, without breaking a copy that is already running.
#
# Copying over the binary in place rewrites the same inode. A running Savras
# then has its pages pulled out from under it, and on macOS the overwritten file
# fails the signature check at exec and is killed outright — "svr" simply dies
# with signal 9. Writing a new file and renaming it over the old one is atomic:
# the new binary gets a new inode, and anything still running keeps the old one.
set -eu

dir="${1:-$HOME/.local/bin}"
cd "$(dirname "$0")/.."

cargo build --release
mkdir -p "$dir"
cp target/release/svr "$dir/.svr.new"
chmod +x "$dir/.svr.new"
mv -f "$dir/.svr.new" "$dir/svr"

echo "installed $("$dir/svr" --version) to $dir/svr"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "note: $dir is not on your PATH" ;;
esac

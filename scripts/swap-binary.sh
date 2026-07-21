#!/usr/bin/env bash
# Replace an executable through a fresh inode, then ad-hoc re-sign on macOS.
#
# The one home for this dance: an in-place overwrite keeps the old inode, and macOS's
# code-signing cache then SIGKILLs the binary at every launch (exit 137, no log line).
# Both `just install` and `just qa-install` route through here.
#
#   swap-binary.sh <src> <dst>
set -euo pipefail

src="$1"
dst="$2"
[ -f "$src" ] || { echo "swap-binary: missing source $src" >&2; exit 1; }

rm -f "$dst"
cp "$src" "$dst.staging"
mv "$dst.staging" "$dst"
[ "$(uname)" = "Darwin" ] && codesign --force --sign - "$dst"
exit 0

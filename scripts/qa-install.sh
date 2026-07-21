#!/usr/bin/env bash
# Swap a locally built herdr-reviewr into the GitHub-installed plugin for QA.
# The full procedure and every known failure mode: docs/qa-install.md.
set -euo pipefail

NEW="target/release/herdr-reviewr"
[ -f "$NEW" ] || { echo "qa-install: build first (cargo build --release)" >&2; exit 1; }

# Locate the managed plugin install. Exactly one is expected.
shopt -s nullglob
roots=("$HOME"/.config/herdr/plugins/github/persiyanov.reviewr-*)
shopt -u nullglob
[ ${#roots[@]} -eq 1 ] || {
  echo "qa-install: expected one installed plugin, found ${#roots[@]}:" >&2
  printf '  %s\n' "${roots[@]:-none}" >&2
  exit 1
}
BIN_DIR="${roots[0]}/bin"
BIN="$BIN_DIR/herdr-reviewr"

# Keep one pristine release for rollback. Never overwrite an existing backup.
[ -f "$BIN.release-backup" ] || cp "$BIN" "$BIN.release-backup"

# The fresh-inode swap lives in one place (scripts/swap-binary.sh): an in-place overwrite
# keeps the old inode and macOS then SIGKILLs the binary at every launch (exit 137).
"$(dirname "$0")/swap-binary.sh" "$NEW" "$BIN"

# Prove the installed binary actually runs before touching any pane.
"$BIN" --resolve-plugin-config >/dev/null || {
  echo "qa-install: installed binary failed --resolve-plugin-config (exit $?)" >&2
  echo "qa-install: rolled-back copy available at $BIN.release-backup" >&2
  exit 1
}
echo "installed: $BIN"

# Running panes keep executing the old binary image. Only a pane restart picks this up.
live=$(pgrep -f "$BIN" || true)
if [ -n "$live" ]; then
  echo "note: running reviewr panes still use the OLD binary (pids:" $live ")"
fi
echo "next: close and reopen each reviewr pane with the toggle keybinding inside herdr."
echo "rollback: cp $BIN.release-backup <staging> && rm $BIN && mv <staging> $BIN"

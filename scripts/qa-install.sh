#!/usr/bin/env bash
# qa-install.sh [--restore]: swap this build and manifest into the installed plugin for QA, or put
# the backed-up release back. The procedure and its failure modes: docs/qa-install.md.
set -euo pipefail

NEW="target/release/herdr-reviewr"
MANIFEST="herdr-plugin.toml"

# Locate the managed plugin install: herdr's own word first, since 0.9.5 moved the checkout. Exactly
# one is expected.
roots=()
root=$(herdr plugin list --json 2>/dev/null | python3 -c 'import json,sys
for p in json.load(sys.stdin)["result"]["plugins"]:
    if p["plugin_id"] == "persiyanov.reviewr" and p.get("source", {}).get("kind") == "github":
        print(p["plugin_root"])' 2>/dev/null) || root=""
[ -z "$root" ] || roots=("$root")
if [ ${#roots[@]} -eq 0 ]; then
  shopt -s nullglob
  roots=("$HOME"/.config/herdr/plugins/github/persiyanov.reviewr-*)
  shopt -u nullglob
fi
[ ${#roots[@]} -eq 1 ] || {
  echo "qa-install: expected one installed plugin, found ${#roots[@]}:" >&2
  printf '  %s\n' "${roots[@]:-none}" >&2
  exit 1
}
ROOT="${roots[0]}"
BIN="$ROOT/bin/herdr-reviewr"

# herdr rereads the installed manifest on every plugin call, so it is renamed into place and
# herdr never reads a half-written file.
place_manifest() {
  cp "$1" "$ROOT/$MANIFEST.staging"
  mv "$ROOT/$MANIFEST.staging" "$ROOT/$MANIFEST"
  echo "installed: $ROOT/$MANIFEST"
}

if [ "${1:-}" = "--restore" ]; then
  "$(dirname "$0")/swap-binary.sh" "$BIN.release-backup" "$BIN"
  echo "installed: $BIN"
  [ ! -f "$ROOT/$MANIFEST.release-backup" ] || place_manifest "$ROOT/$MANIFEST.release-backup"
  echo "next: close and reopen each reviewr pane with the toggle keybinding inside herdr."
  exit 0
fi

[ -f "$NEW" ] || { echo "qa-install: build first (cargo build --release)" >&2; exit 1; }

# A manifest asking for a newer herdr than the running one disables the whole plugin, so refuse
# before touching anything.
need=$(sed -n 's/^min_herdr_version = "\(.*\)"/\1/p' "$MANIFEST")
have=$(herdr --version 2>/dev/null | awk '{print $2}') || have=""
if [ -z "$have" ] || [ "$(printf '%s\n%s\n' "$need" "$have" | sort -V | head -1)" != "$need" ]; then
  echo "qa-install: this build's manifest needs herdr $need, found '${have:-none}'. Upgrade herdr first." >&2
  exit 1
fi

# Keep one pristine release for rollback. Never overwrite an existing backup.
[ -f "$BIN.release-backup" ] || cp "$BIN" "$BIN.release-backup"
[ -f "$ROOT/$MANIFEST.release-backup" ] || cp "$ROOT/$MANIFEST" "$ROOT/$MANIFEST.release-backup"

# The fresh-inode swap lives in one place (scripts/swap-binary.sh): an in-place overwrite
# keeps the old inode and macOS then SIGKILLs the binary at every launch (exit 137).
"$(dirname "$0")/swap-binary.sh" "$NEW" "$BIN"

# Prove the installed binary actually runs before touching any pane: an action outside a
# workspace refuses at once, and a killed binary (exit 137) prints nothing.
empty=$(mktemp -d)
said=$(env -u HERDR_WORKSPACE_ID -u HERDR_PANE_ID HERDR_PLUGIN_CONFIG_DIR="$empty" \
  "$BIN" --action close 2>&1) || :
rmdir "$empty"
case "$said" in
*"invoke from inside herdr"*) ;;
*)
  echo "qa-install: installed binary did not run: ${said:-no output}" >&2
  echo "qa-install: rolled-back copy available at $BIN.release-backup" >&2
  exit 1
  ;;
esac
echo "installed: $BIN"

# The manifest names the commands herdr runs for the pane and every action, so a build that
# changes them is only exercised with its own manifest.
place_manifest "$MANIFEST"

# Running panes keep executing the old binary image. Only a pane restart picks this up.
live=$(pgrep -f "$BIN" || true)
if [ -n "$live" ]; then
  echo "note: running reviewr panes still use the OLD binary (pids:" $live ")"
fi
echo "next: close and reopen each reviewr pane with the toggle keybinding inside herdr."
echo "rollback: just qa-restore"

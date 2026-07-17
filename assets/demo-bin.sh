#!/usr/bin/env bash
# Print the installed release binary only when it matches this checkout's plugin version.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(awk -F'"' '/^version = / {print $2; exit}' "$ROOT/herdr-plugin.toml")"
BIN="$(
  herdr plugin list --plugin persiyanov.reviewr --json |
    jq -er --arg version "$VERSION" \
      '.result.plugins[]
       | select(
           .version == $version
           and .source.kind == "github"
           and .source.requested_ref == ("v" + $version)
         )
       | .plugin_root + "/bin/herdr-reviewr"'
)"

test -x "$BIN"
printf '%s\n' "$BIN"

#!/usr/bin/env bash
# bohay `[[build]]` step: download the prebuilt herdr-reviewr binary for this platform from
# the matching GitHub Release into the module's bin/ dir. Runs on `bohay module install` (a
# managed checkout); `bohay module link` skips the build step, so for a local checkout build
# from source with `cargo build --release` and copy target/release/herdr-reviewr to ./bin/.
#
# The build runs with the module checkout as the working directory, so we resolve the module
# root from this script's location rather than $BOHAY_MODULE_ROOT (build commands may not
# receive the runtime env). At runtime the pane command reads $BOHAY_MODULE_ROOT/bin/herdr-reviewr.
set -euo pipefail

NAME="herdr-reviewr"
REPO="persiyanov/herdr-reviewr"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="$ROOT/bin"

# The release tag matches the manifest version, so a checkout always pulls its own release.
VERSION="$(grep -m1 '^version' "$ROOT/bohay-module.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
TAG="v${VERSION}"

# Map the running platform to the release target triple.
os="$(uname -s)"
arch="$(uname -m)"
case "$os-$arch" in
  Darwin-arm64)                target="aarch64-apple-darwin" ;;
  Darwin-x86_64)               target="x86_64-apple-darwin" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-musl" ;;
  Linux-x86_64)                target="x86_64-unknown-linux-musl" ;;
  *)
    echo "$NAME: no prebuilt binary for $os-$arch, build from source with 'cargo build --release'" >&2
    exit 1
    ;;
esac

archive="${NAME}-${target}.tar.gz"
# taiki-e's checksum sidecar drops the archive extension: <name>-<target>.sha256, not <archive>.sha256.
checksum="${NAME}-${target}.sha256"
base="https://github.com/${REPO}/releases/download/${TAG}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Release-asset downloads are eventually-consistent: GitHub's CDN can 404 for a few minutes
# after a release publishes. Retry (incl. on 404) so an install right after a release doesn't
# fail spuriously.
dl() { curl -fsSL --retry 5 --retry-delay 3 --retry-all-errors --retry-connrefused "$1" -o "$2"; }

echo "$NAME: downloading $archive ($TAG)"
dl "$base/$archive" "$tmp/$archive"
dl "$base/$checksum" "$tmp/$checksum"

echo "$NAME: verifying checksum"
expected="$(awk '{print $1}' "$tmp/$checksum")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
fi
if [ "$expected" != "$actual" ]; then
  echo "$NAME: checksum mismatch (expected $expected, got $actual)" >&2
  exit 1
fi

mkdir -p "$BIN_DIR"
tar -xzf "$tmp/$archive" -C "$tmp"
install -m 0755 "$tmp/$NAME" "$BIN_DIR/$NAME"
echo "$NAME: installed $BIN_DIR/$NAME"

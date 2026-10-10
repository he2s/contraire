#!/bin/sh
# Install the contraire binary (dashboard server and client in one file).
#
#   curl -fsSL https://raw.githubusercontent.com/he2s/contraire/main/install.sh | sh
#
# Downloads the latest release for this machine, verifies its SHA-256 against the
# release's SHA256SUMS, and installs it to ~/.local/bin (override with CONTRAIRE_BIN_DIR).
# CONTRAIRE_VERSION=v0.1.0 pins a version; CONTRAIRE_DOWNLOAD_URL points at a mirror.
set -eu

REPO="${CONTRAIRE_REPO:-he2s/contraire}"
VERSION="${CONTRAIRE_VERSION:-latest}"
BIN_DIR="${CONTRAIRE_BIN_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() { printf 'contraire install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) ;;
  Darwin) fail "no macOS build yet; use: cargo install --git https://github.com/$REPO" ;;
  *) fail "unsupported system $(uname -s); on Windows use install.ps1" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  armv7* | armv8l | armhf) arch=armv7 ;;
  *) fail "unsupported CPU $(uname -m)" ;;
esac
asset="contraire-linux-$arch"

if [ -n "${CONTRAIRE_DOWNLOAD_URL:-}" ]; then
  base="${CONTRAIRE_DOWNLOAD_URL%/}"          # a mirror or a local test server
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  fail "needs curl or wget"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM
say "Downloading $asset ($VERSION) …"
fetch "$base/$asset" "$tmp/$asset" || fail "download failed: $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "download failed: $base/SHA256SUMS"

expected="$(awk -v f="$asset" '$2 == f || $2 == "*"f { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || fail "$asset is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
else
  actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
fi
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset (expected $expected, got $actual)"
say "Checksum verified."

mkdir -p "$BIN_DIR"
install -m 0755 "$tmp/$asset" "$BIN_DIR/contraire" 2>/dev/null || { cp "$tmp/$asset" "$BIN_DIR/contraire" && chmod 0755 "$BIN_DIR/contraire"; }
say "Installed $("$BIN_DIR/contraire" --version) to $BIN_DIR/contraire"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "Note: $BIN_DIR is not on your PATH. Add this to ~/.bashrc or ~/.zshrc:"
     say "  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
say ""
say "Next:"
say "  contraire                     # run the dashboard on this machine (port 8083)"
say "  contraire ps --help           # or control another one from here"

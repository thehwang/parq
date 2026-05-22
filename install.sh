#!/usr/bin/env sh
# pq installer — auto-detects platform, downloads the right tarball from the
# latest GitHub Release, and installs to ~/.local/bin (override with PQ_INSTALL_DIR).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/thehwang/parq/main/install.sh | bash
#
# Override target:                                PQ_INSTALL_DIR=/usr/local/bin sh install.sh
# Pin a specific version (default = latest):      PQ_VERSION=v0.4.0 sh install.sh

set -eu

REPO="thehwang/parq"
INSTALL_DIR="${PQ_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${PQ_VERSION:-latest}"

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Map (os, arch) → release asset name. Windows is intentionally excluded
# because shells differ (PowerShell vs Git Bash); Windows users grab the .zip
# from the Releases page directly.
case "$OS-$ARCH" in
  darwin-arm64)            ASSET="pq-aarch64-apple-darwin.tar.gz" ;;
  darwin-x86_64)           ASSET="pq-x86_64-apple-darwin.tar.gz" ;;
  linux-x86_64|linux-amd64) ASSET="pq-x86_64-unknown-linux-musl.tar.gz" ;;
  *)
    echo "no prebuilt binary for $OS-$ARCH" >&2
    echo "fallback: cargo install pq" >&2
    exit 1
    ;;
esac

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
else
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
fi

mkdir -p "$INSTALL_DIR"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "→ downloading $URL"
if ! curl -fsSL "$URL" -o "$TMP/$ASSET"; then
  echo "download failed: $URL" >&2
  exit 1
fi

# Best-effort sha256 verification when the sidecar exists alongside the asset.
if curl -fsSL "${URL}.sha256" -o "$TMP/${ASSET}.sha256" 2>/dev/null; then
  EXPECTED=$(awk '{print $1}' "$TMP/${ASSET}.sha256")
  if command -v shasum >/dev/null 2>&1; then
    ACTUAL=$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')
  elif command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "$TMP/$ASSET" | awk '{print $1}')
  else
    ACTUAL=""
  fi
  if [ -n "$ACTUAL" ] && [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "sha256 mismatch — expected $EXPECTED, got $ACTUAL" >&2
    exit 1
  fi
  [ -n "$ACTUAL" ] && echo "✓ sha256 verified"
fi

tar -xzf "$TMP/$ASSET" -C "$TMP"
mv -f "$TMP/pq" "$INSTALL_DIR/pq"
chmod +x "$INSTALL_DIR/pq"

echo "✓ installed pq → $INSTALL_DIR/pq"
"$INSTALL_DIR/pq" --version

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo
    echo "⚠  $INSTALL_DIR is not in your PATH. Add this to your shell rc:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

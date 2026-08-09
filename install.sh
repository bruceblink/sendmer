#!/usr/bin/env bash
set -euo pipefail

REPO="bruceblink/sendmer"
BIN="sendmer"

INSTALL_DIR="${HOME}/.sendmer/bin"
VERSION="${SENDMER_VERSION:-}"

echo "📦 Installing sendmer..."

# ----------------------------
# Detect OS
# ----------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux*|linux*)
    OS_TAG="unknown-linux-musl"   # <- 这里改成 musl
    ;;
  Darwin*|darwin*)
    OS_TAG="apple-darwin"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    OS_TAG="pc-windows-msvc"
    ;;
  *)
    echo "❌ Unsupported OS: $OS"
    exit 1
    ;;
esac

# ----------------------------
# Detect ARCH
# ----------------------------
case "$ARCH" in
  x86_64|amd64)
    ARCH_TAG="x86_64"
    ;;
  arm64|aarch64)
    ARCH_TAG="aarch64"
    ;;
  *)
    echo "❌ Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

# ----------------------------
# Fetch latest version if not specified
# ----------------------------
if [ -z "$VERSION" ]; then
  echo "🔍 Fetching latest release..."
  VERSION="$(curl -fsSL \
    -H "Accept: application/vnd.github+json" \
    https://api.github.com/repos/${REPO}/releases/latest \
    | grep '"tag_name"' \
    | sed -E 's/.*"([^"]+)".*/\1/')"
fi

if [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "❌ Invalid release version: $VERSION"
  exit 1
fi

echo "➡️  Version: $VERSION"
echo "➡️  Target:  $ARCH_TAG-$OS_TAG"

# ----------------------------
# Artifact
# ----------------------------
TARBALL="${BIN}-${VERSION}-${ARCH_TAG}-${OS_TAG}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"

TMP_DIR="$(mktemp -d)"
ARCHIVE="${TMP_DIR}/${TARBALL}"
CHECKSUM="${TMP_DIR}/${TARBALL}.sha256"
trap 'rm -rf -- "$TMP_DIR"' EXIT

echo "⬇️  Downloading $URL"
curl -fL "$URL" -o "$ARCHIVE"
curl -fL "${URL}.sha256" -o "$CHECKSUM"

EXPECTED_SHA256="$(awk 'NF { print $1; exit }' "$CHECKSUM")"
EXPECTED_FILE="$(awk 'NF { print $2; exit }' "$CHECKSUM")"
if [[ ! "$EXPECTED_SHA256" =~ ^[0-9a-fA-F]{64}$ || "$EXPECTED_FILE" != "$TARBALL" ]]; then
  echo "❌ Invalid checksum file for $TARBALL"
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(sha256sum "$ARCHIVE" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(shasum -a 256 "$ARCHIVE" | awk '{ print $1 }')"
else
  echo "❌ No SHA-256 utility found (sha256sum or shasum)"
  exit 1
fi

if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
  echo "❌ Checksum verification failed for $TARBALL"
  exit 1
fi

# ----------------------------
# Install
# ----------------------------
echo "📂 Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

tar -xzf "$ARCHIVE" -C "$INSTALL_DIR"

chmod +x "$INSTALL_DIR/$BIN"

# ----------------------------
# Cleanup is handled by the EXIT trap.
# ----------------------------

# ----------------------------
# PATH hint
# ----------------------------
if ! echo "$PATH" | grep -q "$INSTALL_DIR"; then
  echo ""
  echo "⚠️  $INSTALL_DIR is not in your PATH"
  echo "👉 Add this to your shell config:"
  echo ""
  echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi

echo ""
echo "✅ sendmer $VERSION installed successfully!"
echo "👉 Run:"
echo "   sendmer --help"

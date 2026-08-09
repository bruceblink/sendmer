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
    OS_TAG="unknown-linux-musl"
    ARCHIVE_EXTENSION="tar.gz"
    INSTALLED_BIN="$BIN"
    ;;
  Darwin*|darwin*)
    OS_TAG="apple-darwin"
    ARCHIVE_EXTENSION="tar.gz"
    INSTALLED_BIN="$BIN"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    OS_TAG="pc-windows-msvc"
    ARCHIVE_EXTENSION="zip"
    INSTALLED_BIN="$BIN.exe"
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

if [[ "$OS_TAG" == "pc-windows-msvc" && "$ARCH_TAG" != "x86_64" ]]; then
  echo "❌ Unsupported Windows architecture: $ARCH"
  exit 1
fi

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
ARCHIVE_NAME="${BIN}-${VERSION}-${ARCH_TAG}-${OS_TAG}.${ARCHIVE_EXTENSION}"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE_NAME}"

TMP_DIR="$(mktemp -d)"
ARCHIVE="${TMP_DIR}/${ARCHIVE_NAME}"
CHECKSUM="${TMP_DIR}/${ARCHIVE_NAME}.sha256"
trap 'rm -rf -- "$TMP_DIR"' EXIT

echo "⬇️  Downloading $URL"
curl -fL "$URL" -o "$ARCHIVE"
curl -fL "${URL}.sha256" -o "$CHECKSUM"

EXPECTED_SHA256="$(awk 'NF { print $1; exit }' "$CHECKSUM")"
EXPECTED_FILE="$(awk 'NF { print $2; exit }' "$CHECKSUM")"
if [[ ! "$EXPECTED_SHA256" =~ ^[0-9a-fA-F]{64}$ || "$EXPECTED_FILE" != "$ARCHIVE_NAME" ]]; then
  echo "❌ Invalid checksum file for $ARCHIVE_NAME"
  exit 1
fi
EXPECTED_SHA256="${EXPECTED_SHA256,,}"

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(sha256sum "$ARCHIVE" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(shasum -a 256 "$ARCHIVE" | awk '{ print $1 }')"
else
  echo "❌ No SHA-256 utility found (sha256sum or shasum)"
  exit 1
fi
ACTUAL_SHA256="${ACTUAL_SHA256,,}"

if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
  echo "❌ Checksum verification failed for $ARCHIVE_NAME"
  exit 1
fi

# ----------------------------
# Install
# ----------------------------
echo "📂 Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

if [[ "$ARCHIVE_EXTENSION" == "zip" ]]; then
  if ! command -v unzip >/dev/null 2>&1; then
    echo "❌ unzip is required to install the Windows archive"
    exit 1
  fi
  unzip -q "$ARCHIVE" -d "$INSTALL_DIR"
else
  tar -xzf "$ARCHIVE" -C "$INSTALL_DIR"
  chmod +x "$INSTALL_DIR/$INSTALLED_BIN"
fi

if [ ! -f "$INSTALL_DIR/$INSTALLED_BIN" ]; then
  echo "❌ $INSTALLED_BIN not found after extraction"
  exit 1
fi

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

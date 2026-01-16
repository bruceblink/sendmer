#!/usr/bin/env bash
set -e

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

echo "➡️  Version: $VERSION"
echo "➡️  Target:  $ARCH_TAG-$OS_TAG"

# ----------------------------
# Artifact
# ----------------------------
TARBALL="${BIN}-${VERSION}-${ARCH_TAG}-${OS_TAG}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"

TMP_DIR="$(mktemp -d)"
ARCHIVE="${TMP_DIR}/${TARBALL}"

echo "⬇️  Downloading $URL"
curl -fL "$URL" -o "$ARCHIVE"

# ----------------------------
# Install
# ----------------------------
echo "📂 Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

tar -xzf "$ARCHIVE" -C "$INSTALL_DIR"

chmod +x "$INSTALL_DIR/$BIN"

# ----------------------------
# Cleanup
# ----------------------------
rm -rf "$TMP_DIR"

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
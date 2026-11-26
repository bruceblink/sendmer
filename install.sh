#!/usr/bin/env bash
set -euo pipefail

REPO="bruceblink/sendmer"
BIN="sendmer"

INSTALL_DIR="${SENDMER_INSTALL_DIR:-${HOME}/.sendmer/bin}"
VERSION="${SENDMER_VERSION:-}"
COSIGN_VERSION="${SENDMER_COSIGN_VERSION:-v3.1.4}"
COSIGN_BIN="${SENDMER_COSIGN_BIN:-}"

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

# The release and CI matrices only publish native macOS ARM64 assets.
if [[ "$OS_TAG" == "apple-darwin" && "$ARCH_TAG" != "aarch64" ]]; then
  echo "❌ Unsupported macOS architecture: $ARCH (ARM64 only; use a native Apple Silicon shell)" >&2
  exit 1
fi

# A release must be selected explicitly. This prevents a production install
# from silently following a future /releases/latest version.
if [[ -z "$VERSION" ]]; then
  if [[ "${SENDMER_ALLOW_LATEST:-}" == "1" ]]; then
    echo "🔍 Fetching latest release (explicitly enabled)..."
    VERSION="$(curl -fsSL \
      -H "Accept: application/vnd.github+json" \
      https://api.github.com/repos/${REPO}/releases/latest \
      | sed -nE 's/^[[:space:]]*"tag_name":[[:space:]]*"([^"]+)".*/\1/p' \
      | head -n 1)"
  else
    echo "❌ SENDMER_VERSION is required (for example: v0.11.2)" >&2
    echo "   Set SENDMER_ALLOW_LATEST=1 only when floating latest is intentional." >&2
    exit 2
  fi
fi

if [[ "$OS_TAG" == "pc-windows-msvc" ]]; then
  COSIGN_ASSET="cosign-windows-amd64.exe"
else
  case "${OS_TAG}-${ARCH_TAG}" in
    unknown-linux-musl-x86_64) COSIGN_ASSET="cosign-linux-amd64" ;;
    unknown-linux-musl-aarch64) COSIGN_ASSET="cosign-linux-arm64" ;;
    apple-darwin-aarch64) COSIGN_ASSET="cosign-darwin-arm64" ;;
    *) echo "❌ No pinned Cosign asset for ${ARCH_TAG}-${OS_TAG}" >&2; exit 1 ;;
  esac
fi

# Official Cosign v3.1.4 release checksums. Keep this table in lockstep with
# COSIGN_VERSION when updating the verifier.
case "$COSIGN_ASSET" in
  cosign-linux-amd64)  COSIGN_SHA256="424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa" ;;
  cosign-linux-arm64)  COSIGN_SHA256="ade83b7aac62e131cacfe96d958948b2150cc9ef46a570a2649b8e27dedad2" ;;
  cosign-darwin-arm64) COSIGN_SHA256="9f9b8cd557cf2e5b3df3373a00baa005a86b02391e3e06dd0ce37b3a1533d3a4" ;;
  cosign-windows-amd64.exe) COSIGN_SHA256="cbe953787074611f1334cbf37dc1cf953995bb8c45398eb4fc7bb781d29c1f2a" ;;
  *) echo "❌ No pinned Cosign checksum for $COSIGN_ASSET" >&2; exit 1 ;;
esac

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
DOWNLOAD_BASE_URL="${SENDMER_DOWNLOAD_BASE_URL:-https://github.com/${REPO}/releases/download}"
DOWNLOAD_BASE_URL="${DOWNLOAD_BASE_URL%/}"
URL="${DOWNLOAD_BASE_URL}/${VERSION}/${ARCHIVE_NAME}"

TMP_DIR="$(mktemp -d)"
ARCHIVE="${TMP_DIR}/${ARCHIVE_NAME}"
CHECKSUM="${TMP_DIR}/${ARCHIVE_NAME}.sha256"
SBOM="${TMP_DIR}/${ARCHIVE_NAME}.spdx.json"
ARCHIVE_SIGNATURE="${TMP_DIR}/${ARCHIVE_NAME}.sigstore.json"
SBOM_SIGNATURE="${TMP_DIR}/${ARCHIVE_NAME}.spdx.json.sigstore.json"
PROVENANCE="${TMP_DIR}/${ARCHIVE_NAME}.attestation.json"
trap 'rm -rf -- "$TMP_DIR"' EXIT

sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  else
    echo "❌ No SHA-256 utility found (sha256sum or shasum)" >&2
    return 1
  fi
}

# macOS system Bash 3.2 does not support Bash 4 case conversion.
lowercase_hash() {
  printf '%s' "$1" | LC_ALL=C tr 'A-F' 'a-f'
}

resolve_cosign() {
  if [[ -n "$COSIGN_BIN" ]]; then
    if [[ ! -x "$COSIGN_BIN" ]] && ! command -v "$COSIGN_BIN" >/dev/null 2>&1; then
      echo "❌ SENDMER_COSIGN_BIN does not point to an executable: $COSIGN_BIN" >&2
      return 1
    fi
    return 0
  fi

  if [[ "$COSIGN_VERSION" != "v3.1.4" ]]; then
    echo "❌ Unsupported COSIGN_VERSION=$COSIGN_VERSION; use v3.1.4 or set SENDMER_COSIGN_BIN" >&2
    return 1
  fi

  # Do not silently trust an arbitrary Cosign version from PATH. The pinned
  # bootstrap below is the default. Set SENDMER_COSIGN_BIN explicitly when a
  # separately managed verifier is required.
  if declare -F cosign >/dev/null 2>&1; then
    COSIGN_BIN="cosign"
    return 0
  fi

  local cosign_path="$TMP_DIR/$COSIGN_ASSET"
  echo "🔐 Cosign not found; downloading pinned ${COSIGN_VERSION} verifier..."
  curl -fL "https://github.com/sigstore/cosign/releases/download/${COSIGN_VERSION}/${COSIGN_ASSET}" -o "$cosign_path"
  local actual
  actual="$(sha256_file "$cosign_path")"
  actual="$(lowercase_hash "$actual")" || return 1
  if [[ "$actual" != "$COSIGN_SHA256" ]]; then
    echo "❌ Cosign SHA-256 verification failed" >&2
    return 1
  fi
  chmod +x "$cosign_path"
  COSIGN_BIN="$cosign_path"
}

echo "⬇️  Downloading $URL"
curl -fL "$URL" -o "$ARCHIVE"
curl -fL "${URL}.sha256" -o "$CHECKSUM"
curl -fL "${URL}.spdx.json" -o "$SBOM"
curl -fL "${URL}.sigstore.json" -o "$ARCHIVE_SIGNATURE"
curl -fL "${URL}.spdx.json.sigstore.json" -o "$SBOM_SIGNATURE"
curl -fL "${URL}.attestation.json" -o "$PROVENANCE"

EXPECTED_SHA256="$(awk 'NF { print $1; exit }' "$CHECKSUM")"
EXPECTED_FILE="$(awk 'NF { print $2; exit }' "$CHECKSUM")"
if [[ ! "$EXPECTED_SHA256" =~ ^[0-9a-fA-F]{64}$ || "$EXPECTED_FILE" != "$ARCHIVE_NAME" ]]; then
  echo "❌ Invalid checksum file for $ARCHIVE_NAME"
  exit 1
fi
EXPECTED_SHA256="$(lowercase_hash "$EXPECTED_SHA256")" || exit 1

ACTUAL_SHA256="$(sha256_file "$ARCHIVE")"
ACTUAL_SHA256="$(lowercase_hash "$ACTUAL_SHA256")" || exit 1

if [[ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]]; then
  echo "❌ Checksum verification failed for $ARCHIVE_NAME"
  exit 1
fi

# Resolve the pinned verifier after the archive checksum has passed.
resolve_cosign

# Verify both signed blobs and the SLSA provenance before extraction so an
# untrusted release cannot reach the install directory.
ESCAPED_VERSION="${VERSION//./\\.}"
ESCAPED_VERSION="${ESCAPED_VERSION//+/\\+}"
IDENTITY_REGEX="^https://github[.]com/${REPO}/[.]github/workflows/release[.]yml@refs/tags/${ESCAPED_VERSION}$"

verify_sigstore_blob() {
  local blob="$1"
  local bundle="$2"
  "$COSIGN_BIN" verify-blob "$blob" \
    --bundle "$bundle" \
    --certificate-identity-regexp "$IDENTITY_REGEX" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
}

verify_provenance() {
  "$COSIGN_BIN" verify-blob-attestation "$ARCHIVE" \
    --bundle "$PROVENANCE" \
    --certificate-identity-regexp "$IDENTITY_REGEX" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    --certificate-github-workflow-repository "$REPO" \
    --certificate-github-workflow-ref "refs/tags/$VERSION" \
    --certificate-github-workflow-name release \
    --type https://slsa.dev/provenance/v1
}

for trust_file in "$SBOM" "$ARCHIVE_SIGNATURE" "$SBOM_SIGNATURE" "$PROVENANCE"; do
  if [[ ! -s "$trust_file" ]]; then
    echo "❌ Release trust material is empty: $(basename "$trust_file")"
    exit 1
  fi
done

if ! verify_sigstore_blob "$ARCHIVE" "$ARCHIVE_SIGNATURE"; then
  echo "❌ Sigstore verification failed for $ARCHIVE_NAME"
  exit 1
fi
if ! verify_sigstore_blob "$SBOM" "$SBOM_SIGNATURE"; then
  echo "❌ Sigstore verification failed for $(basename "$SBOM")"
  exit 1
fi
if ! verify_provenance; then
  echo "❌ Provenance verification failed for $ARCHIVE_NAME"
  exit 1
fi

# ----------------------------
# Install into a staging directory, then atomically replace the installed
# files. This avoids exposing a partially extracted binary on interruption.
# ----------------------------
INSTALL_PARENT="$(dirname "$INSTALL_DIR")"
mkdir -p "$INSTALL_PARENT"
STAGING_DIR="$(mktemp -d "${INSTALL_PARENT}/.sendmer-install.XXXXXX")"
METADATA_TMP=""
BIN_TMP=""
cleanup_install() {
  rm -rf -- "$STAGING_DIR"
  [[ -z "$METADATA_TMP" ]] || rm -f -- "$METADATA_TMP"
  [[ -z "$BIN_TMP" ]] || rm -f -- "$BIN_TMP"
}
trap 'cleanup_install; rm -rf -- "$TMP_DIR"' EXIT

if [[ "$ARCHIVE_EXTENSION" == "zip" ]]; then
  if ! command -v unzip >/dev/null 2>&1; then
    echo "❌ unzip is required to install the Windows archive"
    exit 1
  fi
  unzip -q "$ARCHIVE" -d "$STAGING_DIR"
else
  tar -xzf "$ARCHIVE" -C "$STAGING_DIR"
fi

if [[ ! -f "$STAGING_DIR/$INSTALLED_BIN" ]]; then
  echo "❌ $INSTALLED_BIN not found after extraction"
  exit 1
fi
if [[ "$ARCHIVE_EXTENSION" != "zip" ]]; then
  chmod +x "$STAGING_DIR/$INSTALLED_BIN"
fi

mkdir -p "$INSTALL_DIR"
BIN_TMP="$INSTALL_DIR/.${INSTALLED_BIN}.tmp.$$"
if [[ -f "$STAGING_DIR/BUILD-METADATA.txt" ]]; then
  METADATA_TMP="$INSTALL_DIR/.BUILD-METADATA.txt.tmp.$$"
  cp "$STAGING_DIR/BUILD-METADATA.txt" "$METADATA_TMP"
  mv -f "$METADATA_TMP" "$INSTALL_DIR/BUILD-METADATA.txt"
fi
cp "$STAGING_DIR/$INSTALLED_BIN" "$BIN_TMP"
if [[ "$ARCHIVE_EXTENSION" != "zip" ]]; then
  chmod +x "$BIN_TMP"
fi
mv -f "$BIN_TMP" "$INSTALL_DIR/$INSTALLED_BIN"

echo "📂 Installing to $INSTALL_DIR"

# ----------------------------
# Cleanup is handled by the EXIT trap.
# ----------------------------

# ----------------------------
# PATH hint
# ----------------------------
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
  echo ""
  echo "⚠️  $INSTALL_DIR is not in your PATH"
  echo "👉 Add this to your shell config:"
  echo ""
  echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
  ;;
esac

echo ""
echo "✅ sendmer $VERSION installed successfully!"
echo "👉 Run:"
echo "   sendmer --help"

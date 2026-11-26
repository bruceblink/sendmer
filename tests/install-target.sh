#!/usr/bin/env bash
set -euo pipefail

TEST_ROOT="$(mktemp -d)"
ARCHIVE_RECORD="$TEST_ROOT/archive-name"
trap 'rm -rf -- "$TEST_ROOT"' EXIT

export HOME="$TEST_ROOT/home"
export SENDMER_VERSION="v0.5.0"
export SENDMER_COSIGN_BIN="cosign"
export ARCHIVE_RECORD
UPPERCASE_HASH='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
LOWERCASE_HASH="$(printf '%s' "$UPPERCASE_HASH" | LC_ALL=C tr 'A-F' 'a-f')"

uname() {
  case "$1" in
    -s) printf '%s\n' 'MINGW64_NT-10.0' ;;
    -m) printf '%s\n' 'x86_64' ;;
  esac
}

# Mimic the archive and checksum downloads without contacting GitHub.
curl() {
  local output=""
  while (( $# > 0 )); do
    case "$1" in
      -o)
        output="$2"
        shift 2
        ;;
      *) shift ;;
    esac
  done

  [[ -n "$output" ]]
  if [[ "$output" == *.sha256 ]]; then
    printf '%s  %s\n' "$UPPERCASE_HASH" "$(basename "${output%.sha256}")" > "$output"
  elif [[ "$output" == *.spdx.json || "$output" == *.sigstore.json || "$output" == *.attestation.json ]]; then
    printf '{}\n' > "$output"
  else
    printf 'fixture archive' > "$output"
  fi
}

sha256sum() {
  printf '%s  %s\n' "$LOWERCASE_HASH" "$1"
}

cosign() {
  [[ "$1" == "verify-blob" || "$1" == "verify-blob-attestation" ]]
  [[ -s "$2" ]]
  [[ "$3" == "--bundle" ]]
  [[ -s "$4" ]]
}

# Record the selected archive and create the executable expected after extraction.
unzip() {
  [[ "$1" == "-q" ]]
  local archive="$2"
  [[ "$3" == "-d" ]]
  local destination="$4"

  printf '%s\n' "$(basename "$archive")" > "$ARCHIVE_RECORD"
  mkdir -p "$destination"
  : > "$destination/sendmer.exe"
  printf 'sendmer_version=%s\nrust_target=x86_64-pc-windows-msvc\nsource_commit=test\n' "$SENDMER_VERSION" > "$destination/BUILD-METADATA.txt"
}

# Source in a subshell so the installer owns its own cleanup trap.
( source ./install.sh )

expected_archive='sendmer-v0.5.0-x86_64-pc-windows-msvc.zip'
[[ "$(<"$ARCHIVE_RECORD")" == "$expected_archive" ]]
[[ -f "$HOME/.sendmer/bin/sendmer.exe" ]]
[[ -f "$HOME/.sendmer/bin/BUILD-METADATA.txt" ]]

if missing_version_output=$( (
  unset SENDMER_VERSION SENDMER_ALLOW_LATEST
  source ./install.sh
) 2>&1); then
  echo "installer should require an explicit release version" >&2
  exit 1
fi
[[ "$missing_version_output" == *"SENDMER_VERSION is required"* ]]

FAIL_HOME="$TEST_ROOT/failing-home"
if failure_output=$( (
  export HOME="$FAIL_HOME"
  export SENDMER_COSIGN_BIN="cosign"
  cosign() {
    if [[ "$1" == "verify-blob" || "$1" == "verify-blob-attestation" ]]; then
      echo 'simulated signature verification failure' >&2
    fi
    return 1
  }
  source ./install.sh
) 2>&1); then
  echo "installer should fail when signature verification fails" >&2
  exit 1
fi
[[ "$failure_output" == *"simulated signature verification failure"* ]]
[[ ! -f "$FAIL_HOME/.sendmer/bin/sendmer.exe" ]]

if unsupported_output=$( (
  uname() {
    case "$1" in
      -s) printf '%s\n' 'MINGW64_NT-10.0' ;;
      -m) printf '%s\n' 'aarch64' ;;
    esac
  }
  curl() {
    echo "unsupported architecture should not download an asset" >&2
    return 1
  }
  source ./install.sh
) 2>&1); then
  echo "Windows ARM64 should be rejected before installation" >&2
  exit 1
fi

[[ "$unsupported_output" == *"Unsupported Windows architecture"* ]]
[[ "$unsupported_output" != *"unsupported architecture should not download an asset"* ]]

# Exercise both ARM64 uname spellings with isolated install roots and the same
# checksum/signature/provenance path used by the Windows fixture.
for arch in arm64 aarch64; do
  (
    export HOME="$TEST_ROOT/macos-$arch"
    export ARCHIVE_RECORD="$TEST_ROOT/archive-$arch"
    VERIFY_RECORD="$TEST_ROOT/verify-$arch"
    uname() {
      case "$1" in
        -s) printf '%s\n' Darwin ;;
        -m) printf '%s\n' "$arch" ;;
      esac
    }
    cosign() {
      [[ "$1" == "verify-blob" || "$1" == "verify-blob-attestation" ]]
      [[ -s "$2" && "$3" == "--bundle" && -s "$4" ]]
      printf '%s %s\n' "$1" "$(basename "$2")" >> "$VERIFY_RECORD"
    }
    tar() {
      [[ "$1" == "-xzf" && "$3" == "-C" ]]
      local archive="$2"
      local destination="$4"
      printf '%s\n' "$(basename "$archive")" > "$ARCHIVE_RECORD"
      mkdir -p "$destination"
      : > "$destination/sendmer"
      printf 'sendmer_version=%s\nrust_target=aarch64-apple-darwin\nsource_commit=test\n' "$SENDMER_VERSION" > "$destination/BUILD-METADATA.txt"
    }
    source ./install.sh
    expected_archive='sendmer-v0.5.0-aarch64-apple-darwin.tar.gz'
    [[ "$(<"$ARCHIVE_RECORD")" == "$expected_archive" ]]
    [[ -x "$HOME/.sendmer/bin/sendmer" ]]
    [[ -f "$HOME/.sendmer/bin/BUILD-METADATA.txt" ]]
    [[ "$COSIGN_ASSET" == cosign-darwin-arm64 ]]
    [[ "$(cat "$VERIFY_RECORD")" == "$(printf 'verify-blob %s\nverify-blob %s.spdx.json\nverify-blob-attestation %s' "$expected_archive" "$expected_archive" "$expected_archive")" ]]
  )
done

# Intel/Rosetta targets fail before downloading even the floating latest tag.
for arch in x86_64 amd64; do
  for version_mode in pinned latest; do
    INTEL_HOME="$TEST_ROOT/intel-$arch-$version_mode"
    DOWNLOAD_RECORD="$TEST_ROOT/download-$arch-$version_mode"
    if intel_output=$( (
      export HOME="$INTEL_HOME"
      if [[ "$version_mode" == latest ]]; then
        unset SENDMER_VERSION
        export SENDMER_ALLOW_LATEST=1
      fi
      uname() {
        case "$1" in
          -s) printf '%s\n' Darwin ;;
          -m) printf '%s\n' "$arch" ;;
        esac
      }
      curl() {
        : > "$DOWNLOAD_RECORD"
        return 1
      }
      source ./install.sh
    ) 2>&1); then
      echo "macOS Intel should be rejected before installation" >&2
      exit 1
    fi
    [[ "$intel_output" == *"Unsupported macOS architecture: $arch"* ]]
    [[ ! -e "$DOWNLOAD_RECORD" ]]
    [[ ! -e "$INTEL_HOME/.sendmer" ]]
  done
done
echo "Installer target checks passed (Windows x86_64, macOS ARM64, unsupported architectures)."

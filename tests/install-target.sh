#!/usr/bin/env bash
set -euo pipefail

TEST_ROOT="$(mktemp -d)"
ARCHIVE_RECORD="$TEST_ROOT/archive-name"
trap 'rm -rf -- "$TEST_ROOT"' EXIT

export HOME="$TEST_ROOT/home"
export SENDMER_VERSION="v0.5.0"
export ARCHIVE_RECORD
UPPERCASE_HASH='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
LOWERCASE_HASH="${UPPERCASE_HASH,,}"

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
  else
    printf 'fixture archive' > "$output"
  fi
}

sha256sum() {
  printf '%s  %s\n' "$LOWERCASE_HASH" "$1"
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
}

# Source the installer so shell functions above replace platform and network commands.
source ./install.sh

expected_archive='sendmer-v0.5.0-x86_64-pc-windows-msvc.zip'
[[ "$(<"$ARCHIVE_RECORD")" == "$expected_archive" ]]
[[ -f "$HOME/.sendmer/bin/sendmer.exe" ]]

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

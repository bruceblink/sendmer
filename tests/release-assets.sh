#!/usr/bin/env bash
set -euo pipefail

TEST_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT

# Build one representative release group using the exact names consumed by the installers.
write_fixture_group() {
  local root="$1"
  local version="$2"
  local target="$3"
  local extension="$4"
  local archive_name="sendmer-${version}-${target}.${extension}"

  mkdir -p "$root"
  printf 'archive fixture\n' > "$root/$archive_name"
  printf '%064d  %s\n' 0 "$archive_name" > "$root/$archive_name.sha256"
  printf '{"spdxVersion":"SPDX-2.3"}\n' > "$root/$archive_name.spdx.json"
  printf '{"mediaType":"application/vnd.dev.sigstore.bundle+json"}\n' > "$root/$archive_name.sigstore.json"
  printf '{"mediaType":"application/vnd.dev.sigstore.bundle+json"}\n' > "$root/$archive_name.spdx.json.sigstore.json"
  printf '{"dsseEnvelope":{}}\n' > "$root/$archive_name.attestation.json"
}

# Validate that one target has every required, non-empty asset and a checksum naming the archive.
validate_fixture_group() {
  local root="$1"
  local version="$2"
  local target="$3"
  local extension="$4"
  local archive_name="sendmer-${version}-${target}.${extension}"
  local archive="$root/$archive_name"
  local checksum="$archive.sha256"
  local sbom="$archive.spdx.json"
  local archive_signature="$archive.sigstore.json"
  local sbom_signature="$archive.spdx.json.sigstore.json"
  local provenance="$archive.attestation.json"
  local checksum_hash
  local checksum_asset

  for required in "$archive" "$checksum" "$sbom" "$archive_signature" "$sbom_signature" "$provenance"; do
    if [ ! -s "$required" ]; then
      return 1
    fi
  done

  checksum_hash="$(awk 'NF { print $1; exit }' "$checksum")"
  checksum_asset="$(awk 'NF { print $2; exit }' "$checksum")"
  [[ "$checksum_hash" =~ ^[0-9a-fA-F]{64}$ ]] || return 1
  test "$checksum_asset" = "$archive_name" || return 1
}

run_expected_matrix() {
  local version="v0.10.0"
  local target
  local extension
  local root

  while IFS='|' read -r target extension; do
    root="$TEST_ROOT/${target//\//_}"
    write_fixture_group "$root" "$version" "$target" "$extension"
    validate_fixture_group "$root" "$version" "$target" "$extension"
  done <<'MATRIX'
x86_64-unknown-linux-musl|tar.gz
aarch64-unknown-linux-musl|tar.gz
x86_64-apple-darwin|tar.gz
aarch64-apple-darwin|tar.gz
x86_64-pc-windows-msvc|zip
x86_64-pc-windows-gnu|zip
MATRIX
}

# Keep the checked-in release matrix aligned with the asset fixture so a target
# cannot silently disappear from the workflow while installer naming stays green.
validate_workflow_matrix() {
  local target
  for target in \
    x86_64-unknown-linux-musl \
    aarch64-unknown-linux-musl \
    x86_64-apple-darwin \
    aarch64-apple-darwin \
    x86_64-pc-windows-msvc \
    x86_64-pc-windows-gnu; do
    grep -Fq "target: $target" .github/workflows/release.yml
  done
}

# Git Bash on Windows rewrites backslash escapes in command arguments. Keep the
# cosign identity pattern free of backslashes so Windows verification matches
# the same certificate identity as Linux and macOS.
validate_windows_safe_identity_regex() {
  grep -Fq 'IDENTITY_REGEX="^https://github[.]com/${GITHUB_REPOSITORY}/[.]github/workflows/release[.]yml@refs/tags/${RELEASE_VERSION}$"' .github/workflows/release.yml
}

# Keep CI's native acceptance matrix aligned with the platform contract. Release
# cross-builds alone cannot prove native filesystem semantics on ARM runners.
validate_ci_native_matrix() {
  local target
  for target in \
    'os: "ubuntu-latest"' \
    'os: "ubuntu-24.04-arm"' \
    'os: "macos-15-intel"' \
    'os: "macos-14"' \
    'toolchain: "x86_64-apple-darwin"' \
    'toolchain: "aarch64-apple-darwin"' \
    'toolchain: "x86_64-pc-windows-msvc"' \
    'toolchain: "x86_64-pc-windows-gnu"'; do
    grep -Fq "$target" .github/workflows/ci.yml
  done
}

expect_rejection() {
  local description="$1"
  shift

  if "$@"; then
    echo "expected rejection: $description" >&2
    exit 1
  fi
}

run_negative_cases() {
  local root="$TEST_ROOT/negative"
  local version="v0.10.0"
  local target="x86_64-unknown-linux-musl"
  local archive_name="sendmer-${version}-${target}.tar.gz"

  write_fixture_group "$root" "$version" "$target" "tar.gz"
  rm "$root/$archive_name.spdx.json"
  expect_rejection "missing SBOM" validate_fixture_group "$root" "$version" "$target" "tar.gz"

  write_fixture_group "$root" "$version" "$target" "tar.gz"
  printf '%064d  wrong-name.tar.gz\n' 0 > "$root/$archive_name.sha256"
  expect_rejection "checksum naming a different archive" validate_fixture_group "$root" "$version" "$target" "tar.gz"
}

validate_workflow_matrix
validate_windows_safe_identity_regex
validate_ci_native_matrix
run_expected_matrix
run_negative_cases
echo "release asset contract rehearsal passed"

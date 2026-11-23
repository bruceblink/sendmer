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
  grep -Fq 'native release binary version smoke' .github/workflows/ci.yml
}

# Git Bash on Windows rewrites backslash escapes in command arguments. Keep the
# cosign identity pattern free of backslashes so Windows verification matches
# the same certificate identity as Linux and macOS.
validate_windows_safe_identity_regex() {
  grep -Fq 'IDENTITY_REGEX="^https://github[.]com/${GITHUB_REPOSITORY}/[.]github/workflows/release[.]yml@refs/tags/${RELEASE_VERSION}$"' .github/workflows/release.yml
}

validate_sbom_integrity_contract() {
  test -f scripts/validate-release-sbom.py
  grep -Fq 'SYFT_FILE_METADATA_SELECTION: all' .github/workflows/release.yml
  grep -Fq 'SYFT_FILE_METADATA_DIGESTS: sha256,sha1' .github/workflows/release.yml
  grep -Fq 'scripts/validate-release-sbom.py' .github/workflows/release.yml
  grep -Fq 'Enrich and validate target SBOM' .github/workflows/release.yml
  grep -Fq 'scripts/validate-release-sbom.py' .github/workflows/release.yml
  grep -Fq 'hashlib.sha256(members[name]).hexdigest()' scripts/validate-release-sbom.py
  grep -Fq 'hashlib.sha1(members[name]).hexdigest()' scripts/validate-release-sbom.py
  grep -Fq 'root["checksums"]' scripts/validate-release-sbom.py
}

# Exercise the SBOM validator with the same two files that a Unix release
# archive contains. This catches regressions where the workflow calls the
# script but the script does not actually enrich the emitted SPDX document.
validate_sbom_enrichment() {
  python3 - "$TEST_ROOT" <<'PY'
import hashlib
import json
import pathlib
import subprocess
import sys
import tarfile

root = pathlib.Path(sys.argv[1]) / "sbom-enrichment"
payload = root / "payload"
payload.mkdir(parents=True)
(payload / "sendmer").write_bytes(b"fixture-binary\n")
(payload / "BUILD-METADATA.txt").write_text(
    "sendmer_version=v0.10.0\n"
    "rust_target=x86_64-unknown-linux-musl\n"
    "source_commit=" + "0" * 40 + "\n",
    encoding="utf-8",
)

archive = root / "sendmer-v0.10.0-x86_64-unknown-linux-musl.tar.gz"
with tarfile.open(archive, "w:gz") as output:
    output.add(payload / "sendmer", arcname="sendmer")
    output.add(payload / "BUILD-METADATA.txt", arcname="BUILD-METADATA.txt")
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
checksum = archive.with_name(archive.name + ".sha256")
checksum.write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
sbom = archive.with_name(archive.name + ".spdx.json")
sbom.write_text(
    json.dumps(
        {
            "spdxVersion": "SPDX-2.3",
            "SPDXID": "SPDXRef-DOCUMENT",
            "name": "sendmer",
            "documentNamespace": "https://example.test/sendmer",
            "creationInfo": {"created": "2026-11-23T00:00:00Z", "creators": ["Tool: fixture"]},
            "packages": [
                {
                    "SPDXID": "SPDXRef-Package",
                    "name": "sendmer",
                    "downloadLocation": "NOASSERTION",
                    "filesAnalyzed": False,
                }
            ],
            "files": [],
            "relationships": [
                {
                    "spdxElementId": "SPDXRef-DOCUMENT",
                    "relatedSpdxElement": "SPDXRef-Package",
                    "relationshipType": "DESCRIBES",
                }
            ],
        }
    ),
    encoding="utf-8",
)
subprocess.run(
    [
        sys.executable,
        "scripts/validate-release-sbom.py",
        str(archive),
        str(checksum),
        str(sbom),
        "--repository",
        "bruceblink/sendmer",
        "--version",
        "v0.10.0",
    ],
    check=True,
)
document = json.loads(sbom.read_text(encoding="utf-8"))
assert {item["fileName"] for item in document["files"]} == {
    "sendmer",
    "BUILD-METADATA.txt",
}
for item in document["files"]:
    assert {entry["algorithm"] for entry in item["checksums"]} == {"SHA1", "SHA256"}
root_package = next(package for package in document["packages"] if package["SPDXID"] == "SPDXRef-Package")
assert root_package["filesAnalyzed"] is True
assert root_package["checksums"] == [{"algorithm": "SHA256", "checksumValue": digest}]
PY
}

validate_immutable_release_contract() {
  grep -Fq 'cancel-in-progress: false' .github/workflows/release.yml
  grep -Fq 'Require repository immutable releases' .github/workflows/release.yml
  grep -Fq 'RELEASE_ADMIN_TOKEN' .github/workflows/release.yml
  grep -Fq 'gh release create "$VERSION"' .github/workflows/release.yml
  grep -Fq -- '--draft' .github/workflows/release.yml
  grep -Fq 'Publish immutable GitHub Release' .github/workflows/release.yml
  grep -Fq 'gh release edit "$RELEASE_VERSION" --draft=false' .github/workflows/release.yml
  grep -Fq 'Existing release asset has a different digest' .github/workflows/release.yml
  grep -Fq 'All six target groups and trust assets are present.' .github/workflows/release.yml
  grep -Fq '.immutable' .github/workflows/release.yml
  if grep -Fq -- '--clobber' .github/workflows/release.yml; then
    echo 'release uploads must not use --clobber' >&2
    return 1
  fi
}

# Keep CI's native acceptance matrix aligned with the platform contract. Release
# cross-builds alone cannot prove native filesystem semantics on ARM runners.
validate_ci_native_matrix() {
  local target
  for target in \
    'os: "ubuntu-latest"' \
    'os: "ubuntu-24.04-arm"' \
    'os: "macos-15-intel"' \
    'os: "macos-15"' \
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
validate_sbom_integrity_contract
validate_sbom_enrichment
validate_immutable_release_contract
validate_ci_native_matrix
run_expected_matrix
run_negative_cases
echo "release asset contract rehearsal passed"

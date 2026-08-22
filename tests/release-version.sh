#!/usr/bin/env bash
set -euo pipefail

VERSION_PATTERN='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
VALID_VERSION='v1.2.3-preview.1+build.7'
INVALID_VERSIONS=('v1.2.3.rc1' 'v1.2.3-' '1.2.3')

[[ "$VALID_VERSION" =~ $VERSION_PATTERN ]]
for version in "${INVALID_VERSIONS[@]}"; do
  ! [[ "$version" =~ $VERSION_PATTERN ]]
done

# Keep the package, lockfile, and both README install examples on one version.
CRATE_VERSION="$(python3 -c 'import pathlib, tomllib; manifest = tomllib.loads(pathlib.Path("Cargo.toml").read_text(encoding="utf-8")); lock = tomllib.loads(pathlib.Path("Cargo.lock").read_text(encoding="utf-8")); expected = manifest["package"]["version"]; actual = next(package["version"] for package in lock["package"] if package["name"] == "sendmer"); assert actual == expected, (expected, actual); print(expected)')"
EXPECTED_TAG="v${CRATE_VERSION}"
for readme in README.md README_ZH.md; do
  grep -Fq "SENDMER_VERSION=\"${EXPECTED_TAG}\"" "$readme"
  grep -Fq "SENDMER_VERSION=${EXPECTED_TAG}" "$readme"
done

# Keep the release workflow and Unix installer on the same validation contract.
grep -Fq "$VERSION_PATTERN" .github/workflows/release.yml
grep -Fq "$VERSION_PATTERN" install.sh

# Keep the release trust-material contract fail-closed and reviewable.
grep -Fq 'anchore/sbom-action@v0' .github/workflows/release.yml
grep -Fq 'sigstore/cosign-installer@v4' .github/workflows/release.yml
grep -Fq 'actions/attest@v4' .github/workflows/release.yml
grep -Fq 'artifact-metadata: write' .github/workflows/release.yml
grep -Fq 'cosign sign-blob' .github/workflows/release.yml
grep -Fq 'cosign verify-blob' .github/workflows/release.yml
grep -Fq 'gh attestation verify' .github/workflows/release.yml
grep -Fq '.attestation.json' .github/workflows/release.yml
grep -Fq '.attestation.json' install.sh
grep -Fq '.attestation.json' install.ps1
grep -Fq 'RELEASE_ASSET_SIGNATURE' .github/workflows/release.yml
grep -Fq 'RELEASE_SBOM_SIGNATURE' .github/workflows/release.yml
grep -Fq 'RELEASE_PROVENANCE' .github/workflows/release.yml
grep -Fq 'echo "sbom=${ASSET}.spdx.json"' .github/workflows/release.yml
grep -Fq 'SBOM="${ASSET}.spdx.json"' .github/workflows/release.yml
grep -Fq 'Manual releases must be dispatched from the matching release tag' .github/workflows/release.yml

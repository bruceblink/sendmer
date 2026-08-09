#!/usr/bin/env bash
set -euo pipefail

VERSION_PATTERN='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
VALID_VERSION='v1.2.3-preview.1+build.7'
INVALID_VERSIONS=('v1.2.3.rc1' 'v1.2.3-' '1.2.3')

[[ "$VALID_VERSION" =~ $VERSION_PATTERN ]]
for version in "${INVALID_VERSIONS[@]}"; do
  ! [[ "$version" =~ $VERSION_PATTERN ]]
done

# Keep the release workflow and Unix installer on the same validation contract.
grep -Fq "$VERSION_PATTERN" .github/workflows/release.yml
grep -Fq "$VERSION_PATTERN" install.sh

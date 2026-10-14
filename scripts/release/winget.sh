#!/usr/bin/env bash
# Writes the winget manifests for a published release, from the templates in
# packaging/winget/template, into
# packaging/winget/manifests/s/StevenPickles/SpaceBadger/<version>/, the
# layout of microsoft/winget-pkgs.
#
#   scripts/release/winget.sh 0.2.0
#
# The checksum comes from the release's SHA256SUMS (see sha256.sh) and the
# release date from CHANGELOG.md. Run from the repository root; prints the
# directory it wrote.
set -euo pipefail

version=${1:?usage: winget.sh X.Y.Z}
if ! [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "version must be X.Y.Z, not '$version'" >&2
    exit 1
fi
repo=${REPO:-stevenpickles/spacebadger}
asset=SpaceBadger-$version-windows-x64.zip
url=https://github.com/$repo/releases/download/v$version/$asset
sha256=$(scripts/release/sha256.sh "$version" "$asset" | tr '[:lower:]' '[:upper:]')
date=$(sed -n "s/^## \[$version\] - \([0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}\)$/\1/p" CHANGELOG.md)
if [ -z "$date" ]; then
    echo "CHANGELOG.md has no dated '## [$version]' section" >&2
    exit 1
fi

out=packaging/winget/manifests/s/StevenPickles/SpaceBadger/$version
mkdir -p "$out"
for template in packaging/winget/template/*.yaml; do
    sed -e "s|@VERSION@|$version|g" -e "s|@URL@|$url|g" \
        -e "s|@SHA256@|$sha256|g" -e "s|@DATE@|$date|g" \
        "$template" > "$out/$(basename "$template")"
done
echo "$out"

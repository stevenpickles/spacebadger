#!/usr/bin/env bash
# Writes the Chocolatey package for a published release, from the template in
# packaging/chocolatey/template, into packaging/chocolatey/spacebadger/.
# The package downloads the Windows zip when installed; nothing is embedded.
#
#   scripts/release/chocolatey.sh 0.2.0
#   choco pack packaging/chocolatey/spacebadger/spacebadger.nuspec
#
# The checksum comes from the release's SHA256SUMS (see sha256.sh). Run from
# the repository root; prints the .nuspec it wrote.
set -euo pipefail

version=${1:?usage: chocolatey.sh X.Y.Z}
if ! [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "version must be X.Y.Z, not '$version'" >&2
    exit 1
fi
repo=${REPO:-stevenpickles/spacebadger}
asset=SpaceBadger-$version-windows-x64.zip
url=https://github.com/$repo/releases/download/v$version/$asset
sha256=$(scripts/release/sha256.sh "$version" "$asset")

template=packaging/chocolatey/template
out=packaging/chocolatey/spacebadger
rm -rf "$out"
for file in spacebadger.nuspec tools/chocolateyinstall.ps1; do
    mkdir -p "$(dirname "$out/$file")"
    sed -e "s|@VERSION@|$version|g" -e "s|@URL@|$url|g" -e "s|@SHA256@|$sha256|g" \
        "$template/$file" > "$out/$file"
done
echo "$out/spacebadger.nuspec"

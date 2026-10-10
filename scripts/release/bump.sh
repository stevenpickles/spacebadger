#!/usr/bin/env bash
# Prepares a release branch: sets the version and turns the changelog's
# Unreleased entries into a dated section for it.
#
#   scripts/release/bump.sh 1.2.3
#
# Run from the repository root on release/v1.2.3, then review and commit.
set -euo pipefail

version=${1:?usage: bump.sh X.Y.Z}
if ! [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "version must be X.Y.Z, not '$version'" >&2
    exit 1
fi
if grep -q "^## \[$version\]" CHANGELOG.md; then
    echo "CHANGELOG.md already has a $version section" >&2
    exit 1
fi
# Entries waiting under Unreleased, without surrounding blank lines.
entries=$(awk '/^## /{inside = ($0 == "## [Unreleased]"); next} inside' CHANGELOG.md |
    sed -e '/./,$!d' -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')
if [ -z "$entries" ]; then
    echo "CHANGELOG.md has nothing under '## [Unreleased]' to release" >&2
    exit 1
fi

sed -i "/^\[workspace.package\]/,/^\[/s/^version = \".*\"$/version = \"$version\"/" Cargo.toml
cargo update --workspace --offline --quiet

# Keep an empty Unreleased heading, then the new section with the entries.
awk -v v="$version" -v d="$(date +%Y-%m-%d)" -v e="$entries" '
    $0 == "## [Unreleased]" { print; print ""; print "## [" v "] - " d; print ""; print e; print ""; skip = 1; next }
    /^## / { skip = 0 }
    !skip
' CHANGELOG.md > CHANGELOG.md.new
mv CHANGELOG.md.new CHANGELOG.md

echo "Version $(scripts/release/version.sh); CHANGELOG.md has a $version section."
echo "Review with git diff, then commit."

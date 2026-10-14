#!/usr/bin/env bash
# Prints the SHA-256 of a published release download, read from that
# release's SHA256SUMS, for the package-manager manifests.
#
#   scripts/release/sha256.sh 0.2.0 SpaceBadger-0.2.0-windows-x64.zip
#
# Set SHA256SUMS to a local copy of the file to skip the download, and REPO
# (owner/name) to read another repository's releases.
set -euo pipefail

version=${1:?usage: sha256.sh VERSION FILE}
file=${2:?usage: sha256.sh VERSION FILE}
repo=${REPO:-stevenpickles/spacebadger}

if [ -n "${SHA256SUMS:-}" ]; then
    sums=$(cat "$SHA256SUMS")
else
    sums=$(curl -fsSL "https://github.com/$repo/releases/download/v$version/SHA256SUMS")
fi
hash=$(printf '%s\n' "$sums" | awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }')
if ! [[ $hash =~ ^[0-9a-f]{64}$ ]]; then
    echo "no SHA-256 for $file in the SHA256SUMS of v$version" >&2
    exit 1
fi
echo "$hash"

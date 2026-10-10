#!/usr/bin/env bash
# Prints the CHANGELOG.md section for a version, used as its release notes.
# Fails when there is none, so a release can't go out undescribed.
#
#   scripts/release/notes.sh 0.1.0
set -euo pipefail

version=${1:?usage: notes.sh VERSION}
notes=$(awk -v v="$version" '
    /^## / { inside = index($0, "## [" v "]") == 1; next }
    inside
' CHANGELOG.md | sed -e '/./,$!d' -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')
if [ -z "$notes" ]; then
    echo "CHANGELOG.md has no '## [$version]' section" >&2
    exit 1
fi
printf '%s\n' "$notes"

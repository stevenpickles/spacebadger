#!/usr/bin/env bash
# Prints the app version from the workspace Cargo.toml, the only place it is
# kept. Run from the repository root.
set -euo pipefail

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
if ! [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "no x.y.z version under [workspace.package] in Cargo.toml (found '$version')" >&2
    exit 1
fi
echo "$version"

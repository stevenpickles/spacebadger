#!/usr/bin/env bash
# Writes dist/THIRD-PARTY-LICENSES.html: the licenses of every Rust crate
# compiled into the app and every npm package bundled into its interface.
# package.sh puts it in each download.
#
# Needs cargo-about, Python 3, and `npm ci`; set CARGO_ABOUT or PYTHON to use
# ones not on PATH. Run from the repository root.
set -euo pipefail

about=${CARGO_ABOUT:-cargo-about}
mkdir -p dist
"$about" generate --locked --manifest-path src-tauri/Cargo.toml \
    -c packaging/about.toml packaging/about.hbs -o dist/notices-rust.html
# A throwaway build of the interface, for its sourcemaps.
npx vite build --sourcemap --outDir ../dist/ui-map --emptyOutDir >/dev/null
"${PYTHON:-python3}" scripts/release/web_notices.py dist/notices-rust.html dist/ui-map dist/THIRD-PARTY-LICENSES.html
rm -rf dist/notices-rust.html dist/ui-map
echo dist/THIRD-PARTY-LICENSES.html

#!/usr/bin/env bash
# Packs a release build into its portable download in dist/.
#
#   scripts/release/package.sh windows|linux|macos
#
# Run from the repository root after `npx tauri build` (with `--no-bundle`
# on Windows). Prints the path of the file it made.
set -euo pipefail

platform=${1:?usage: package.sh windows|linux|macos}
version=$(scripts/release/version.sh)
target=${CARGO_TARGET_DIR:-target}/release
mkdir -p dist

# A folder holding the app and its notices, archived as a whole.
stage() {
    local dir=dist/stage/$1
    rm -rf "$dir"
    mkdir -p "$dir"
    cp LICENSE README.md "$dir/"
    if [ -f dist/THIRD-PARTY-LICENSES.html ]; then
        cp dist/THIRD-PARTY-LICENSES.html "$dir/"
    fi
    echo "$dir"
}

case $platform in
windows)
    name=SpaceBadger-$version-windows-x64
    dir=$(stage "$name")
    cp "$target/spacebadger.exe" "$dir/SpaceBadger.exe"
    rm -f "dist/$name.zip"
    pwsh -NoProfile -Command "Compress-Archive -Path '$dir' -DestinationPath 'dist/$name.zip'"
    echo "dist/$name.zip"
    ;;
linux)
    name=SpaceBadger-$version-linux-x86_64.AppImage
    images=("$target"/bundle/appimage/*.AppImage)
    if [ ${#images[@]} -ne 1 ] || [ ! -f "${images[0]}" ]; then
        echo "expected one AppImage in $target/bundle/appimage" >&2
        exit 1
    fi
    cp "${images[0]}" "dist/$name"
    chmod +x "dist/$name"
    echo "dist/$name"
    ;;
macos)
    name=SpaceBadger-$version-macos-arm64
    dir=$(stage "$name")
    # ditto keeps the bundle's symlinks, permissions and signature.
    ditto "$target/bundle/macos/SpaceBadger.app" "$dir/SpaceBadger.app"
    rm -f "dist/$name.zip"
    ditto -c -k --keepParent "$dir" "dist/$name.zip"
    echo "dist/$name.zip"
    ;;
*)
    echo "unknown platform $platform" >&2
    exit 2
    ;;
esac

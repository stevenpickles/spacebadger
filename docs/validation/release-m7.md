# Milestone 7: portable releases

Date: 2026-10-10. Branch `feature/13/portable-release`.

## Checked locally

| Check | Result |
|---|---|
| `scripts/release/version.sh` | `0.1.0`, read from the workspace `Cargo.toml` |
| `scripts/release/notes.sh 0.1.0` / `9.9.9` | The 0.1.0 changelog section (checked while it was a dated section; it now waits under Unreleased for the release branch) / fails: "CHANGELOG.md has no '## [9.9.9]' section" |
| `scripts/release/bump.sh` on a copy of the repository | `0.1.0`: moved the Unreleased entries into a dated `## [0.1.0]` section and kept an empty Unreleased heading. Again with `0.1.0`: refused, the section exists. `0.2.0` with nothing under Unreleased: refused. `0.2.0` with an entry: set every workspace crate to 0.2.0 in `Cargo.toml` and `Cargo.lock`, and `notes.sh 0.2.0` printed the entry |
| Windows release build (`npx tauri build --no-bundle`) | File properties: SpaceBadger 0.1.0, "Copyright (c) 2026 Steven Pickles. MIT License.", taken from `Cargo.toml` and the bundle settings |
| `package.sh windows` | `SpaceBadger-0.1.0-windows-x64.zip`, 3.0 MB, containing `SpaceBadger.exe`, `LICENSE`, `README.md`, and `THIRD-PARTY-LICENSES.html` |
| Extracted zip on Windows 11 | `SpaceBadger.exe` started from the extracted folder and scanned a folder |
| Notices (`notices.sh`, cargo-about 0.9.2) | 350 Rust crates under MIT, Unicode-3.0, MPL-2.0, Apache-2.0, BSD-3-Clause, and Zlib; npm `svelte` and `@tauri-apps/api` found in the interface's sourcemaps; our own crates left out |
| Linux job in an `ubuntu:22.04` container (same packages and steps as the workflow) | `SpaceBadger-0.1.0-linux-x86_64.AppImage`, 78 MiB. The first attempt failed with a rustc stack overflow compiling `tauri`; with `RUST_MIN_STACK=16777216`, now set in the workflow, it built |
| The AppImage on a plain `ubuntu:24.04` under Xvfb | Needs the system `libEGL` (installed on any desktop). With `libegl1`, it started, scanned a folder, and drew the map |
| `actionlint` (both workflows), `shellcheck` (release scripts) | No findings |

## Not checked yet

- The macOS build and its zip: there's no Mac here. The pull request runs the macOS job in CI, but only the build and packing are exercised, not launching the app. The app is ad-hoc signed and not notarized.
- The release-branch check and the publish job: they run only on a `release/v*` branch and a version tag. Their checks are plain shell, but the first real release is their first run.
- Windows 10.

# Changelog

All notable changes to SpaceBadger are listed here. Versions follow [semantic versioning](https://semver.org/); new entries go under `## [Unreleased]`, and `scripts/release/bump.sh` turns them into a version's section on its release branch. Each `## [x.y.z]` section becomes the notes of that GitHub release.

## [Unreleased]

## [0.1.0] - 2026-10-10

First release, as portable downloads for Windows x64, Linux x86_64 (AppImage), and macOS on Apple Silicon.

- Scans a folder or drive and draws it as a treemap while the scan runs, by allocated or logical size, with depth or file-type colors.
- Accounts for hard links, cloud placeholders (OneDrive), sparse and compressed files, and mount boundaries, and reports what it couldn't read.
- Volume overview for whole-drive scans, including space not attributed to files.
- Filename filter with several `;`-separated patterns, `*`/`?` wildcards, and `!` exclusions, with a ranked result list.
- Small-items lists, details panel, context menu, and "Show in file manager".
- Select several items and move them to the Recycle Bin or Trash, guarded by an "Allow deleting" switch, a typed confirmation, and never deleting permanently.


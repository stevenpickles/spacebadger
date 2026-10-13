# Changelog

All notable changes to SpaceBadger are listed here. Versions follow [semantic versioning](https://semver.org/); new entries go under `## [Unreleased]`, and `scripts/release/bump.sh` turns them into a version's section on its release branch. Each `## [x.y.z]` section becomes the notes of that GitHub release.

## [Unreleased]

- Filter: `is:video`, `is:image`, `is:document`, and the other file-type categories match files by type, and `!is:` leaves a type out. The filter suggests keywords, types, folder names after `dir:`, and extensions after `*.` from the scan as you type.
- Filter: a `dir:` prefix matches folder names, so `dir:cache` finds the files inside folders named like `cache`, and `!dir:node_modules` leaves those folders out. Combined with file patterns, folder patterns narrow where they apply.
- Windows: Restart as administrator from the toolbar (or beside "Permission denied" under Not counted) reopens SpaceBadger elevated and scans the same folder again, after asking; declining leaves it as it was.
- Interface pass: sizes are file sizes throughout (the Allocated/Logical switch is gone; the volume overview still compares on-disk usage).
- Toolbar regrouped, with a filter syntax popover, a scan progress bar, and a keyboard and mouse shortcuts sheet (press `?`).
- Breadcrumbs have an Up button and collapse deep paths. The side panel keeps the details on top at a fixed height and lists the contents of the folder in the map below, largest first, in step with the map's selection. Unchecking "Folders" lists only files, the largest from every level under the folder. Results, small items, and what wasn't counted join it as tabs when open; narrow windows stack the panel under the map.
- Treemap labels keep the size visible and pick a readable color per tile; selection and hover are clearer; the tooltip stays near the pointer and shows the share of the parent folder; the legend is a foldable card in both color modes.
- Result and small-item lists share one keyboard-driven list with range selection, a context menu, and readable folder paths.
- The delete dialog is a true modal that shows full paths and ends on a result screen listing anything that couldn't be moved. Details, menus, and omissions explain themselves in plain words, and failures to load show a Retry.
- The window title shows the version: `SpaceBadger v0.1.0` for a release, `SpaceBadger v0.1.0+dev.<commit>` for a build between releases (`.dirty` when it has uncommitted changes).
- Screen readers hear scan phases, search totals, and the selected item once, instead of every counter update.

## [0.1.0] - 2026-10-10

First release, as portable downloads for Windows x64, Linux x86_64 (AppImage), and macOS on Apple Silicon.

- Scans a folder or drive and draws it as a treemap while the scan runs, by allocated or logical size, with depth or file-type colors.
- Accounts for hard links, cloud placeholders (OneDrive), sparse and compressed files, and mount boundaries, and reports what it couldn't read.
- Volume overview for whole-drive scans, including space not attributed to files.
- Filename filter with several `;`-separated patterns, `*`/`?` wildcards, and `!` exclusions, with a ranked result list.
- Small-items lists, details panel, context menu, and "Show in file manager".
- Select several items and move them to the Recycle Bin or Trash, guarded by an "Allow deleting" switch, a typed confirmation, and never deleting permanently.


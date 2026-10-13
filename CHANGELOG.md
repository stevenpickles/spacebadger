# Changelog

All notable changes to SpaceBadger are listed here. Versions follow [semantic versioning](https://semver.org/); new entries go under `## [Unreleased]`, and `scripts/release/bump.sh` turns them into a version's section on its release branch. Each `## [x.y.z]` section becomes the notes of that GitHub release.

## [Unreleased]

## [0.2.0] - 2026-10-13

A redesigned interface, a much more capable filter, and sizes in familiar units.

### Filter
- Folder patterns: `dir:cache` finds the files inside folders named like `cache`, at any depth, and `!dir:node_modules` leaves those folders out. Combined with file patterns, they narrow where those apply: `dir:node_modules; *.js` is JavaScript inside `node_modules`.
- File types: `is:video`, `is:audio`, `is:image`, `is:document`, `is:archive`, `is:diskimage`, `is:program`, `is:code`, and `is:other`, the same categories as the file-type colors. `!is:` leaves a type out, and any unambiguous start works (`is:doc`).
- Suggestions as you type: keywords, types, folder names after `dir:`, and extensions after `*.` from the scan, with counts and sizes. Press ↓ or Ctrl+Space to open them, Enter or Tab to use one.
- With a filter on, a folder's details show what matches there: matching size and share, how many of its files match, and the whole folder's size.

### Side panel
- The details stay on top; below them, the contents of the folder in the map, largest first, in step with the map's selection. Uncheck "Folders" to list only files, the largest from every level under the folder.
- With nothing selected, the details describe the folder in view.
- Results, small items, and what wasn't counted join the contents as tabs when open. Narrow windows stack the panel under the map.

### Sizes
- Sizes read KB, MB, and GB, counted the way the system's file manager counts them: 1,024 bytes per KB on Windows, like Explorer, and 1,000 on macOS and Linux, like Finder and Files.
- Sizes are file sizes throughout; the Allocated/Logical switch is gone. The volume overview still compares on-disk usage.

### Map and navigation
- The toolbar is regrouped, with a filter syntax popover, a scan progress bar, and a keyboard and mouse shortcuts sheet (press `?`).
- Breadcrumbs have an Up button and collapse deep paths.
- Treemap labels keep the size visible and pick a readable color per tile. Selection and hover are clearer, the tooltip stays near the pointer and shows the share of the parent folder, and the legend is a foldable card in both color modes.
- Result and small-item lists share one keyboard-driven list with range selection, a context menu, and readable folder paths.
- Show in Explorer/Finder opens a folder instead of selecting it in its parent; files are still only shown selected.

### Windows
- Restart as administrator, from the toolbar or beside "Permission denied" under Not counted, reopens SpaceBadger elevated and scans the same folder again, after asking. Declining leaves it as it was.

### Also
- The delete dialog is a true modal that shows full paths and ends on a result screen listing anything that couldn't be moved.
- Details, menus, and what wasn't counted explain themselves in plain words, and failures to load offer a Retry.
- Screen readers hear scan phases, search totals, and the selected item once, instead of every counter update.
- The window title shows the version, such as `SpaceBadger v0.2.0`; builds between releases add `+dev` and the commit.

## [0.1.0] - 2026-10-10

First release, as portable downloads for Windows x64, Linux x86_64 (AppImage), and macOS on Apple Silicon.

- Scans a folder or drive and draws it as a treemap while the scan runs, by allocated or logical size, with depth or file-type colors.
- Accounts for hard links, cloud placeholders (OneDrive), sparse and compressed files, and mount boundaries, and reports what it couldn't read.
- Volume overview for whole-drive scans, including space not attributed to files.
- Filename filter with several `;`-separated patterns, `*`/`?` wildcards, and `!` exclusions, with a ranked result list.
- Small-items lists, details panel, context menu, and "Show in file manager".
- Select several items and move them to the Recycle Bin or Trash, guarded by an "Allow deleting" switch, a typed confirmation, and never deleting permanently.


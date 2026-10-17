# Changelog

All notable changes to SpaceBadger are listed here. Versions follow [semantic versioning](https://semver.org/); new entries go under `## [Unreleased]`, and `scripts/release/bump.sh` turns them into a version's section on its release branch. Each `## [x.y.z]` section becomes the notes of that GitHub release.

## [Unreleased]

### Git repositories
- The scan finds Git repositories and linked worktrees, along with submodules, bare repositories, and nested repositories, by reading a few small files Git keeps. It never runs Git or changes anything, and a folder only counts when its Git data checks out: folders merely named `.git` or `worktrees` don't. What couldn't be confirmed is listed as unresolved, with the reason. A `.git` file pointing to a network share or removable drive outside the scan, or to Git data reached through a link, isn't followed ("not read"), and Git data still unread 5 seconds after every folder is listed is given up ("didn't answer in time"), so neither can hold up a scan. Some network locations (`/afs`, autofs, macOS firmlinks, and drives that look local such as iSCSI, virtual disks, or Google Drive) can't be told apart and are read, within that 5-second limit. A checkout kept inside a `.git` folder isn't detected.
- A **Git** tab lists them largest first, with kind, path, branch, and scanned size, and with a filter on, what matches in each. Selecting one selects it in the map and the details; Enter or double-click opens it.
- Repository and worktree folders get a thin colored line and a small badge such as `repo` or `worktree` in the map, in every color mode. Color by **Git** shows repository content, linked-worktree content, Git metadata (hatched), and everything else.
- The details show the repository an item is in, its kind and branch (or detached HEAD), a worktree's main repository or a submodule's superproject, and the repository's scanned size apart from what matches the filter.
- Nested repositories are counted once in totals, and a worktree's shared Git data counts once, in the main repository where it is. Nothing suggests a repository or worktree is unused or safe to delete.

### Filter
- `git:repo`, `git:worktree`, and `git:metadata` keep files in repositories and worktrees, in linked worktrees, or in Git's own data. They narrow everything else (`is:code; git:worktree`), and `!` leaves one out (`git:repo; !git:metadata`); `!git:repo` leaves out worktrees and Git's data too. Suggestions offer them.
- **All**, **Repos**, and **Worktrees** beside the filter box switch between all files, Git repositories, and linked worktrees, keeping the rest of the filter as typed.

### Scanning
- macOS lists folders with `getattrlistbulk`, one call per batch of entries instead of two per entry, with the same results; `SB_MACOS_LISTING=readdir` switches back to the old listing.

## [0.3.0] - 2026-10-14

### Filter
- Size terms: `size:>500MB`, `size:>=1GB`, `size:<10KB`, `size:<=2GB`, and ranges like `size:10MB..1GB`. Units are B, KB, MB, GB, and TB, counted like the sizes on screen (1,024 bytes per KB on Windows, 1,000 on macOS and Linux).
- Age terms: `older:1y` and `newer:30d` (with `h`, `d`, `w`, `mo`, or `y`), or a date: `older:2024-01-01`. Files whose modified time is unknown never match one.
- Size and age terms narrow everything else in the filter, and every one must hold; on their own they match every file that passes. `!` turns one around. Suggestions offer them, with common values such as `>1GB` and `6mo`.

### Details
- Files show when they were last modified.

### Compare with an earlier scan
- Each completed scan is saved as a compact snapshot. Scan the same folder again later, and **Compare…** in the toolbar lists the earlier scans to compare with: what grew, what shrank, and what's new since.
- Color by **Change**: grown items warm, shrunk ones cool, new ones purple, deeper the bigger the change.
- The details show an item's change, such as "+1.20 GB since Oct 1, 2:03 PM (was 3.40 GB)", and a **Changes** tab lists the biggest changes under the folder in view, including removed items.
- Files under 1 MiB are recorded without their size: one that grew past 1 MiB shows as grown by at least the difference, and otherwise no change is shown. Each folder keeps its newest 5 snapshots, the first of each earlier day, and its first, and at most 256 MiB in all.

### Export
- Export… beside the breadcrumbs and in a folder's right-click menu saves a folder's contents, or everything under it, as CSV or a text report. With a filter on, only matching files are exported, and folders with their matching totals.
- CSV has full paths, names, sizes in bytes and as shown, file counts, file types, each item's share, and files' modified times in ISO 8601 local time. It opens correctly in Excel, non-English names included, and names that look like formulas stay text.
- The text report lists totals, the 50 largest folders and files (with modified dates), the filter, and what wasn't counted.
- Large exports are written in the background while the map and scan carry on; when done, Show in Explorer shows the file.
- Exports and snapshots are written to a new file of their own, then renamed into place once complete: a failed export leaves the chosen file as it was, and no other file beside it is ever overwritten or written through a link. Only the file the app wrote can be put in place, even if another program swaps the temporary file for its own (on macOS and Linux, drives and shares without Unix permissions, such as USB sticks, SMB, and NTFS, only get a last-moment check that it is still the same file). Snapshots saved at the same moment, even by two copies of the app, never overwrite one another: each gets its own id, and the usual retention still applies. If the app is closed or crashes mid-export, a hidden `.<name>.….partial` file (on macOS and Linux, a folder holding it) may be left beside the chosen one; it can be deleted. When the file can't be replaced, the message says it may be open in another program or read-only.

### Map
- Moving the pointer over the map no longer redraws it: within one tile only the tooltip moves, and entering another tile just moves the hover highlight.
- When the map changes under a resting pointer (a rescan, resize, or another folder), the highlight and tooltip show the tile now under it, not another one.
- The map is redrawn sharp after moving the window to a monitor with different scaling.

### Toolbar
- The toolbar fits on one row at 1280 pixels again: Rescan and Cancel share one place (Cancel while scanning), and "Color by" is a list.

### Duplicates
- Find duplicates, beside Export… at the end of the breadcrumbs or in a folder's menu, finds files with the same contents in the folder in view, among the files the filter matches: `is:video` then Find duplicates finds duplicate videos. It's the only feature that reads file contents, and only when asked. Cloud files are never read, so nothing is downloaded, and two names of one hard-linked file don't count.
- The Duplicates tab lists groups as they're found, most content in extra copies first, with progress and a Stop button that keeps what was found. Choose the smallest file size to look at, from 1 KB to 100 MB (1 MB to start). The space deleting them would free on disk is estimated only where it's known (no unknown allocation or other hard-link names), since sparse, compressed, and linked files can free much less. Files written to while being read are left out.
- Select duplicates like any other items, or use "Select all but the first" to select every copy except the one with the shortest path in each group. Deleting still goes through the guarded delete, which warns if a group would lose every copy, and deleted files leave their groups. Before moving anything, the delete checks each duplicate you selected itself and reads the copies meant to stay again in full, with progress and a way to stop, keeping a selected duplicate that changed or whose other copy can't be confirmed unchanged; on Windows the confirmed copy is held until the moves are done. Duplicates inside a selected folder go with the folder.
- A group shows its first 100 files and a "Show more" row for the next thousand, so a folder of hundreds of thousands of identical files doesn't stall the list; "Select all but the first" still selects every extra copy, shown or not.

### Scanning
- Folders holding hundreds of thousands of files are read a few thousand at a time: the map stays responsive while they're read, Cancel takes effect at once, and they take about half the memory they did.
- The map is drawn quickly however many files a folder holds: files too small to see are only added up, not sorted, so a folder of 5 million small files lays out in about 10 ms instead of over half a second, and the map's memory of sibling order stays small (under 9 MiB).
- With a filter on while scanning, tooltips, the details, the filter's totals, and the Contents list no longer wait for each redraw of the map or for the filter to catch up with what was just scanned: the filter keeps up in the background, and its totals may trail the scan by a moment until it finishes. Suggestions for the filter no longer pause the scan while they're gathered.
- With Folders unchecked, the Contents tab no longer slows down the map, tooltips, and details while a scan runs: the files under a folder are ranked a little at a time and only as far as you've scrolled, and kept between updates, ranked again only when that folder has changed and at most every 0.4 s. Once the scan is done the list is always exact.
- Large scans no longer make the map, tooltips, and details stall now and then: the scan's record of what it found grows a piece at a time instead of being copied whole each time it fills up, which held everything else up for 25–90 ms at a few million items. No single update now holds them up for more than about a millisecond.
- Tooltips and the details come first: they no longer wait behind redrawing the map or filling lists. Moving to another folder or filter, or resizing quickly, drops the work for what's no longer shown instead of finishing it first, so the map and lists catch up sooner.

### Packages
- winget and Chocolatey packages of the portable Windows download, generated for each release: `winget install StevenPickles.SpaceBadger` and `choco install spacebadger` once they're accepted. Both add a `spacebadger` command.

### Deleting
- Only the item that was scanned is moved to the Recycle Bin or Trash: one replaced, modified, or turned into a link since the scan, or under a folder that was replaced, is left alone and reported as changed.
- The result says what happened to each item: moved, already gone, changed, couldn't be moved, or gone without being confirmed in the Recycle Bin (a warning). Items that were already gone are no longer counted as moved.
- The confirmation gives the content size, and how much disk space is freed only when that can be known (not with hard links or unmeasured files).
- On Windows, an item is left alone if any name in its full path, the scanned folder, the folders above it, and a network server and share included, would be read differently by the Recycle Bin: one ending with a dot or a space (`root.` would be taken for `root`), a reserved device name such as `NUL` or `COM1`, a special shell folder name ending in `.{…}`, or anything else Windows would rewrite. The result says which name, and nothing is moved.
- On Windows, the item Windows Explorer finds at the path must be the very one just checked, by name as well as by file (another hard-link name of the same file doesn't count). A special shell folder or a folder shortcut in the path (by its name or its `desktop.ini`) can lead Explorer to another place, such as your profile, the Recycle Bin, or the shortcut's target; such items are left alone with a message saying so.

### Fixed
- With Folders unchecked, the Contents tab could still list files just deleted, at no size, if it refreshed as the delete finished.
- The details could show the change of the item selected before, beside the one selected now, when its answer came back last; likewise for the folder in view.
- The details could fall back to an earlier filter's totals, or an earlier revision's, when a slow answer arrived after a newer one.
- The contents list could mix rows from different moments of a running scan, showing an item twice or leaving one out. The rows it shows now always come from one scan update, refreshed together as the scan goes on.

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


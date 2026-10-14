# SpaceBadger

A desktop disk-space explorer for Windows, Linux, and macOS. A Rust scanner feeds a nested, zoomable treemap in a Tauri webview.

- Product specification: [`spacebadger-implementation-brief.md`](spacebadger-implementation-brief.md)
- Implementation plan and milestones: [`docs/PLAN.md`](docs/PLAN.md)

Status: milestones 1–6 are complete; portable downloads (milestone 7) are set up, winget and Chocolatey packages are prepared (milestone 8), and apt and Homebrew are next. Choose a folder or drive and explore a live treemap while it scans: select, zoom, breadcrumbs, cancel with partial results. Select a "small items" region to list what it contains. Filter by file name to see only matching files in the map and a size-sorted result list. Right-click an item to show it in Explorer, Finder, or your file manager. Select several items and move them to the Recycle Bin or Trash. Color the map by folder depth or by file type, with a legend. Export a folder as CSV or a text report. Scan a folder again later and compare with an earlier scan to see what grew. When a whole drive is scanned, the volume overview shows files found, used space not attributed to any file (with likely reasons), and free space.

## Download

Portable builds for Windows x64, Linux x86_64 (AppImage), and macOS on Apple Silicon are on the [releases page](https://github.com/stevenpickles/spacebadger/releases). Nothing is installed: extract and run. They aren't code-signed, so Windows and macOS warn on first launch; see [`docs/RELEASING.md`](docs/RELEASING.md#downloads) for how to open them and how releases are made.

On Windows, once the packages are accepted, either package manager installs the same portable app and adds a `spacebadger` command:

```sh
winget install StevenPickles.SpaceBadger
choco install spacebadger
```

## Layout

| Path | Contents |
|---|---|
| `crates/sb-core` | Scan model, size accounting, aggregation, search, treemap layout, export |
| `crates/sb-scan` | Filesystem traversal and per-OS metadata adapters |
| `crates/sb-protocol` | Versioned IPC types; exports TypeScript bindings via `ts-rs` |
| `crates/sb-probe` | Filesystem correctness-probe CLI |
| `src-tauri` | Desktop shell and command bridge |
| `ui` | Svelte 5 + TypeScript interface (Vite) |

## Prerequisites

- Rust: installed via rustup. The pinned version in `rust-toolchain.toml` is installed automatically.
- Node.js 24+ and npm: build-time only. The installed app does not need Node.
- Platform webview and build tools ([Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)):
  - **Windows:** Microsoft C++ Build Tools ("Desktop development with C++") and WebView2. WebView2 is preinstalled on Windows 11.
  - **Linux (Debian/Ubuntu):** `libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libxdo-dev libssl-dev build-essential`
  - **macOS:** Xcode Command Line Tools.

## Commands

```sh
npm install                  # once
npm run tauri dev            # run the app with hot reload
cargo test --workspace       # core tests (also regenerates ui/src/lib/protocol/*.ts)
npm run check                # type-check the interface
npx tauri build              # release build (macOS app, Linux AppImage); add --no-bundle on Windows
cargo run --release -p sb-bench   # scale benchmark: 1M and 5M synthetic nodes
cargo run --release -p sb-bench -- --wide 5000000   # scanning one folder of 5M entries
```

The app also accepts a folder to scan on start: `spacebadger <folder>` (for example `target\debug\spacebadger.exe C:\Users\me`).

On Windows, **Restart as administrator…** in the toolbar reopens SpaceBadger with administrator rights and scans the same folder again, so folders that need those rights are counted too. Windows asks for permission first; the running copy stays as it was if you decline.

Press `?` (or the ? button in the toolbar) for a list of shortcuts. In the map: click selects, double-click (or Enter) opens a folder, Backspace or the ↑ button beside the breadcrumbs goes up, arrow keys move the selection, and Escape clears it. Right-click, the Menu key, or Shift+F10 shows actions: open in the map, go up, show in Explorer/Finder/file manager, and copy the path. Show in Explorer/Finder opens a folder in the file manager, and shows a file selected in its folder. Double-clicking a file does nothing; files are never opened.

The side panel shows the selected item's details on top and, below, the contents of the folder in the map, largest first. Selecting in either selects in both; Enter or double-click on a folder in the list opens it. Uncheck **Folders** to list only files, the largest anywhere under the folder, each with the subfolder it's in; Enter on one shows it in the map.

Selecting several items works as in a file manager, in the map, the contents list, the result list, and the small-items list: Ctrl+click (⌘-click on macOS) adds or removes an item, Shift+click selects the range from the last item clicked (in the map, among items in the same folder, largest first), and Ctrl+A selects everything drawn in the viewed folder. The details panel totals the selection.

Deleting is guarded:

- It's off until you check **Allow deleting** in the toolbar. That switch starts off every time the app starts and is never saved.
- With it on, Delete (or the menu or details panel) asks first, listing what will go. Folders go with everything in them, including files a filter hides. The button unlocks only after you type `delete` and a 3-second countdown has run.
- Items only ever go to the Recycle Bin (Trash on macOS and Linux). Nothing is deleted permanently: on Windows, anything the system would delete instead of recycling (network and removable drives, items too large for the Recycle Bin, recycling turned off) is left where it is.
- Deleting is available once the scan finishes or is cancelled.
- Only the item that was scanned is deleted. Just before each move, it and every folder above it are checked against the scan (same file ID or inode number, same drive, not a link, and for a file the same size and modified time). Anything replaced, modified, or turned into a link since is left alone as changed; scan again to see it as it is now. A folder the scan got no file ID for (some drives give none) can't be told from a replacement, so items in it are left alone too, and on Windows so are items whose path, from the drive or network share down, has a name ending with a dot or a space or a reserved device name such as `NUL`, which the Recycle Bin would mistake for another item, and items that Windows Explorer reads as somewhere else (through a special shell folder or folder shortcut in the path).
- The dialog gives the content size, and the disk space freed once the Recycle Bin or Trash is emptied only when it can be known: not when some files' allocation couldn't be measured or some have other hard links.

The result says what happened to each item: moved to the Recycle Bin, already gone (nothing moved), changed since the scan (left alone), couldn't be moved (left where it was), or not confirmed in the Recycle Bin (shown as a warning, since some or all of it may have been deleted without a copy). Items no longer where they were leave the map at once without a rescan; the others stay selected.

The checks narrow but can't fully close the gap between checking and moving, since the Recycle Bin and Trash move items by path. On Windows the folders above an item are held open so they can't be renamed or replaced during the move, and afterwards the item held open is checked to be the one in the Recycle Bin; something swapped in at that instant would be in the Recycle Bin, never deleted outright, and is reported. On macOS and Linux the checks run just before the move, by path.

The filter box matches file names (not paths), ignoring case. Separate patterns with `;`. A pattern with `*` (any text) or `?` (one character) must match the whole name, one without matches names containing it, and a leading `!` leaves names out: `*.jpg; *.png; !thumb*` finds JPEG and PNG files except thumbnails. Start a pattern with `dir:` to match folder names instead: `dir:cache` finds every file inside a folder named like `cache`, at any depth; `dir:node_modules; *.js` finds JavaScript files inside `node_modules`; and `*.log; !dir:archive` finds logs except those inside `archive` folders. `is:` picks a file type, by the same categories as the file-type colors: `is:video`, `is:audio`, `is:image`, `is:document`, `is:archive`, `is:diskimage`, `is:program`, `is:code`, or `is:other` (any unambiguous start works, like `is:doc`). `is:video; dir:Downloads` finds videos in Downloads, and `!is:code` leaves code out.

`size:` and `older:`/`newer:` narrow the matches by size and modified date; every one you add must hold. `size:>500MB`, `size:>=1GB`, `size:<10KB`, and `size:<=2GB` compare a file's size, and `size:10MB..1GB` is a range, ends included. Units are B, KB, MB, GB, and TB, in any case, decimals allowed (`1.5GB`); a bare number is bytes. A KB is 1,024 bytes on Windows and 1,000 on macOS and Linux, as sizes are shown. `older:1y` finds files last modified over a year ago and `newer:30d` those modified in the last 30 days, with `h`, `d`, `w`, `mo`, and `y` (a month is 30 days, a year 365), or a date: `older:2024-01-01`, `newer:2024-06-30` (from local midnight). A fractional size such as `1.3KB` rounds the way the comparison means, so `size:<1.3KB` keeps every whole byte below it. On their own they match every file that passes: `size:>1GB` is every file over 1 GB, and `*.iso; size:>1GB; older:1y` finds large disk images untouched for a year. A leading `!` turns one around (`!size:>1GB`). Files whose modified time is unknown never pass an age term. Times before 1970 or after 2106 aren't kept exactly: the details leave them out, and every file from before 1970 counts as older than any date from 1970 on, so dates before 1970 aren't told apart.

As you type, the filter suggests keywords, file types after `is:`, common sizes and ages after `size:`, `older:`, and `newer:`, folder names from the scan after `dir:`, and extensions from the scan after `*.`, with their counts and sizes. Press ↓ or Ctrl+Space to open the suggestions, Enter or Tab to use one, and Escape to close them. In the result list, Enter or double-click opens the file's folder in the map with the file selected; Escape in the filter box clears it.

To see what grew, scan a folder again later and compare. Each time a scan completes (not when it's cancelled), SpaceBadger saves a compact snapshot of it in its local data folder (`snapshots` in `%LOCALAPPDATA%\app.spacebadger.desktop` on Windows, `~/Library/Application Support/app.spacebadger.desktop` on macOS, `~/.local/share/app.spacebadger.desktop` on Linux). Once a later scan of the same folder completes, **Compare…** in the toolbar lists the earlier scans by date and size; choose one to compare with, and **Stop comparing** in the same list ends it. Rescanning keeps comparing with the same earlier scan. While comparing:

- **Color by Change** colors the map by how each item changed: grown warm, shrunk cool, new purple, unchanged neutral, deeper the bigger the change relative to the item's earlier size. Sizes in the map stay current sizes.
- The details read, for example, "Change: +1.20 GB since Oct 1, 2:03 PM (was 3.40 GB)", or "New since …".
- The **Changes** tab lists what grew, shrank, appeared, or was removed under the folder in view, largest change first; a new or removed folder stands for everything in it (uncheck **Folders** to list the files instead). Removed items aren't in the map; clicking another item selects it, and Enter opens a folder or shows a file in the map.
- With a name filter on, the map shows matching files, but changes are always of all files.

Snapshots record every folder, and files of 1 MiB or more with their sizes. Smaller files are recorded only by a hash of their name: one that has since grown past 1 MiB shows as grown by at least the difference, one that is still small gets no change shown rather than a guess, and a small file shows as new when its name wasn't there before (a rare hash collision makes it say nothing instead). A folder that couldn't be read, then or now, says nothing about what changed inside it, and one with entries that couldn't be read doesn't call a missing item removed or a found one new. Items are matched by path, ignoring case on Windows and macOS; a renamed item shows as removed under its old name and new under its new one. Each folder keeps its newest 5 snapshots, the first of each earlier day, and always its first; beyond 256 MiB in all, the oldest go. A full scan of a 3.6-million-item drive makes a 32 MiB snapshot.

**Export…** at the right end of the breadcrumbs, or in a folder's right-click menu, saves what's in a folder as a file. Choose **This folder's contents** (its own files and folders) or **Everything under this folder** (every file and folder at any depth), and a format:

- **CSV**, for spreadsheets: one row per item with its full path, name, `folder` or `file`, size in bytes, size as shown in the app, the number of files (for folders), the file type (the same categories as `is:` and the file-type colors), its share of the exported folder in percent, and, for files, when it was last modified, in ISO 8601 local time with the UTC offset in effect then (such as `2026-10-13T18:05:09+02:00`; empty when unknown, or when the file system reports a time before 1970 or after 2106). Each folder comes before its contents, largest first. The file is UTF-8 with a byte order mark and CRLF line endings, quoted as in RFC 4180, so Excel and other spreadsheet apps read names with commas, quotes, line breaks, or non-English characters correctly. A name that starts with `=`, `+`, `-`, `@`, a tab, or a carriage return is written with a leading `'` (`=SUM(A1).txt` becomes `'=SUM(A1).txt`, `@types` becomes `'@types`), so a spreadsheet shows it as text instead of running it as a formula.
- **Text report**: the folder, scan root, how the scan went, when it was exported, totals, the 50 largest folders and the 50 largest files under the folder (files with when they were last modified), and what the scan didn't count.

With a filter on, only matching files are exported, with the folders that contain them and the size and count of their matching files. You choose where to save in the system's save dialog; the suggested name is like `SpaceBadger-Downloads-2026-10-13.csv`. The file is written in the background, in steps, so the map and a running scan carry on; when it's done, **Show in Explorer** (Finder, file manager) shows the file. The export is written to a hidden temporary file beside the one you chose (named like `.<name>.<number>.partial`), which replaces it only once complete: a failed export leaves an existing file as it was, and no other file is touched. If SpaceBadger is closed or crashes mid-export, that temporary file can be left behind; it's safe to delete. Rescanning or choosing another folder stops an export in progress, as does changing the filter.

An export taken while scanning holds what had been scanned so far. Sizes are as of the moment each row was written, shares are of the folder's size at that moment, and each folder's contents are ordered largest first as they were when the export reached that folder, so later rows may be larger than earlier ones.

**Find duplicates**, beside **Export…** at the right end of the breadcrumbs (or **Find duplicates here** on a folder's menu), looks for files with the same contents in the folder in view, among the files the filter matches if one is on: `is:video` then Find duplicates finds duplicate videos. It is the one feature that reads what's inside files, and only when you ask:

- Only files that share their size with another are read. Each is compared by a hash of its first and last 64 KB, and those still alike by a hash of their whole contents (BLAKE3).
- Cloud files are never read, so nothing is downloaded. A file is only read if it is still a plain file of the size the scan saw, never through a link it has become; files stored in a special way (deduplicated or tiered) are left out too, since reading them raw would get them wrong. Files that are gone, changed, or in use are left out and counted.
- A hard-linked file counts once, so two of its names are never duplicates: deleting one frees nothing. A file with other names is marked "has other names", since deleting it alone may free nothing.
- Files smaller than the size chosen in the Duplicates tab (1 KB to 100 MB; 1 MB to start) are left out.
- A file written to while it's read is left out too: each file's identity, size, and modified time are checked as it's opened and again once read.

Groups appear as they're confirmed, with the most content in the copies beyond the first on top; Stop keeps what was found. That content is the files' length, and deleting may free less on disk: sparse and compressed files take less, a file with other hard-link names frees nothing until every name goes, and block cloning or deduplication can share storage. An on-disk estimate, "up to … on disk" (every copy's allocated size but the smallest's), is shown only where every copy's allocated size is known and none has other names.

Files in the list select like anywhere else, and **Select all but the first** selects every copy except the one with the shortest path in each group. Nothing is ever deleted for you: deleting goes through the guarded delete above. Its dialog warns if a group would lose every copy, or that duplicates you selected may be kept because the copies meant to stay have gone or changed since they were compared (by identity, size, and modified time), but that check is provisional. When the delete starts, before anything moves, each duplicate you selected itself (as from the Duplicates tab or **Select all but the first**) must be unchanged since it was compared, and the copies meant to stay are read again in full, a few at a time, until one per group still matches. A selected duplicate is kept, with a note naming it and saying why, if it changed or if no copy of its group can be confirmed, whether the copies changed, couldn't be read, or checking was stopped. On Windows the confirmed copy is held open until the moves are done, so it can't change or go meanwhile. Duplicates inside a selected folder go with the folder. Deleted files leave their groups.

Press <kbd>`</kbd> (backquote) while the map has focus to show a timing overlay: layout round trips, drawing, and input-to-frame latency.

`cargo build`/`cargo test` without `--workspace` cover only the library crates, so they don't need the webview toolchain.

After changing a type in `sb-protocol`, run `cargo test -p sb-protocol` and commit the regenerated bindings. CI fails if they are stale.

## License

MIT; see [`LICENSE`](LICENSE). Release downloads include the licenses of bundled third-party code in `THIRD-PARTY-LICENSES.html`.

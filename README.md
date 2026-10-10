# SpaceBadger

A desktop disk-space explorer for Windows, Linux, and macOS. A Rust scanner feeds a nested, zoomable treemap in a Tauri webview.

- Product specification: [`spacebadger-implementation-brief.md`](spacebadger-implementation-brief.md)
- Implementation plan and milestones: [`docs/PLAN.md`](docs/PLAN.md)

Status: milestones 1–6 are complete; portable downloads (milestone 7) are set up, and package-manager packages are next. Choose a folder or drive and explore a live treemap while it scans: select, zoom, breadcrumbs, allocated/logical sizes, cancel with partial results. Select a "small items" region to list what it contains. Filter by file name to see only matching files in the map and a size-sorted result list. Right-click an item to show it in Explorer, Finder, or your file manager. Select several items and move them to the Recycle Bin or Trash. Color the map by folder depth or by file type, with a legend. When a whole drive is scanned, the volume overview shows files found, used space not attributed to any file (with likely reasons), and free space.

## Download

Portable builds for Windows x64, Linux x86_64 (AppImage), and macOS on Apple Silicon are on the [releases page](https://github.com/stevenpickles/spacebadger/releases). Nothing is installed: extract and run. They aren't code-signed, so Windows and macOS warn on first launch; see [`docs/RELEASING.md`](docs/RELEASING.md#downloads) for how to open them and how releases are made.

## Layout

| Path | Contents |
|---|---|
| `crates/sb-core` | Scan model, size accounting, aggregation, search, treemap layout |
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
```

The app also accepts a folder to scan on start: `spacebadger <folder>` (for example `target\debug\spacebadger.exe C:\Users\me`).

In the map: click selects, double-click (or Enter) opens a folder, Backspace goes up, arrow keys move the selection, and Escape clears it. Right-click, the Menu key, or Shift+F10 shows actions: open in the map, go up, show in Explorer/Finder/file manager, and copy the path. Double-clicking a file does nothing; files are never opened.

Selecting several items works as in a file manager, in the map, the result list, and the small-items list: Ctrl+click (⌘-click on macOS) adds or removes an item, Shift+click selects the range from the last item clicked (in the map, among items in the same folder, largest first), and Ctrl+A selects everything drawn in the viewed folder. The details panel totals the selection.

Deleting is guarded:

- It's off until you check **Allow deleting** in the toolbar. That switch starts off every time the app starts and is never saved.
- With it on, Delete (or the menu or details panel) asks first, listing what will go. Folders go with everything in them, including files a filter hides. The button unlocks only after you type `delete` and a 3-second countdown has run.
- Items only ever go to the Recycle Bin (Trash on macOS and Linux). Nothing is deleted permanently: on Windows, anything the system would delete instead of recycling (network and removable drives, items too large for the Recycle Bin, recycling turned off) is left where it is.
- Deleting is available once the scan finishes or is cancelled.

Deleted items leave the map at once without a rescan; items that couldn't be moved stay selected, with the reason shown.

The filter box matches file names only (not folders or paths), ignoring case. Separate patterns with `;`. A pattern with `*` (any text) or `?` (one character) must match the whole name, one without matches names containing it, and a leading `!` leaves names out: `*.jpg; *.png; !thumb*` finds JPEG and PNG files except thumbnails. In the result list, Enter or double-click opens the file's folder in the map with the file selected; Escape in the filter box clears it.

Press <kbd>`</kbd> (backquote) while the map has focus to show a timing overlay: layout round trips, drawing, and input-to-frame latency.

`cargo build`/`cargo test` without `--workspace` cover only the library crates, so they don't need the webview toolchain.

After changing a type in `sb-protocol`, run `cargo test -p sb-protocol` and commit the regenerated bindings. CI fails if they are stale.

## License

MIT; see [`LICENSE`](LICENSE). Release downloads include the licenses of bundled third-party code in `THIRD-PARTY-LICENSES.html`.

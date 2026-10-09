# SpaceBadger

A desktop disk-space explorer for Windows, Linux, and macOS. A Rust scanner feeds a nested, zoomable treemap in a Tauri webview.

- Product specification: [`spacebadger-implementation-brief.md`](spacebadger-implementation-brief.md)
- Implementation plan and milestones: [`docs/PLAN.md`](docs/PLAN.md)

Status: milestone 4 features complete; milestone 5 (scale and packaging) is next. Choose a folder or drive and explore a live treemap while it scans: select, zoom, breadcrumbs, allocated/logical sizes, cancel with partial results. Select a "small items" region to list what it contains. Filter by file name to see only matching files in the map and a size-sorted result list. Right-click an item to show it in Explorer, Finder, or your file manager. Color the map by folder depth or by file type, with a legend. When a whole drive is scanned, the volume overview shows files found, used space not attributed to any file (with likely reasons), and free space.

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
npx tauri build              # release build + native installers
cargo run --release -p sb-bench   # scale benchmark: 1M and 5M synthetic nodes
```

The app also accepts a folder to scan on start: `spacebadger <folder>` (for example `target\debug\spacebadger.exe C:\Users\me`).

In the map: click selects, double-click (or Enter) opens a folder, Backspace goes up, arrow keys move the selection, and Escape clears it. Right-click, the Menu key, or Shift+F10 shows actions: open in the map, go up, show in Explorer/Finder/file manager, and copy the path. Double-clicking a file does nothing; files are never opened.

The filter box matches file names only (not folders or paths), ignoring case, as a literal substring: `.pdf` finds every PDF, and `*` has no special meaning. In the result list, Enter or double-click opens the file's folder in the map with the file selected; Escape in the filter box clears it.

Press <kbd>`</kbd> (backquote) while the map has focus to show a timing overlay: layout round trips, drawing, and input-to-frame latency.

`cargo build`/`cargo test` without `--workspace` cover only the library crates, so they don't need the webview toolchain.

After changing a type in `sb-protocol`, run `cargo test -p sb-protocol` and commit the regenerated bindings. CI fails if they are stale.

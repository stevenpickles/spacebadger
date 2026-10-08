# SpaceBadger

A desktop disk-space explorer for Windows, Linux, and macOS. A Rust scanner feeds a nested, zoomable treemap in a Tauri webview.

- Product specification: [`spacebadger-implementation-brief.md`](spacebadger-implementation-brief.md)
- Implementation plan and milestones: [`docs/PLAN.md`](docs/PLAN.md)

Status: milestone 0 (project baseline). The app opens a window, but there's no scanning yet.

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
```

`cargo build`/`cargo test` without `--workspace` cover only the library crates, so they don't need the webview toolchain.

After changing a type in `sb-protocol`, run `cargo test -p sb-protocol` and commit the regenerated bindings. CI fails if they are stale.

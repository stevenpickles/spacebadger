# SpaceBadger — implementation plan

Derived from [`spacebadger-implementation-brief.md`](../spacebadger-implementation-brief.md), which remains the product specification. This plan records the architecture decisions, platform strategy, and milestone sequence. Update it when a decision changes.

## Environment baseline (2026-10-08)

| Item | State |
|---|---|
| Rust | 1.97.1 (MSVC), pinned in `rust-toolchain.toml` |
| Tauri | 2.12.x (latest stable; 3.0 is still alpha and not used) |
| Frontend build | Node 26 / npm 11, build-time only; the installed app has no Node runtime |
| Windows validation | Windows 11 + NTFS + OneDrive available on the dev machine |
| Linux validation | Docker Desktop only (no WSL distro): good for scanner probes and bind mounts, not for the desktop app |
| macOS validation | No machine available yet; CI build only until a real Mac with iCloud Drive is available |

## Architecture

| Area | Decision |
|---|---|
| Workspace | Cargo workspace: `crates/sb-core` (model, aggregation, search, layout), `crates/sb-scan` (traversal + per-OS adapters), `crates/sb-protocol` (versioned IPC types), `crates/sb-probe` (correctness-probe CLI), `src-tauri` (desktop bridge), `ui/` (Svelte 5 + TypeScript + Vite, canvas treemap). `sb-bench` is added in milestone 5. |
| Type sharing | IPC types live in `sb-protocol` and are exported to TypeScript with `ts-rs` into `ui/src/lib/protocol/`. A test fails if the generated bindings are stale. (`tauri-specta` for Tauri 2 is still a release candidate, so it is not used.) |
| Data model | Struct-of-arrays keyed by `u32` NodeId: parent, first-child, next-sibling, name offset into a byte arena (native names), logical `u64`, allocated `u64` with an unknown sentinel, file identity, flags. Full paths are reconstructed on demand. Target ~60 B/node + names, so 5M nodes ≈ 300 MB + names. |
| Scanner | Bounded directory-worker pool (smaller for network roots) → batches → a single aggregator thread that owns the model and propagates ancestor sums incrementally. Every scan has a generation ID; cancellation is an atomic flag checked between OS calls. |
| Streaming | The backend emits a coalesced progress/dirty event every ~150 ms. The UI then **pulls** the layout for the current viewport, which gives natural backpressure. The full tree is never sent. |
| Layout | In Rust, viewport-scoped: squarified treemap with depth and pixel thresholds and an "Other small items" aggregate. Returned as a compact binary rect list. Sibling order is re-sorted only when sizes change past a hysteresis threshold, plus a final relayout on completion. Hit testing is done in the UI against the returned rects. |
| Search | A backend overlay over the model with its own match aggregates, maintained incrementally as files arrive and tagged with a query revision so stale results are dropped. The result list is virtualized. |
| UI | Canvas for the treemap; standard accessible DOM for the toolbar, breadcrumbs, details panel, search results, and status bar. |

## Platform metadata strategy (highest risk)

- **Windows:** enumerate with `NtQueryDirectoryFile` + `FileIdExtdDirectoryInformation`, which returns EndOfFile, AllocationSize, attributes, reparse tag, and file ID per directory batch without opening files (fast and non-hydrating).
  - *Probe:* is directory-entry AllocationSize accurate versus `GetCompressedFileSizeW` for sparse, compressed, and OneDrive placeholder files? Directory entries are known to be stale in some cases.
  - *Probe:* the link count isn't in directory info. Is opening a file with `FILE_READ_ATTRIBUTES | FILE_FLAG_OPEN_REPARSE_POINT` safe for placeholders (no hydration)?
  - Reparse tags by category: symlink and mount-point (junction or volume mount) are skipped and counted; cloud tags are measured; unknown directory tags are skipped and reported.
- **Linux:** `getdents` plus `statx`/`fstatat` with `AT_SYMLINK_NOFOLLOW`. Allocated bytes = `st_blocks × 512`. Mount boundaries come from `STATX_ATTR_MOUNT_ROOT` or `stx_mnt_id`, falling back to `/proc/self/mountinfo`, so same-device bind mounts are caught. Hard links are tracked by `(dev, ino)`.
- **macOS:** `getattrlistbulk` provides allocated size, data length, file ID, link count, flags, and mount status. Set `IOPOL_MATERIALIZE_DATALESS_FILES` to "off" for the scan threads. Identify cloud-only files by `SF_DATALESS`.
  - *Probe:* `/` scanning across the system/data volume split and firmlinks.
- **Show in file manager:**
  - Windows: `SHOpenFolderAndSelectItems`.
  - macOS: `NSWorkspace activateFileViewerSelectingURLs`.
  - Linux: `org.freedesktop.FileManager1.ShowItems` over D-Bus, falling back to opening the parent folder with structured arguments, never through a shell.
  - Check first whether `tauri-plugin-opener`'s `revealItemInDir` meets these rules.

## Milestones

0. **Baseline** — pinned toolchain and dependencies, workspace skeleton, minimal Tauri window, shared protocol bindings, CI on Windows, Linux, and macOS (fmt, clippy, tests, UI type-check, app build).
1. **Correctness probes** — `sb-probe` CLI reporting length, allocation (by every candidate API), file ID, link count, reparse tag or flags, and hydration state before and after.
   - Windows fixtures: sparse (`fsutil sparse`), compressed (`compact /c`), hard links (`mklink /H`), junctions, symlinks, and OneDrive files in their three states (online-only, locally available, always keep).
   - Linux fixtures, in Docker: sparse files, bind mounts, symlink loops.
   - Output: a results table in `docs/validation/`, with unsupported cases listed explicitly.
2. **Scanner core** — scoped traversal, stable identities, both metrics, omission aggregation by reason, batching, cancellation. Tested with fixture trees and a fake-filesystem adapter, no window needed.
3. **Streaming desktop slice** — root picker, live provisional treemap, select/zoom, breadcrumbs, cancel with retained partial results.
4. **Usability** — filename filter and results list, details panel, reveal, refresh, color modes, empty and error states, optional volume overview strip.
   - **Unattributed space block.** At a whole-volume root, in allocated mode, with no filename filter, and only after a completed scan: one block sized as volume used (capacity − free) minus scanned allocation. It's labelled as a non-file region and opens a breakdown of likely contributors (permission-denied folders with counts and samples, filesystem metadata, shadow copies, alternate data streams, other overhead). During a scan or after cancel, the gap includes unscanned files, so the block isn't shown.
   - **Elevated scans (scanner side, early milestone 3).** Never auto-elevate. When the process already runs elevated:
     - Windows: enable `SeBackupPrivilege` and open directories with `FILE_FLAG_BACKUP_SEMANTICS` so protected folders (`System Volume Information`, `Config.Msi`, other users' recycle bins) list normally, which also surfaces shadow-copy storage.
     - Windows: query NTFS metadata sizes (`FSCTL_GET_NTFS_VOLUME_DATA` for MFT valid length, plus the change journal) and show them as items in a "Filesystem metadata" block.
     - Alternate data streams stay opt-in ("measure alternate streams"), because per-file stream queries are ~6.5× slower (milestone 1). Raw MFT reading is a separate later decision.
     - Linux/macOS: running as root removes most permission-denied omissions; filesystem overhead (e.g. ext4 reserved blocks) stays in the residual block.
   - Whatever remains after measured contributors is shown as "other filesystem overhead", never distributed into folders.
5. **Scale and packaging** — `sb-bench` with 1M and 5M synthetic plus real fixtures, memory/IPC/layout tuning, native packages, README, validation report.

## Open questions

1. Is a Mac with iCloud Drive available for validation? If not, macOS ships as "builds in CI, unvalidated" with that limitation documented.
2. Is an SMB or NFS share available for slow-network testing?

## Decisions log

- 2026-10-08: Tauri 2.12 (stable) over 3.0-alpha. `ts-rs` over `tauri-specta` (RC). Svelte 5 + Vite for UI chrome; treemap drawn on canvas.
- 2026-10-08: Windows scanner reads allocated size from directory enumeration (`FileIdExtdDirectoryInfo`) and detects hard-link aliases by directory file ID instead of opening every file; alternate data streams are not counted. Evidence: [`docs/validation/windows-m1.md`](validation/windows-m1.md).
- 2026-10-08: OneDrive online-only files: enumeration and attribute-only opens don't hydrate; detect cloud state from attributes because reparse tags are disguised by default. Linux mount boundaries need `STATX_ATTR_MOUNT_ROOT`/mount ID, because bind mounts share `st_dev`. Evidence: [`docs/validation/linux-m1.md`](validation/linux-m1.md).
- 2026-10-08: Scanner core built (milestone 2). The default 8 workers scan ~110–190k files/s warm on NVMe; full `C:\` (2.95M files) peaks at 420 MiB. macOS adapter is portable `readdir`/`lstat` until a Mac is available. Evidence: [`docs/validation/scanner-m2.md`](validation/scanner-m2.md).
- 2026-10-08: Space not attributable to files (67 GiB on the benchmark `C:\`) is shown as a separate "unattributed" block at whole-volume roots after a completed scan, kept distinct from file area. Elevated scans measure protected folders and NTFS metadata into it; alternate streams are opt-in. The app never elevates itself.

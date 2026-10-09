# Milestone 3 — streaming desktop slice

Date: 2026-10-08. Branch `feature/4/streaming-slice`. Same benchmark machine as [`scanner-m2.md`](scanner-m2.md).

## What exists

- **Layout (`sb-core::layout`):** squarified, nested treemap computed in Rust for the current view only.
  - Largest folders are expanded first, up to a 30,000-rectangle budget, while a folder's interior is at least 4 px.
  - Children under 12 px² are merged into one "N small items" region with their true combined area; nothing is inflated.
  - Folder header strips (16 px) carry the name and size when there's room.
  - While scanning, sibling order is kept from the previous layout and re-sorted only when a sibling outgrows the one before it by 1.5×. Finished scans sort afresh.
- **Bridge (`src-tauri`):** `scan_choose` (native folder dialog), `scan_refresh`, `scan_cancel`, `scan_status`, `layout`, and `node_details`.
  - Every scan-specific command names its generation; older generations are refused.
  - Status events are the scanner's coalesced progress (first change at once, then every 150 ms).
  - Layouts are returned as a compact binary record list ([`sb_protocol::wire`](../../crates/sb-protocol/src/wire.rs)), not JSON.
- **Interface:** canvas treemap plus DOM toolbar, breadcrumbs, details panel, omissions list, and status bar.
  - The map pulls a layout when the scan revision, view, metric, or size changes. Only one request is in flight; changes that arrive meanwhile become one follow-up request.
  - Replies for another view, metric, or scan are dropped. Selection and view are stable node IDs, so updates never reset them.
  - Labels are cut with an ellipsis and clipped to their rectangle.
  - Keyboard: arrows move the selection among same-depth items, Enter opens a folder, Backspace goes up, Escape clears.
- **Scanner additions:**
  - Network roots (UNC or mapped drive on Windows; NFS/SMB/CIFS/9P/Ceph/AFS/Coda/FUSE/Lustre on Linux) scan with 3 workers.
  - On Windows, an already-elevated process enables `SeBackupPrivilege`, so protected folders list. The app shows an "Administrator access" badge. It never asks for elevation.

## Measurements

| Check | Result |
|---|---|
| First map, user profile (1.34M files), debug build | Rendered within 2.5 s of launch, while scanning |
| Layout of user profile root, 1920×1080, release | 4–6 ms for ~7,000 rectangles (first sort and stable repeat) |
| Cancel from the toolbar (UI Automation) | Banner, status "Cancelled: partial result", map retained |
| Allocated ↔ Logical toggle on a partial result | Relayout with labels and totals updated |

## Verified by screenshot

Driven through Windows UI Automation and captured with `PrintWindow`:
- Live streaming map at 2.5 s and 30 s.
- Cancel and the partial-result banner.
- The Logical toggle.

Synthetic mouse and keyboard input is blocked in the automation session, so these were not exercised automatically: click selection, double-click zoom, breadcrumbs, and keyboard navigation. They need a manual pass.

## Known gaps

- "Other small items" can be selected, and its folder opened, but its members are only listed by filename search in milestone 4.
- Folders that weren't scanned after a cancel hold no bytes, so they have no area. Their state shows in the details panel.
- Network shares: worker count is reduced, but no share has been measured yet (open question 2).
- macOS: remote detection always reports local, and privileged detection isn't implemented.

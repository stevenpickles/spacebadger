# Milestone 2 — scanner core

Date: 2026-10-08. Branch `feature/3/scanner-core`.

## What exists

- **`sb-core::tree`:** struct-of-arrays nodes with stable `u32` IDs and a native-name arena. Ancestor totals for logical size, known allocation, unknown-allocation count, and file count are updated incrementally. Hard-link aliases keep their logical length but carry no allocation.
- **`sb-scan::engine`:** a bounded worker pool feeding one aggregator that owns all tree writes.
  - Generation IDs per scan.
  - Hard-link ownership by identity (first path seen owns the allocation), with a re-measure of both names because NTFS alias listings can be stale.
  - Omissions grouped by reason with bounded samples that include the OS message.
  - Coalesced progress: the first change publishes immediately, then at most every 150 ms.
  - Cancellation: unlisted folders are marked not scanned, and the scan never reports completion.
- **Adapters:**
  - Windows (`FileIdExtdDirectoryInfo` listings).
  - Linux (`statx` relative to `O_NOFOLLOW` directory fds, mount ID boundaries).
  - Portable Unix for macOS.
  - In-memory fake for tests.
- **Tests:**
  - 11 engine tests on the fake filesystem: hierarchy and weights, cloud and unknown allocation, hard links, links and mounts and special files, errors, link and unreadable roots, partial results while scanning, cancel while an OS call is blocked, generations, and event coalescing.
  - 3 native integration tests on real temporary folders.
  - These pass on Windows and in a Linux container. macOS runs them in CI only.
- **`cargo run --release -p sb-scan --example scan -- <root>`** for real-disk runs.

## Measurements

Benchmark machine: AMD Ryzen 9 5900X (12 cores), 64 GiB RAM, Samsung 980 PRO 2 TB NVMe, Windows 11 Pro 10.0.26200, NTFS. 8 workers (the default on this machine).

| Root | Files | Dirs | Aliases | Time | Files/s | Notes |
|---|---:|---:|---:|---:|---:|---|
| OneDrive folder | 47,148 | 8,937 | 5 | 0.25 s | 188k | Warm |
| User profile | 1,336,611 | 227,249 | 83,048 | 23.0 s | 58k | Cold cache |
| User profile | 1,336,885 | — | 83,048 | 7.5–8.5 s | 156–178k | Warm |
| `C:\` | 2,952,127 | 590,364 | 365,335 | 46.8 s | 63k | Partly cold |
| `C:\` | 2,952,129 | — | — | 26.9 s | 110k | Warm; **peak working set 420 MiB** |

- **First files** reached the tree in under 1 ms after start on every warm run.
- **Cancel** during a `C:\` scan was acknowledged in 12.5 ms against a 250 ms target, keeping 289,794 files.
- **Memory:** the tree for 3.54M nodes is about 279 MiB (estimate); the whole process peaked at 420 MiB. Scaled linearly, 5M nodes would be about 600 MiB for the scanner, before the webview.
- **Totals versus the volume:** `C:\` file allocation was 1,431.7 GiB, while the volume reports 1,498.6 GiB used. The 67 GiB gap is the kind of difference the status message must explain: NTFS metadata (MFT and others), shadow copies, 596 permission-denied folders (e.g. `System Volume Information`, `Config.Msi`), and uncounted alternate data streams.
- **Omissions on `C:\`:**
  - 2,501 symlinks and 138 mount points/junctions (e.g. `Documents and Settings`)
  - 122 special files (Unix sockets)
  - 596 permission denied
  - 1 sharing violation

## Known gaps

- **macOS adapter:** unvalidated. It uses `readdir` + `lstat` with device-only mount checks, so scanning `/` will treat firmlinked data-volume folders as other filesystems. A `getattrlistbulk` adapter and firmlink handling need a real Mac.
- **Windows volume mounted only into a folder:** it can't be chosen as a root, because it's rejected as a link. A drive letter or volume GUID path works.
- **Hard-link tracking on Windows** keeps a map entry for every file (listings carry no link count). It's included in the 420 MiB peak.
- **Pending folder paths:** queued directories hold their full path until listed. This hasn't been an issue so far; revisit if very wide trees show memory spikes.
- **Network shares** are not yet measured. The worker count should be lowered for them in milestone 3.

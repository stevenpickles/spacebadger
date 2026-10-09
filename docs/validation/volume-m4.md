# Milestone 4 — volume overview

Date: 2026-10-09. Branch `feature/8/volume-overview`. Same benchmark machine as [`scanner-m2.md`](scanner-m2.md).

## What exists

- **Volume info (`sb-scan::native::volume_info`):** capacity, free bytes (including any reserved for administrators), filesystem name, and whether the scan root is the volume's top folder.
  - Windows: `GetVolumePathNameW`, `GetDiskFreeSpaceExW`, and `GetVolumeInformationW`. On NTFS it also reads the master file table's valid length with `FSCTL_GET_NTFS_VOLUME_DATA`. That needs the volume device opened, which normally requires administrator rights.
  - Linux: `statvfs`. The root counts as the volume top when statx marks it as a mount root, or when its device or mount ID differs from its parent's. Common filesystem names come from `statfs` magic numbers.
  - macOS: not implemented yet, so the overview isn't offered there.
- **Strip:** a "Volume overview" checkbox in the toolbar, off by default and remembered on the device. It's enabled only when the scan root is a volume's top folder.
  - The strip shows files found, used space not attributed to files, and free space as shares of capacity.
  - While scanning, the middle block is hatched and labelled "Not scanned yet"; after a cancel, "Not scanned". It's only called "not attributed" after a completed scan, as the plan requires.
  - With Logical size or a filter the strip is replaced by a note, because it compares allocated space.
  - Its explanation lists:
    - NTFS metadata: measured when possible, otherwise a hint to run as administrator.
    - The number of unreadable items, with a link to the omission list.
    - Shadow copies and restore points.
    - Alternate data streams.
  - When something was measured, it splits the gap into measured and unexplained parts.
  - The unattributed space is never spread into folders in the map.

## Verified on Windows

Debug build, non-elevated, `C:\` (1.82 TiB NTFS), checked with screenshots:

| Check | Result |
|---|---|
| `C:\Users\Steven\Downloads` in the scan example | Reported as not the volume top |
| Scan of `C:\` | Overview offered |
| During the scan, 20 s in | Files found 921 GiB, "Not scanned yet" 583 GiB (hatched), Free 358 GiB |
| After completion (2.96M files) | Files found 1.41 TiB (77.4%), not attributed 63.1 GiB (3.4%), free 357 GiB (19.2%) |
| Explanation | 1.47 TiB used vs 1.41 TiB measured; NTFS metadata not measured (not elevated); 596 unreadable items with a link; shadow copies; alternate streams |
| Master file table size, non-elevated | Not readable, as expected |
| Malformed root (`C:"`) | Scan fails with a clear banner; overview checkbox stays disabled |

The 63.1 GiB gap is close to the ~67 GiB found in milestone 2.

## Not yet verified

- An elevated run, where the master file table should be measured and protected folders should shrink the gap.
- Linux behavior in the desktop app (the volume info itself is tested in a Linux container), and macOS.

# Ending a listing at a short buffer (not shipped)

Date: 2026-10-17. Branch `feature/60/windows-listing-v2`. Design idea 2.8.

## The idea

NTFS fills a `NtQueryDirectoryFile` buffer until the next entry wouldn't fit. A buffer that comes back with room for the largest record (600 bytes for `FILE_ID_EXTD_DIR_INFORMATION`, 616 for `FILE_ID_BOTH_DIR_INFORMATION`: the header plus a 255-unit name, rounded to 8) should therefore hold the last entries. If so, the following call, which only says "no more files", could be skipped. That is one call per folder, measured at 4.6–16 µs.

## What was measured

A validation build made the extra call anyway and counted every case where it returned data after a buffer that had room left. It covered local fixed NTFS and ReFS roots only.

| Run | Listings that could have ended early | Extra calls that returned data |
|---|---:|---:|
| `C:\` full scan, 1 | 638,146 | 0 |
| `C:\` full scan, 2 | 638,149 | 0 |

Machine: Ryzen 9 5900X, Samsung 980 PRO, Windows 11 Pro 10.0.26300, NTFS, not elevated.

The core of the patch was in `list_with`. After each query that filled `filled` bytes:

```rust
if expect_end { MISSES += 1 }            // the call after a short buffer returned data
expect_end = short_ends && filled + T::MAX_RECORD <= size_of_val(buffer);
```

`short_ends` was set in `open_root` when the root was on a local fixed volume whose `GetVolumeInformationW` name was `NTFS` or `ReFS`. The full patch was kept as `scratchpad\agent-listing\validate-2.8.patch` in the session that produced this note.

## Why it isn't shipped

- **It can lose entries without any sign.** When a filesystem returns a short buffer before the end, the rest of the folder is silently missing. Exactness is the product's core promise, and these places either do return short buffers or haven't been validated:
  - ReFS and Dev Drive;
  - ProjFS virtualization roots;
  - bind links, whose merged views can cross volumes;
  - Cloud Files providers that populate folders partially;
  - minifilters that trim entries;
  - Dokan and WinFsp volumes, which report themselves as "NTFS".

  The volume check can't tell any of these apart from plain NTFS.
- **The gain is within noise.** On the measured trees, one call per folder made no difference that interleaved runs could separate from noise on a loaded machine.

Revisit only with a way to know a folder is plain NTFS end to end, and a measured gain.

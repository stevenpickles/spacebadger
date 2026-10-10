# Milestone 1 — Windows correctness probes

Date: 2026-10-08. Tool: `sb-probe` (this branch). Fixtures: `scripts/probe-fixtures.ps1`.

## Environment

| Item | Value |
|---|---|
| OS | Windows 11 Pro 10.0.26200, x64 |
| Volume | C:, NTFS, 4 KiB clusters |
| Privileges | Standard user; Developer Mode off (symlink creation unavailable) |
| OneDrive | Personal account running, sync root registered. Files On-Demand was off for the first run and enabled for the cloud tests below |
| Elevation | Symlink fixtures created by an elevated run of `probe-fixtures.ps1`; probes ran unelevated |

## Fixture results

Values are bytes. "Dir entry" is `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` on the parent folder, which doesn't open the file. "Handle" is `FileStandardInfo` on a `FILE_READ_ATTRIBUTES` handle opened with `OPEN_REPARSE_POINT | OPEN_NO_RECALL`. "GCFS" is `GetCompressedFileSizeW`.

| Case | Length | Dir entry alloc | Handle alloc | GCFS | Notes |
|---|---:|---:|---:|---:|---|
| Empty | 0 | 0 | 0 | 0 | |
| 100-byte file (MFT-resident) | 100 | 104 | 104 | 100 | Resident data uses no clusters; 104 is the 8-byte-aligned resident size |
| Ordinary 1 MiB + 1 | 1,048,577 | 1,052,672 | 1,052,672 | 1,048,577 | GCFS returns length, not cluster-rounded allocation |
| NTFS-compressed 16 MiB text | 16,777,228 | 2,101,248 | 2,101,248 | 2,101,248 | |
| Sparse, 1 GiB length, 64 KiB data | 1,073,741,824 | 65,536 | 65,536 | 65,536 | |
| Hard link, alias grown through other name | 1,048,576 | **262,144 (stale)** | 1,048,576 | 1,048,576 | Path-based calls or an open through the stale name refresh the entry |
| 1 MiB alternate data stream | 11 (main) | 16 | 16 | 11 | **ADS bytes invisible to all three** |
| Hidden + system | 4,096 | 4,096 | 4,096 | 4,096 | Enumerated normally |
| Junction, junction loop | — | — | — | — | Tag `MOUNT_POINT`; not descended into |
| Directory symlink | — | — | — | — | Tag `SYMLINK`; not descended into |
| File symlink → 1 MiB file | 0 | 0 | 0 | **1,048,577** | Enumerated as a file entry with tag `SYMLINK`; GCFS follows the link to its target |

- **Hard-link identity:** the file ID from the dir entry is identical across hard-link aliases and equals the handle's `FileIdInfo` ID. On NTFS, the upper 64 bits are zero.
- **Cloud Files on a non-placeholder:** `CfGetPlaceholderInfo` returns `ERROR_INVALID_FUNCTION` for files that aren't cloud placeholders.

## Cloud placeholders: OneDrive Files On-Demand

Two files were marked "Free up space" (online-only): a 228,020,224-byte `.aup3` and a 1,556,710-byte `.mp3`. Each was snapshotted before and after every step, with a 5 s settle at the end.

| Observation | Result |
|---|---|
| Dir entry | `AllocationSize` 0, `EndOfFile` = full length, attributes `ARCHIVE \| UNPINNED \| RECALL_ON_DATA_ACCESS` (0x500020) |
| Reparse tag | **Hidden.** No `REPARSE_POINT` attribute or tag in the dir entry or `FileAttributeTagInfo`; `CfGetPlaceholderStateFromAttributeTag` reports no placeholder state. This matches the Cloud Files default compatibility mode, which disguises placeholders for unpackaged processes |
| `CfGetPlaceholderInfo` | `OnDiskDataSize` 0, `ValidatedDataSize` 0, pin `UNPINNED`, `IN_SYNC` |
| `GetCompressedFileSizeW` | 0 |
| Attribute handle with `OPEN_NO_RECALL` | Allocation 0; **no hydration** |
| Attribute handle without `OPEN_NO_RECALL` (`.mp3`) | Allocation 0; **no hydration** |
| State after all steps + 5 s | Unchanged for both files |

After Files On-Demand was enabled, a whole-tree walk showed the folder's dir-entry allocation dropping by exactly the 228 MB freed. 196 files carry `PINNED` and 198 carry `UNPINNED`.

## Real tree: OneDrive folder (fully local, 47,146 files, 8,938 dirs, 243 GB)

Single-threaded release build with a warm cache.

| Mode | Time | Files/s |
|---|---:|---:|
| Dir-entry enumeration only | 686 ms | ~68,700 |
| + `GetCompressedFileSizeW` per file | 4,432 ms | ~10,600 |
| + attribute handle per file | 4,532 ms | ~10,400 |

- Dir-entry allocation matched handle allocation for **all 47,146 files**.
- GCFS equaled the length for 46,379 files and the allocation only for the 767 sparse, compressed, or exact-cluster files.
- The handle walk found 10 multiply-linked files. 6 junctions (`MOUNT_POINT`) were skipped, and 15 sparse files were present.
- No attribute changes were observed between enumeration and open.

## Decisions for the Windows scanner (milestone 2)

1. **Allocated size = dir-entry `AllocationSize`.** It handles sparse and compressed files correctly and needs no open. Don't use `GetCompressedFileSizeW` as the allocation metric.
2. **Hard-link accounting by dir-entry file ID.** Use a per-scan set of `(volume, file ID)` to detect aliases within the scan without opening files. When an ID repeats, open that file once (attributes only, `OPEN_NO_RECALL`) to get its authoritative size, because alias dir entries can be stale. Aliases whose other names lie outside the scan root are counted as unique; document this as entry-based.
3. **Don't open every file.** Opening each one costs about 6.5× the throughput for no accuracy gain in practice.
4. **Alternate data streams are not counted.** Document the limitation in the status/detail explanation of why totals differ from volume usage. Revisit only if the user asks for it, since it needs a per-file open plus `FileStreamInfo`.
5. **Reparse handling:** skip and count `MOUNT_POINT` and `SYMLINK` entries, **including file symlinks**, which enumerate as file entries. Descend into cloud-tag directories. Never use path-based size APIs on entries, because they follow links.
6. **Cloud files:** allocated = dir-entry `AllocationSize` (0 for online-only), logical = `EndOfFile`. Detect cloud state from the `RECALL_ON_DATA_ACCESS`, `RECALL_ON_OPEN`, `PINNED`, and `UNPINNED` attributes, because reparse tags are disguised by default. Enumeration and attribute-only opens don't hydrate.

## Not yet validated

- **Cloud edge states:** partially hydrated files, hydration in progress, and files modified locally but not yet uploaded. Providers other than OneDrive.
- **Placeholder compatibility mode:** behavior when a process opts in to exposed placeholders (`RtlSetProcessPlaceholderCompatibilityMode`). Not needed if attribute-based detection is used.
- **Volume mount points (a volume mounted into an NTFS folder):** need elevation to create a VHD.
- **Other filesystems:** ReFS, exFAT/FAT32 (`FileIdExtdDirectoryInfo` may be unsupported), SMB shares.
- **macOS probes:** type-check only; not run. Linux results are in [`linux-m1.md`](linux-m1.md).

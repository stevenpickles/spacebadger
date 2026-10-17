# Raw MFT read: the elevated gate

Date: 2026-10-17. Branch `feature/62/mft-probe`. Tool: `sb-probe mft` (this branch). Fixtures: `scripts/probe-fixtures.ps1`.

Design idea 2.3 (scan an NTFS volume by reading its master file table once, when the app runs elevated) is gated on this probe: the engine path isn't built until the gate below has passed on this machine, elevated. The probe was written and unit-tested unelevated; **the gate itself has not been run yet** because it needs an administrator prompt. This page says exactly how to run it and what counts as a pass.

## What the probe does

```text
sb-probe mft <volume> [--compare <root>] [--threads N] [--chunk-mib N]
                      [--lag-secs N] [--samples N] [--show N]
```

1. Refuses, with exit code 3 and nothing read, when the drive is a network drive, isn't NTFS, or the process isn't elevated. It never asks for elevation.
2. Opens `\\.\C:` with `GENERIC_READ`, sharing reading and writing, unbuffered. The only control codes it sends are queries: `FSCTL_GET_NTFS_VOLUME_DATA`, `FSCTL_QUERY_USN_JOURNAL`, `FSCTL_READ_USN_JOURNAL`, `FSCTL_GET_NTFS_FILE_RECORD`. Nothing is written, flushed or locked, and no file is opened for its data.
3. Records the journal's next USN, reads record 0 (`$MFT`), applies the update-sequence fixups, follows its `$DATA` run list (into its extension records if `$MFT` has an attribute list), and reads the table's extents in order in 8 MiB reads (`--chunk-mib`, 4–16), parsing on several threads (`--threads`, default the core count up to 8).
4. Per record: `$STANDARD_INFORMATION` (attributes, USN), every `$FILE_NAME` but DOS aliases (name and parent; several names are hard links), the unnamed `$DATA` (resident length rounded up to 8 as listings report it; non-resident allocated size, or the total allocated for sparse and compressed files), WOF's compressed stream, the `$REPARSE_POINT` tag, and the `$ATTRIBUTE_LIST`. Extension records are merged into their base by base reference. A record whose update sequence doesn't check out (torn by a write during the read) is read again through NTFS with `FSCTL_GET_NTFS_FILE_RECORD`. That copy may come back already fixed up. Applying the fixups again is then a no-op: a stride tail that happens to equal the check value is replaced by the saved original, which it already holds. The update-sequence array must cover the record exactly (`length / 512 + 1` entries), or the record is malformed. Run lists must lie on the volume, and sizes read from disk are bounded before anything is allocated from them.
5. Builds the flat record table and walks it from the root (record 5), through directories only, with bottom-up totals; metafiles (records 0–15, `$Extend` and below) are never reached.
6. With `--compare <root>`, lists `<root>` with the production listing adapter (`sb_scan::NativeFs`, the same code the app scans with, backup privilege enabled), then reads the journal and compares node by node: file ID, kind (by the same `classify` rules), cloud flag, length and allocation for files, and the reparse tag (from an attributes-only `OPEN_REPARSE_POINT | OPEN_NO_RECALL` open) for every reparse point or cloud entry. Name and parent are the match key.
7. Prints a JSON report on stdout. Exit 0 when read (and with `--compare`, in agreement), 1 when the comparison disagrees or a read fails, 2 for bad usage.

Differences are allowed only for records the USN journal shows changed from the start of the read to the end of the listing walk, or in the `--lag-secs` window (default 60 s) before the read: a raw read sees the table as the lazy writer last flushed it, which can trail the files by seconds.

Some listing behaviours are counted under `comparison.explained` rather than as disagreements, each documented in [windows-m1.md](windows-m1.md) or below. Every size mismatch is re-measured with the adapter's per-file query. It counts as explained only when the re-measure equals the record; otherwise it fails.

- `hard_link_remeasured`: a hard-linked file's listed sizes were stale (the directory entry of a name it wasn't written through), and the re-measure, which the engine already does for such files, agreed with the record.
- `listing_stale_remeasured`: the same for a single-name file. NTFS updates the directory entries of files that are open and growing (logs, databases) lazily, so a live `C:\` always has some.
- `cloud_tag_disguised`: a cloud placeholder's reparse tag is hidden from ordinary processes by the Cloud Files filter; the record has the real cloud tag, and both sides call it a cloud file. Two cloud tag variants (`CLOUD` and `CLOUD_n`) count as the same tag.
- `wof_compressed_stream`: a WOF-compressed file whose listed allocation equals its `WofCompressedData` stream's rather than the unnamed stream's. Whether listings do this hasn't been observed yet; a non-zero count tells the engine path which allocation to report.

## Running the gate

All commands from the repository root. Steps 1–2 unelevated, the rest in an **elevated** terminal (Run as administrator).

1. Build: `cargo build --release -p sb-probe`
2. Fixtures (Developer Mode for the symlinks; the script only creates the folder it's given and refuses one that exists):

   ```powershell
   pwsh scripts/probe-fixtures.ps1 $env:TEMP\sb-mft-fixtures
   ```

   Free up space on two or three OneDrive files (right-click, "Free up space") so there are online-only placeholders.
3. **Cold read** (gate a): restart Windows, open an elevated terminal before anything else heavy runs, and:

   ```powershell
   target\release\sb-probe.exe mft C: > mft-cold.json
   ```

4. **Fixtures and OneDrive** (gate c). Run these before the `C:\` compare. That compare re-measures the stale `hardlink-b.bin`, which refreshes its directory entry, so a later fixture run would no longer show the stale case. If the order slips, delete the fixtures folder and run step 2 again.

   ```powershell
   target\release\sb-probe.exe mft C: --compare $env:TEMP\sb-mft-fixtures --show 50 > mft-fixtures.json
   target\release\sb-probe.exe mft C: --compare $env:OneDrive --show 0 > mft-onedrive.json
   ```

5. **Whole-volume agreement** (gate b). Takes as long as a listing of `C:\` (about 30–90 s):

   ```powershell
   target\release\sb-probe.exe mft C: --compare C:\ > mft-compare-c.json
   ```

Leave the machine otherwise idle during steps 4 and 5 so few records change; changed ones are allowed but each one is a node not verified.

## What counts as a pass

| Gate | Where to look | Pass |
|---|---|---|
| (a) read and parse ≤ 4 s cold | `mft-cold.json`: `timing.read_parse_wall_s`, `read_parse_within_4s` | ≤ 4.0 s, `true` |
| (a, reported alongside) read, parse and build against the same 4 s | `mft-cold.json`: `timing.read_parse_build_s`, `read_parse_build_within_4s` | report both; the engine path will be judged on this total |
| (b) 100 % agreement on unchanged records | `mft-compare-c.json`: exit code, `comparison.pass`, `comparison.disagreements`, `comparison.unverified`, `comparison.listing_failures`, `changes.lost` | exit 0; `pass: true`; `disagreements: {}`; `unverified: 0`; `listing_failures: 0`; `lost: false` |
| (b) few nodes excused as changed | `mft-compare-c.json`: `comparison.changed_within_limit` (`comparison.changed_ratio` is the sum of `changed_during_run` over `matched`) | `true`, i.e. under 1 % of matched nodes; more leaves too much unverified, so re-run on a quieter machine. This is not part of `pass` or the exit code, so exit 0 alone is not enough |
| (c) fixtures identical | `mft-fixtures.json`: as (b), plus the `comparison.nodes` values in the table below | as (b), and every value below |
| (c) OneDrive identical | `mft-onedrive.json`: as (b); `explained.cloud_tag_disguised` ≥ the placeholders freed in step 2 | as (b) |

Also report from `mft-compare-c.json` (the design asks for them; they don't pass or fail): `records.attribute_list_records`, `records.nonresident_attribute_lists` (records an engine would re-read by file ID), `records.failed_first_pass` and `reread_*` (torn records), `records.not_under_root` (metafiles and orphans), `comparison.explained`, `comparison.changed_during_run`, and the totals `comparison.totals_mft` against `comparison.totals_listing` (equal but for changed records).

Expected fixture values in `comparison.nodes` (from the listing measurements in [windows-m1.md](windows-m1.md)):

| Path under the fixtures | kind | logical | allocated | Notes |
|---|---|---:|---:|---|
| `empty.txt` | File | 0 | 0 | |
| `tiny.txt` | File | 100 | 104 | resident, rounded to 8 |
| `ordinary.bin` | File | 1,048,577 | 1,052,672 | |
| `compressed.txt` | File | 16,777,228 | 2,101,248 | total allocated; may differ slightly with compression, but must equal the listing |
| `sparse.bin` | File | 1,073,741,824 | 65,536 | total allocated |
| `hardlink-a.bin`, `hardlink-b.bin` | File | 1,048,576 | 1,048,576 | same record (`names: 2`); `explained.hard_link_remeasured` = 1 for the stale `b` entry, if the fixtures were compared before anything re-measured them (step 4) |
| `ads.txt` | File | 11 | 16 | the 1 MiB stream isn't counted (`records.alternate_streams_allocated` has it) |
| `hidden-system.bin` | File | 4,096 | 4,096 | |
| `junction`, `loop\back` | MountPoint | | | tag `0xa0000003`, not descended |
| `dir-symlink`, `file-symlink.bin` | Symlink | | | tag `0xa000000c` |

## If it fails

Send the JSON files. `comparison.samples` holds up to `--samples` (default 20) differences per field with the path, record number, and both values; `--samples 200` gives more. A failure in (b) or (c) means the engine path stays unbuilt until the parser is fixed and the gate re-run; a pass on this machine is the first of the "more than one machine" the design asks for before the setting defaults on.

## Measured so far (unelevated)

- Unelevated, `sb-probe mft C:` and `sb-probe mft C: --compare C:\Users` both refuse with "reading C:'s master file table needs an elevated (administrator) prompt; nothing was read" and exit 3 **[measured]**.
- Parser cost on synthetic 1 KiB records (one name, resident data), release build, one thread, machine loaded by other builds: 0.47–0.69 µs per record to fix up and parse, 0.28–0.43 µs per record to build the table **[measured]**. For `C:\`'s ~4.5M records that's ~0.3–0.4 s of parsing on 8 threads, beside the sequential read **[estimated]**; the table build is single-threaded (~1.3–1.9 s **[estimated]**) and reported separately as `timing.build_s`, outside gate (a).
- The reader was exercised on a synthetic volume image: `$MFT` in three extents with a negative run offset, chunk sizes smaller than an extent and straddling extent boundaries, and a torn record (unit test `reads_a_fragmented_table_from_a_volume_image`).

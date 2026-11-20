# Very wide folders

Date: 2026-10-14. Branch `feature/30/bounded-listings`. Same machine as [milestone 5](scale-m5.md): AMD Ryzen 9 5900X (24 threads), 64 GiB RAM, Samsung 980 PRO NVMe, Windows 11 Pro 10.0.26200, NTFS. Release builds.

A review found that the scanner bounded its queue by directory count, not size: an adapter returned a whole directory as one `Vec<Entry>`, and the aggregator added it to the tree with the write lock held throughout, without checking for a cancel. One folder of millions of entries could therefore spike memory and block every layout until it was done. The existing scale measurements didn't cover this case.

## Benchmark

```text
cargo run --release -p sb-bench -- --wide 1000000 --wide 5000000 --wide-dir <folder>
```

`--wide N` scans a synthetic folder of N entries through the real engine: files with distinct sizes and IDs, every file possibly hard-linked (as Windows listings report), and an empty subfolder per thousand entries. `--wide-dir` scans a real folder with the native adapter; here, a folder of 500,000 empty files on NTFS. Each case runs in its own process. Measured:

- **Peak memory**: the process's peak working set.
- **Longest reader wait**: a thread takes the tree's read lock every millisecond, as layouts do, and records how long each wait took. The longest wait bounds the longest write-lock hold a reader ran into.
- **Cancel**: a second scan is cancelled 10 ms after the first entries reach the aggregator; the time is from `cancel()` to the scan returning.

Two runs each; the range is shown where they differ.

| Case | Measure | Before | After |
|---|---|---|---|
| Synthetic, 1M entries | Peak memory (tree 65 MiB) | 273 MiB | 167 MiB |
| | Longest reader wait | 248–304 ms | 4.8–5.8 ms |
| | Cancel acknowledged | 250–279 ms (all 999,000 files added first) | 2.1–3.6 ms (after ~60,000) |
| | Scan time | 341–402 ms | 253 ms |
| Synthetic, 5M entries | Peak memory (tree 520 MiB) | 1,207 MiB | 633–644 MiB |
| | Longest reader wait | 1,368–1,405 ms | 40–46 ms |
| | Cancel acknowledged | 1,419–1,582 ms | 2.3–4.9 ms |
| | Scan time | 1.79–1.83 s | 1.30–1.42 s |
| Real NTFS folder, 500,000 files | Peak memory (tree 32 MiB) | 140 MiB | 91–92 MiB |
| | Longest reader wait | 118–137 ms | 2.7–3.6 ms |
| | Cancel acknowledged | 103–123 ms (completed instead: the whole listing was added first) | 1.6–2.9 ms (cancelled after ~33,000) |
| | Scan time | 341–382 ms | 226–244 ms |

Before, a whole listing waited in memory until it was added, and on Windows it existed twice at its peak (the decoded records, then the entries); after, each worker holds at most two batches of 4,096 entries and the channel at most four per worker.

### What was changed

1. **Batched listings.** `FsAdapter::read_dir` hands entries to a sink in batches of at most 4,096 as it reads them, instead of returning the directory. Windows passes entries on as each 64 KiB `GetFileInformationByHandleEx` buffer is decoded, Linux per `getdents`/`statx` entry, the portable Unix adapter per `readdir` entry. The sink returns `Break` to stop a listing; the worker does so when the scan is cancelled. Each batch is one message in the bounded channel, so the channel bounds entries rather than listings, and the aggregator takes the write lock once per batch and checks for a cancel between batches.
2. **Hard-link owners outside the lock.** With batching alone, the 5M case still had reader waits of 137–160 ms. Instrumenting the lock showed them at 0.23M, 0.46M, 0.92M, 1.8M, and 3.7M nodes: the owner map (one entry per file on Windows, where every listed file may be shared) growing, 139 ms at 3.7M entries. The map is no longer grown with the lock held (see the follow-up below for how aliases are marked).

The remaining 40 ms at 5M entries is the tree's own arrays doubling as they pass 4.2M nodes (21 ms at 2.1M, 11 ms at 1M). That happens in any scan of that many nodes, wide folders or not, and is left as is: avoiding it means growing the tree's arrays off the lock.

## Correctness

- A folder becomes `Listed` only with its last batch. The last batch travels with the listing's end, so most directories still cost one message and one lock.
- A listing that fails after some batches keeps the entries already added and their subfolders are scanned; the folder is `Listed` with unreadable entries, so `Tree::fully_listed` is false, comparisons treat it as read in part (nothing in it is called removed), and the map and details show it as not fully read (see the follow-up below). The error is counted under its omission reason with the folder's path. A folder that fails before any entry is `Failed`, as before, and a root that fails part way no longer fails the scan.
- A listing stopped by a cancel stays pending and ends as not scanned, keeping the batches already applied, and the scan reports Cancelled even if that listing was its last work.
- If the tree's capacity runs out mid-listing, the folder ends as not scanned (it was marked listed before). Hard-linked files whose names were listed with different sizes keep their listed sizes then: their re-measures run only once every folder is listed, which a stopped scan never reaches (a cancel likewise).
- On Windows the fallback from `FileIdExtdDirectoryInfo` to `FileIdBothDirectoryInfo` happens only before any entry was read, so no entry is listed twice.

Tests: batch sizes, the final partial batch, entries before an error, and stopping (adapter unit tests); a real folder of 8,199 files listed in bounded batches with each name once, and stopping when asked (native test); and, through the fake filesystem, hard links and omissions across batches, a folder and the root failing part way, and a cancel while a wide listing is blocked between batches (engine tests).

## Follow-up from review

A review of the change found three problems, now fixed:

1. **A cancel could be reported as Complete.** A worker that saw the cancel flag mid-listing told the aggregator it had stopped, which counted the job as done. If that was the last outstanding job and the aggregator hadn't yet seen the flag itself, it declared the scan Complete: the folder stayed Pending, and the app would have saved the truncated scan as a snapshot to compare against. The same could happen with a job skipped for a cancel, which predates this change. Dropped work now marks the scan cancelled, and completion is declared only after checking the cancel flag. A deterministic test reproduces the race (it failed before the fix), and another cancels 40 scans at spread-out moments and checks that no folder is left pending and that a cancelled scan never reports Complete.
2. **Folders read in part looked complete.** A folder whose listing failed part way, or with entries that couldn't be read, is `Listed` but not fully listed. The map now draws it with the incomplete outline, like failed and cancelled folders. The details say "Partly read; some of its contents couldn't be read" (a new `partial` folder state in the protocol) instead of "Scanned", and the tooltip says "Not fully scanned" without "yet".
3. **Aliases were briefly counted twice.** Hard-link aliases were marked in a second lock hold after their batch, so readers could briefly see a linked file's allocation twice. Known owners are now looked up before taking the lock, names sharing a file within the batch are matched under the lock through a map bounded by the batch, and only recording new owners (which grows the large map) happens after release. The lock waits are unchanged (5.8–6.8 ms at 1M entries, 47 ms at 5M).

`--wide-slow N` lists the synthetic folder at 2 ms per batch, so listing is slower than adding to the tree, and a cancel usually lands while the aggregator is idle (the case the race affected). Measured after the fixes and after merging per-node filesystem IDs from `dev` (which raise the tree estimate to 73 MiB at 1M nodes and 584 MiB at 5M), two runs each:

| Case | Scan time | Peak memory | Longest reader wait | Cancel |
|---|---|---|---|---|
| 1M entries | 339–341 ms | 173–175 MiB | 5.8–5.9 ms | Cancelled in 3.1–3.2 ms |
| 5M entries | 1.81–1.95 s | 666–667 MiB | 47 ms | Cancelled in 2.6–3.3 ms |
| 1M entries, slow listing | 671–672 ms | 179–180 MiB | 7.0–7.1 ms | Cancelled in 1.1–1.5 ms |
| 5M entries, slow listing | 3.45–3.48 s | 682 MiB | 47 ms | Cancelled in 1.7–3.1 ms |

Scan times are higher than in the first table because other builds were running on the machine; the fast cases ran before and after the slow ones in the same runs.

## Ordinary trees

`C:\Program Files` (356,900 files, 32,547 folders) with the `sb-scan` `scan` example, warm, 15 alternating runs each: before median 0.64 s (fastest 0.58 s), after median 0.65 s (fastest 0.60 s), within run-to-run noise. A first version reserved a full 4,096-entry batch for every directory and was 11% slower here; batches now grow as filled.

## Desktop app

Release build (`npx tauri build --no-bundle`): the 500,000-file folder scans in 0.3 s and the window answers clicks straight after (the files are empty, so the map is empty and the panel reports 500,000 files with no size); `C:\Program Files` scans in 0.7 s with a normal map. The listing is too quick to interact with mid-scan on a local SSD; the reader-wait figures above stand in for that.

## Not covered

- Slow network shares, where a wide listing takes longest and batching matters most to how soon its entries appear.
- Linux and macOS timings; both adapters build and pass clippy for their targets, and the Linux one is exercised by the same tests in CI.

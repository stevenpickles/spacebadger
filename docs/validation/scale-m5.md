# Milestone 5 — scale

Date: 2026-10-09. Branch `feature/10/scale-bench`.

Benchmark machine: AMD Ryzen 9 5900X (12 cores, 24 threads), 64 GiB RAM, Samsung 980 PRO 2 TB NVMe, Windows 11 Pro 10.0.26200, NTFS. Release builds.

## Synthetic trees (`sb-bench`)

```text
cargo run --release -p sb-bench                  # 1M and 5M nodes
cargo run --release -p sb-bench -- --nodes 2000000 --seed 7
```

Trees are built folder by folder, as the scanner adds them. About 4.6 files per folder; folder and file sizes have heavy tails; names average 20 bytes, with 3% non-ASCII and a weighted mix of common extensions. Each size runs in its own process, so peak memory is per size. Seed 1:

| Measure | 1M nodes | 5M nodes |
|---|---|---|
| Files / folders | 815,158 / 184,842 | 4,111,187 / 888,813 |
| Build tree | 316 ms | 1.63 s |
| Peak process memory | 121 MiB | 548 MiB |
| Root layout 1920×1080 (first / stable / final sort) | 7.3 / 5.9 / 6.6 ms, 3,273 rects | 20.0 / 19.4 / 16.3 ms, 6,448 rects |
| Root layout, logical, fresh cache | 7.5 ms | 20.1 ms |
| Root layout on the wire | 141 KiB, 0.2 ms to encode | 271 KiB, 0.3 ms |
| Widest folder (~20,000 children): layout / list its small items | 0.8 / 0.6 ms | 0.9 / 0.4 ms |
| Search `.pdf` (18k / 91k matches) | 35 ms | 173 ms |
| Search `e` (599k / 3.0M matches) | 50 ms | 271 ms |
| Search with no matches | 20 ms | 112 ms |
| Filtered root layout, `e` | 4.9 ms | 14.1 ms |
| Result list, `e`: full sort / first page | 19.0 / 2.5 ms | 126 / 18 ms |
| Catch up a search after 50,000 new files | 11 ms | 23 ms |

### Tuning done

- **Result ranking.** The result list re-sorted every match whenever matching totals changed, which happens continuously during a scan. At 3M matches that was 126 ms per refresh, holding the search lock that map layouts also need. Matches are now ranked lazily: the largest *n* are selected in linear time and only that prefix is sorted. The first page now costs 18 ms.
- No other change was needed for these targets. Layouts stay within 20 ms at 5M nodes because only the current view is laid out (bounded by the 30,000-rectangle budget), and the wire format stays under 300 KiB.

## Real disk, whole desktop app

Release build scanning `C:\` (2,964,268 files, 592,283 folders, 1.41 TiB allocated), measured from outside the app. Memory was sampled every ~2 s across the app process and all its WebView2 processes. Interaction was clicks and arrow keys posted to the map about three times a second throughout, with the timing overlay on.

| Measure | Result | Brief target |
|---|---|---|
| Full scan | 28.3 s | — |
| First useful map | Detailed, labelled map on screen 1.0 s after launch (0.8 s into the scan) | Within 1 s of the first batch |
| Layout round trip (request → decoded) | p50 9.0, p95 19.5, max 37.9 ms (174 layouts) | — |
| Draw a frame | p50 5.2, p95 5.7, max 6.5 ms | ≥ 30 fps (≤ 33 ms) |
| Input to next frame | p50 6.2, p95 31.0, max 37.9 ms (500 inputs) | Most within 100 ms |
| Peak memory, app + webview, during scan | 865 MiB working set, 794 MiB private | Under 1 GiB |
| Memory after the scan | 716 MiB working set; private 302 MiB app + 254 MiB webview | Under 1 GiB |

After the scan, private memory by process: app 302 MiB; WebView2 GPU 133, renderer 64, browser 35, utilities 19, crash handler 3 MiB. Working sets add up to more than private memory because they count shared pages in every process.

The app process peaks at 431 MiB during the scan (work queues and listing buffers) and settles at 283 MiB, about 80 bytes per node.

### Bug found during measurement

The small-items list stayed open after the selection moved to an unrelated item. It now closes unless the selected item is in the list's folder.

## Not covered here

- 5M nodes on a real disk: the largest real tree available is the 3.56M-node `C:\` above. The 5M figures are synthetic.
- Linux and macOS performance.
- Slow network shares (open question 2 in the plan).

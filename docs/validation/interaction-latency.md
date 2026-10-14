# Interaction latency while scanning

Date: 2026-10-14. Branch `feature/36/query-metrics`. Same machine as [milestone 5](scale-m5.md): AMD Ryzen 9 5900X (24 threads), 64 GiB RAM, Windows 11 Pro 10.0.26200. Release builds.

An architecture review found that interactive reads can do expensive maintenance while holding shared locks: every query with a filter catches the search up with the scan (`current_search` calls `catch_up(tree, usize::MAX)`) holding the search mutex and the tree's read lock, folder ranking and suggestion indexing work against the shared tree, and the `async` query commands run synchronous work and blocking lock waits on the runtime's workers. The round-trip times measured so far combine all of these. This is the baseline for restructuring the execution; it measures each cause separately.

## What is measured

Timings are kept only when `SPACEBADGER_TRACE` is set; otherwise each helper costs one atomic load.

- **Backend** (`src-tauri/src/metrics.rs`). Each query command is timed as a request on the thread that does its work: its queue time where it can be measured (from asking to starting, for the commands on blocking threads), the tree's read lock and the session's mutexes (search, layout order, folder files, names) as wait and hold, and stages: `catch-up` (with the backlog of nodes and changes it had to examine), `compute` (layout), `encode` (rectangles and the wire format), `rank` (`small_items`, `folder_files`, `search_results`), `index` (suggestions), and `examine` (`search_set`'s stepped pass). Each is kept per command in log-linear histograms (8 buckets per doubling, so percentiles are within 12.5%), and the 5 slowest requests of each command are kept with their breakdown. The report is written at exit to the file `SPACEBADGER_TRACE` names (or to standard error with `1`, which Windows release builds don't have, since they run without a console; empty or `0` is off), and the `trace_report` command returns it.
- **Interface** (`ui/src/lib/metrics.ts`). Round trips of the query commands (`api.ts`) and decoding layouts (`layoutWire.ts`). `timed`, `measure`, and `record` are there for the map to time painting in a later change. In the developer tools, `await spacebadgerTrace.full()` prints the interface's table and then the backend's, so a round trip can be compared with the backend's work; the difference is IPC, serialization, and, for the commands that run on runtime workers, the queue time the backend can't see.
- **Load scenario** (`src-tauri/src/session/load.rs`, an ignored test). A synthetic scan of 3.2 million nodes (8 folder levels, about 25 files per folder with a long tail, 20,000 files directly in the root, a `.log` file in about 20, listed with 0.4 ms per folder over 8 workers, so it takes about 10 s) runs while the queries the interface makes are issued at its rates: on every scan status (150 ms) the layout of the root (1600×1000), the filter's totals, the details of the selected folder, and the Contents list of the root (`small_items` with all children, or `folder_files`, 200 rows); and a tooltip (`node_details` of a random node) every 50 ms. Each kind waits for its previous request, like the interface's coalescing. Requests run on the same Tauri async runtime as the commands, on workers for the `async` commands and on blocking threads for `folder_files` and `search_set`. Latency is from asking to the reply, without IPC.

```text
cargo test -p spacebadger --release --lib interaction_latency -- --ignored --nocapture
```

Four scenarios: the filter `*.log` typed as the scan starts with the Contents list showing folders, the same with Contents showing files only, no filter, and the filter typed 5 s into the scan. Each prints the tables below and the backend report.

## Baseline

Two runs, latency in ms while scanning (about 65–75 requests per kind, 200 tooltips, so p99 is close to the largest):

| Scenario | Request | p50 | p95 | p99 |
|---|---|---|---|---|
| Filter on, Contents with folders | layout | 7.7–9.1 | 9.8–11.1 | 11.4–18.1 |
| | node_details (selection) | 1.9–5.6 | 9.5–10.8 | 11.1–12.0 |
| | search_summary | 5.1–7.1 | 8.7–10.4 | 10.1–17.3 |
| | small_items (Contents) | 2.7–6.7 | 10.0–10.7 | 11.3–12.0 |
| | tooltip | 0.1 | 7.8–10.0 | 10.7–11.1 |
| Filter on, Contents with files only | folder_files (Contents) | 15.5–25.3 | 36.3–39.5 | 39.6–49.5 |
| | layout | 8.6–11.1 | 32.6–32.8 | 41.5–48.0 |
| | node_details (selection) | 9.1–11.2 | 32.9–38.0 | 48.0–49.4 |
| | search_summary | 3.8–9.8 | 23.8–39.3 | 35.9–41.6 |
| | tooltip | 0.1 | 13.8–14.8 | 23.1–30.8 |
| No filter, Contents with folders | layout | 14.4–15.2 | 19.8–23.2 | 20.6–25.5 |
| | node_details (selection) | 0.1–12.0 | 9.6–19.7 | 14.4–20.3 |
| | small_items (Contents) | 0.9–13.0 | 20.2–20.6 | 20.7–24.9 |
| | tooltip | 0.1 | 16.7–17.3 | 20.1–22.5 |
| Filter typed 5 s into the scan | search_set | 51.0–52.2 | | |
| | layout | 9.4–9.8 | 15.7–15.9 | 18.3–29.1 |
| | small_items (Contents) | 8.2–8.8 | 15.0–16.0 | 18.1–28.7 |
| | tooltip | 0.1 | 10.9–13.1 | 15.7–16.2 |

After the scan, with the filter on and Contents showing folders: layout 6.5–8.9 ms at p50 and 8.9–10.9 ms at p99; the other requests keep a p95 of 7–10 ms, waiting for the layout as below. Without a filter, everything but the layout settles below 1.1 ms.

Where the slowest requests' time goes, first run, filter on, Contents with folders, while scanning: the mean of each part over the requests at or above the p95 (and p99) latency, in ms; `backlog` is the nodes and changes the search had to catch up on.

| request | p95 | p99 | tail | queue | search wait | tree wait | catch-up | compute | rank | encode | tree hold | search hold | backlog |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| layout | 11.1 | 18.1 | ≥p95 | 0.1 | 4.0 | 0.0 | 0.5 | 6.9 | 0.0 | 1.1 | 8.5 | 8.5 | 11,606 |
| | | | ≥p99 | 0.1 | 12.0 | 0.0 | 0.0 | 4.8 | 0.0 | 1.0 | 5.8 | 5.8 | 9 |
| node_details | 10.8 | 12.0 | ≥p95 | 0.1 | 11.1 | 0.0 | 0.0 | | | | 0.0 | 0.0 | 9 |
| search_summary | 10.4 | 17.3 | ≥p95 | 0.0 | 12.2 | 0.0 | 0.0 | | | | 0.1 | 0.1 | 14 |
| small_items | 10.7 | 12.0 | ≥p95 | 0.1 | 10.7 | 0.0 | 0.3 | | 0.2 | | 0.5 | 0.5 | 10,993 |
| | | | ≥p99 | 0.0 | 10.7 | 0.0 | 1.1 | | 0.1 | | 1.2 | 1.2 | 43,914 |
| tooltip | 10.0 | 10.7 | ≥p95 | 0.0 | 9.3 | 1.0 | 0.0 | | | | 0.0 | 1.0 | 0 |

The same with Contents showing files only:

| request | p95 | p99 | tail | search wait | catch-up | compute | rank | encode | tree hold | search hold |
|---|---|---|---|---|---|---|---|---|---|---|
| folder_files | 39.5 | 49.5 | ≥p95 | 12.1 | 0.0 | | 29.4 | | 30.4 | 30.4 |
| layout | 32.8 | 41.5 | ≥p95 | 26.6 | 0.0 | 9.1 | | 1.1 | 10.2 | 10.2 |
| node_details | 38.0 | 49.4 | ≥p95 | 41.7 | 0.0 | | | | 0.0 | 0.0 |
| search_summary | 39.3 | 41.6 | ≥p95 | 40.4 | 0.0 | | | | 0.0 | 0.0 |
| tooltip | 13.8 | 30.8 | ≥p95 | 23.2 | 0.0 | | | | 0.0 | 1.8 |

Without a filter, the tail is the tree's read lock instead: small_items 16.6 ms (≥p95) to 23.9 ms (≥p99) waiting for the tree, tooltips 13.0–15.5 ms, details 5.7–14.3 ms, while the layout holds it for 23–25 ms. After the scan, with no writer, the same requests wait for nothing.

## Top contributors to p95/p99

1. **The search mutex is held for the whole layout.** With a filter, `layout` takes the search mutex, then the tree, and keeps both through compute and encoding (7–10 ms). Every other request takes the search mutex first, so they all queue behind each layout: the search wait is essentially all of the p95/p99 of node_details, search_summary, small_items, and tooltips. A tooltip asks for no filter, yet `details` still takes the search mutex unconditionally. This holds after the scan too.
2. **`folder_files` ranks the whole folder again on every scan revision.** Its cache is keyed by revision, so while scanning each request ranks all files under the root again: 29–35 ms of `rank` with the search mutex, the tree, and the folder-files mutex held. Behind it, layout, details, and the filter's totals reach 33–49 ms at p95/p99, and tooltips 23–31 ms.
3. **A long tree read hold stalls readers through the waiting scanner.** Without a filter there's no search mutex in the way, but the unfiltered layout holds the tree's read lock for 23–25 ms. The scanner's write then waits for it, and new readers wait behind the queued writer (Windows SRW locks don't let readers past a waiting writer), so tooltips and the Contents list wait 13–24 ms for the tree while scanning and not at all afterwards. The engine's write holds aren't timed yet; the reader waits show their effect.
4. **Layout compute** is the largest stage itself: 5–9 ms filtered, 15–24 ms unfiltered for this tree at 1600×1000. Encoding adds 0.7–1.8 ms.
5. **Search catch-up is small at interactive rates.** With requests every 150 ms the backlog averages 2,000–5,000 nodes and peaked at 46,000 (1.9 ms); about 40 ns per node. It is unbounded, though: after the interface goes quiet (window hidden, no status refreshes), the first query pays for everything scanned meanwhile, at about 12 ms per second of scanning at this scan rate, with both locks held. Typing a filter mid-scan took 51–52 ms, but `search_set` examines in steps without the search mutex, so other requests weren't held up by it.
6. **Queue time is negligible here**: p95 under 0.1 ms and at most 0.8 ms on 24 threads, because the load doesn't saturate the runtime. On a machine with fewer cores, synchronous lock waits on workers could add to it; the queue is recorded per request to show that.

Not covered by this baseline: IPC transfer, JSON serialization, decoding, and painting, which need the real app (`SPACEBADGER_TRACE` and `spacebadgerTrace.full()`). The app was only checked to write the report at exit.

## Recommendations for the restructuring

- Don't hold the search mutex across layout compute and encoding. Snapshot what the layout needs (an `Arc` of the caught-up overlay, or a read lock with catch-up taking a short write), and have `details` take the search lock only when it's given a search.
- Catch the search up in the background after scan batches, in bounded steps, instead of in reads; reads use the latest caught-up version, so the filtered map lags by at most a step.
- Cache `folder_files` and `small_items` rankings across revisions while scanning (rank again at most every few hundred ms, or incrementally), and rank off the locks.
- Shorten tree read holds: copy what a layout needs and encode after releasing the lock, or bound each hold, so the scanner's writes, and the readers behind them, aren't held up. Time the engine's write holds too.
- Run tooltips and details ahead of layouts and rankings, so the cheapest requests never wait behind the most expensive.
- Judge the change with this scenario: the target is tooltips, details, and the Contents list under 16 ms at p99 while scanning with a filter, which today reach 11–12 ms with Contents showing folders and 23–49 ms with files only.

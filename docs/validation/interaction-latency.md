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

### After the layout orders only what it draws

Run again once #39 (the layout orders only the children it draws) and #41 (revisions read under the tree lock) were merged into `dev`, one run, p95/p99 in ms while scanning:

| Scenario | layout | details | Contents | tooltip |
|---|---|---|---|---|
| Filter on, Contents with folders | 12.2 / 15.9 | 12.3 / 15.6 | 12.0 / 13.8 | 9.4 / 12.0 |
| Filter on, Contents with files only | 32.0 / 34.5 | 31.1 / 34.3 | 33.0 / 35.3 | 29.0 / 33.0 |
| No filter, Contents with folders | 7.5 / 28.5 | 7.1 / 22.3 | 7.2 / 28.8 | 6.1 / 8.0 |
| Filter typed 5 s into the scan | 9.1 / 9.8 | 9.1 / 9.8 | 8.9 / 9.5 | 7.8 / 9.6 |

The unfiltered layout's compute fell from 15–24 ms to about 5.4 ms, which removes most of contributor 3 at p95. Its p99 is a single stall of about 22 ms, and it reaches the other requests through the search mutex: the layout takes it before the tree, so while it waits for the tree, everything that takes the search mutex waits too. With a filter, contributors 1 and 2 are unchanged.

### After the Contents list keeps its rankings

Contributor 2. `small_items` and `folder_files` now share a cache of rankings (`sb_core::ranking`) keyed by folder, list (children or files under it), metric, and filter, each kept with the revision, scan state, time, and folder totals (matching totals with a filter) it was made from. After the scan a ranking is served only at the revision it was made at, so final pages are exactly a full sort's and never list deleted items; while scanning it's served as long as the folder's totals haven't changed, so scanning elsewhere doesn't invalidate it, and otherwise for 400 ms. A ranking is made by a resumable walk that looks at 100,000 nodes per hold of the search mutex and the tree's read lock (about 1 ms), letting go between holds; for the files under a folder it keeps only the largest asked for (at least 1,000, doubling for deeper pages) in a bounded heap, so neither the walk nor the kept ranking holds every file. While scanning, a request walks at most 400,000 nodes of an out-of-date ranking and then serves the one kept, so a large folder is ranked again over a few requests; after the scan, and when nothing is kept yet, a request walks to the end. Rankings take at most 2^20 items (16 bytes each) and 16 entries, least recently used dropped first. Replies carry the revision of the ranking (`revision`, as added by #38), so pages of one ranking agree and a newer one makes the Contents tab refresh.

Measured on a busier machine than the baseline (other builds running: scans took 12–14 s rather than 10 s), so `dev` (849316a) was run again alternately with this change, two runs each, p95 / p99 in ms while scanning (about 80–90 requests per kind, so p99 is close to the largest):

| Scenario | Request | Before | After |
|---|---|---|---|
| Filter on, Contents with files only | folder_files (Contents) | 35.4–36.6 / 43.8 | 12.2–14.8 / 16.1–18.9 |
| | layout | 10.6–37.0 / 21.1–50.7 | 11.0–16.2 / 17.3–21.8 |
| | node_details | 18.4–34.7 / 27.3–40.2 | 9.5–16.2 / 16.6–45.7 |
| | search_summary | 2.4–33.9 / 31.4–42.8 | 3.3–10.0 / 12.1–27.2 |
| | tooltip | 2.4–29.7 / 26.3–42.2 | 8.5–12.7 / 12.7–16.8 |
| Filter on, Contents with folders | small_items (Contents) | 3.3–14.2 / 22.0–69.4 | 2.3–12.0 / 24.4–39.1 |
| | layout | 12.2–15.7 / 35.3–70.2 | 12.1–12.9 / 19.7–39.4 |
| | node_details | 4.8–10.5 / 31.8–78.6 | 4.2–8.1 / 10.6–11.9 |
| | tooltip | 2.7–3.0 / 21.8–32.2 | 2.4–3.7 / 6.6–8.7 |

With files only, `folder_files` spends 4–6 ms ranking in a request that walks (a 1 ms hold at a time) and nothing in one served from the cache, instead of 26–38 ms in a single hold every request, and the 30–40 ms search-mutex waits it caused the other requests are gone. What's left of the tail is the filtered layout's 7–10 ms hold of the search mutex (contributor 1): with a walk's steps interleaved, the slowest requests still wait 8–16 ms for it, and single stalls of 25–45 ms (waits for the tree or the runtime, on both sides of the comparison, more often on this loaded machine) set most p99s. The target of 16 ms at p99 is met at p95 and by the Contents request itself in one of two runs; it needs contributor 1 fixed too (the search overlay restructuring). With folders shown, the ranking was already cheap (0.1–0.4 ms) and is now usually served from the cache; that scenario's tail is contributor 1 alone.

After the scan, the first request ranks the whole folder once, in steps (about 25–30 ms for the root of this tree, waiting for no one), so `folder_files`' p99 after the scan is that one request; the rest are served from the cache in about 1 ms.

Randomized tests compare walked, cached, and partial pages with a full sort (ties, filters, both metrics, deeper pages, deletes between pages, walks across a growing tree), and test the staleness rules and the memory bound.

### After the filter is kept up in the background

Branch `feature/39/derived-views`, merged with `dev` at `cb60f40` (the ranking cache and the treemap changes included); contributor 1 and part of 5. Requests no longer catch the search up. Instead, the search sits behind a read-write lock shared with a job. After each scan status, the job advances the search 10,000 nodes per step. It read-locks the tree first and only tries the search's write lock, backing off for 1 ms while requests read. After 20 tries in a row it queues for the lock instead, so requests that overlap without a gap can't stall the search indefinitely. Requests read-lock the search only when they're given one, and tooltips never touch it. So layouts, details, totals, and the Contents list run side by side. The layout releases the search before encoding, and the tree before serializing. While scanning, a request may see the search as of an earlier status (`SearchSummary::revision` says which). Once the scan has finished, a request whose search is behind the tree's revision catches it up exclusively first, so final results are unchanged. The job's steps take 0.25 ms on average, 0.36 ms at p95, and at most 2.7 ms (timed as `search_catch_up` in the report).

`dev` (`cb60f40`) and this branch were run alternately, three times each, on a quiet machine (scans of 9.0–9.7 s). The table gives ranges over the runs, with the median in brackets, in ms:

| Scenario | Request | Before p95 | After p95 | Before p99 | After p99 |
|---|---|---|---|---|---|
| Filter on, Contents with folders | tooltip | 5.2–6.1 (5.3) | 3.9–4.5 (4.0) | 6.6–7.2 (6.8) | 5.3–7.0 (5.7) |
| | node_details | 5.7–7.2 (6.5) | 4.2–5.5 (5.2) | 6.3–8.8 (8.3) | 7.2–16.9 (7.8) |
| | small_items (Contents) | 6.3–7.5 (6.7) | 5.0–6.8 (5.2) | 7.3–11.4 (8.1) | 7.3–21.8 (11.3) |
| | search_summary | 6.6–6.9 (6.7) | 5.0–5.5 (5.2) | 7.1–11.4 (7.6) | 7.2–17.4 (7.8) |
| | layout | 7.0–8.5 (7.0) | 5.3–7.2 (6.8) | 8.0–11.4 (8.4) | 11.8–22.7 (12.7) |
| Filter on, Contents with files only | tooltip | 8.1–10.5 (8.8) | 3.8–5.7 (4.8) | 9.2–11.5 (10.4) | 4.9–7.2 (5.8) |
| | node_details | 8.7–11.5 (9.9) | 3.9–5.9 (4.8) | 9.1–13.2 (10.9) | 4.9–6.5 (5.3) |
| | folder_files (Contents) | 8.3–10.4 (8.9) | 7.2–9.1 (7.6) | 9.2–11.3 (10.2) | 8.4–9.8 (9.1) |
| | layout | 9.0–11.6 (9.8) | 5.3–7.5 (5.4) | 10.0–13.2 (10.9) | 5.9–7.8 (6.0) |
| Filter typed 5 s into the scan | tooltip | 1.4–6.7 (3.4) | 0.3–3.6 (3.5) | 5.6–7.6 (5.9) | 4.1–6.3 (4.1) |
| | node_details | 1.8–7.2 (5.1) | 3.8–4.6 (4.0) | 6.5–11.3 (8.0) | 4.4–6.3 (5.1) |
| | small_items (Contents) | 5.4–6.2 (5.6) | 4.2–5.9 (4.4) | 6.1–11.3 (8.3) | 4.4–6.0 (4.7) |

At the median, details while scanning come back in 0.1 ms instead of 3.3 ms with Contents showing folders, and in 0.8 ms instead of 5.0 ms with files only. After the scan, with the filter on, details, the filter's totals, tooltips, and the Contents list (folders) answer in under 0.5 ms at p99, where before they took 5.8–7.9 ms waiting for the layout; with files only, tooltips and details fall from about 19.5 ms at p99 to 0.2–0.3 ms.

Target: tooltips, details, and the Contents list under 16 ms at p99 while scanning with a filter.

- **Met at the median of three runs, for every scenario.** The worst medians are 11.3 ms (Contents, folders) and 9.1 ms (Contents, files only).
- **One run each exceeded it for details (16.9 ms) and the Contents list (21.8 ms).** Both came from a single stall in the first scenario: the whole time went to waiting for the tree while the scanner held it for writing. The job holds nothing at that point.

Timing the scanner's write holds (a temporary probe in `apply_batch`, not committed) on a busy machine explains these stalls:

- **33–55 ms at 2,097,152 nodes and 16–33 ms at 1,048,576**, when the tree's column vectors double and are copied (Windows' allocator copies on growth).
- **10–27 ms at other sizes**, as the name buffer grows.

A request that arrives during one of these holds waits for it, and so do the requests behind it. Before, those requests also waited for the search mutex, which hid the stall within the same tail.

The same comparison against the earlier `dev` (`af911b3`, before the ranking cache) had been made while other processes kept all 24 hardware threads busy (a run queue of about 40). It showed the same picture with larger numbers. While scanning, details took 0.1 ms at the median instead of 2.4 ms. After the scan, details took 2.3 ms at p95 instead of 10.6 ms. The p99 while scanning, however, was set by write stalls, which descheduling stretched to 25–80 ms.

Remaining contributors, in order:

1. The scanner's write holds while the tree grows. Fixing them belongs to the engine: segmented columns, or reserving from the previous snapshot's size, would remove the large ones.
2. Readers waiting behind a writer, which in turn is queued behind the layout's 5–7 ms read hold (contributor 3). A prioritized executor would shorten this.

The completion index is now built in steps of 50,000 nodes, with the tree's lock released between steps. While scanning, an index past its five seconds keeps answering while a new one is built on its own thread. Typing in the filter therefore no longer holds the scanner up for a whole build (431 ms for 5M nodes). A delete matches the comparison again before announcing the change, rather than leaving that to the first tooltip after it.

Not covered by this baseline: IPC transfer, JSON serialization, decoding, and painting, which need the real app (`SPACEBADGER_TRACE` and `spacebadgerTrace.full()`). The app was only checked to write the report at exit.

## Recommendations for the restructuring

- Don't hold the search mutex across layout compute and encoding. Snapshot what the layout needs (an `Arc` of the caught-up overlay, or a read lock with catch-up taking a short write), and have `details` take the search lock only when it's given a search.
- Catch the search up in the background after scan batches, in bounded steps, instead of in reads; reads use the latest caught-up version, so the filtered map lags by at most a step.
- Cache `folder_files` and `small_items` rankings across revisions while scanning (rank again at most every few hundred ms, or incrementally), and rank off the locks.
- Shorten tree read holds: copy what a layout needs and encode after releasing the lock, or bound each hold, so the scanner's writes, and the readers behind them, aren't held up. Time the engine's write holds too.
- Run tooltips and details ahead of layouts and rankings, so the cheapest requests never wait behind the most expensive.
- Judge the change with this scenario: the target is tooltips, details, and the Contents list under 16 ms at p99 while scanning with a filter, which today reach 11–12 ms with Contents showing folders and 23–49 ms with files only.

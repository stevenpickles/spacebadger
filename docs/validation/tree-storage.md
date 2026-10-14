# Chunked tree storage

Date: 2026-10-14. Branch `feature/42/tree-storage`. Same machine as [milestone 5](scale-m5.md): AMD Ryzen 9 5900X (24 threads), 64 GiB RAM, Windows 11 Pro 10.0.26200. Release builds. Other agents' builds were running on the machine at times, so every comparison below alternates the two builds, run by run.

The scan tree kept each node column (parent, links, name offsets, flags, sizes, times, IDs) in a `Vec`, and names in one byte `Vec`. The scanner adds each batch of entries with the tree's write lock held, so whenever a column doubled, copying it held the lock: a temporary probe measured holds of 33–55 ms at 2,097,152 nodes, 16–33 ms at 1,048,576, and 10–27 ms when the name buffer grew, and [wide folders](wide-dirs.md) left a 40–45 ms reader wait at 4.2M nodes. With the search lock no longer serializing requests (#45), these stalls set the p99 of tooltips, details, and the Contents list, which wait for the tree's read lock.

## Measuring the holds

- `ScanConfig::write_holds` takes an optional `HoldProbe`, told after each of the scanner's write-lock holds how long it lasted and how many nodes the tree had. `None` (the default) costs nothing.
- `sb-bench --wide N` installs one and prints the longest and 99th-percentile hold overall and by tree size, in bands doubling from 256K nodes, with the five longest and where they happened.
- In the app, `SPACEBADGER_TRACE` installs one too: the holds appear in the report as `- tree write hold`, outside any request, beside the waits they cause.
- `sb-bench` (the default `--nodes` cases) now also times what the Contents list asks for: the top 1,000 files under the root and the root's children, each in one walk.

## Change

Every column is a list of fixed-size chunks of 16,384 nodes (`Column<T>`), allocated whole when the chunk's first node is added and never moved or grown; node `i` is at slot `i % 16384` of chunk `i / 16384`. Names go into chunks of 1 MiB; a name never spans two (names are at most 65,535 bytes, so one always fits a fresh chunk), and its 32-bit start is the chunk above bit 20 and the offset below, which keeps the 4 GiB limit on names. Only the first name chunk grows (by doubling, up to 1 MiB), so a small tree's names stay small. Adding a node therefore allocates at most one chunk per column (about 1 MiB in all, mostly zero pages) and copies nothing.

Growing the columns off the lock instead (allocating and copying under a read lock, then swapping under a short write lock) was rejected: the scanner writes between the copy and the swap, so the copy would need to catch up under the write lock, and its memory peak is still two copies of the tree.

Reads stay O(1) but go through one more load, the chunk's address. The public accessors check the node against the tree's length once (as indexing a flat column did) and then read the chunk without a second bounds check: with the second check, the search results' first page (a partial sort by size of 600,000 matches) measured 2.5 times slower than before; without it, 1.1–1.2 times. Walking a folder's children keeps the chunk of the last sibling, since siblings are mostly added together, so a long list costs one dependent load per child rather than two: that brought laying out a folder of 5 million files from 1.75 times to 1.15 times the time before. The public API is unchanged.

## Write holds and reader waits

`cargo run --release -p sb-bench -- --wide 1000000 --wide 5000000`, three runs of each build alternately (one folder of N entries, so batches of up to 4,096 entries per hold; a reader takes the read lock every millisecond):

| Case | Measure | Before | After |
|---|---|---|---|
| 1M entries | Longest write hold | 7.2–8.2 ms (at 512K nodes) | 0.8–1.0 ms |
| | p99 write hold | 0.8–1.0 ms | 0.6–0.7 ms |
| | Longest reader wait | 6.2–7.8 ms | 0.5–0.7 ms |
| | Scan time | 361–430 ms | 297–317 ms |
| | Peak memory (tree estimate) | 183 MiB (73) | 176 MiB (71) |
| 5M entries | Longest write hold | 49–88 ms (at 4,096K nodes) | 1.2–1.3 ms |
| | p99 write hold | 0.8–1.2 ms | 0.6–0.7 ms |
| | Longest reader wait | 49–87 ms | 0.6–1.3 ms |
| | Scan time | 2.05–2.43 s | 1.56–1.71 s |
| | Peak memory (tree estimate) | 663–678 MiB (584) | 671–679 MiB (350) |

Longest write hold by tree size, 5M entries:

| Nodes | Before | After |
|---|---|---|
| 0–256K | 2.6–3.2 ms | 0.9–1.2 ms |
| 256K–512K | 4.1–4.7 ms | 0.8–1.1 ms |
| 512K–1M | 7.6–8.9 ms | 0.7–1.3 ms |
| 1M–2M | 13.2–13.7 ms | 0.8–1.2 ms |
| 2M–4M | 25.0–25.9 ms | 0.8–1.0 ms |
| 4M–8M | 49–88 ms | 0.7–0.9 ms |

Before, the longest hold in each band was the doubling at its start and grew with the tree; after, it's a batch's own work, about 1 ms for 4,096 entries, at any size. The tree estimate (`Tree::memory_bytes`) counted vector capacity, so it fell from 584 to 350 MiB at 5M nodes; the process's peak is unchanged, since a doubling's spare capacity was mostly never touched. At most one partly used chunk per column is wasted (about 1 MiB), against up to half of each column with doubling.

## Throughput

`cargo run --release -p sb-bench` (1M and 5M synthetic nodes) and `--lod 1000000 --lod 5000000`, three and two runs of each build alternately, ms:

| Case | Before | After |
|---|---|---|
| 1M: build tree | 332–347 | 318–343 |
| 1M: root layout (first, stable repeat) | 1.3, 0.8–0.9 | 1.2–1.3, 0.8 |
| 1M: top 1,000 files under the root (Contents walk) | 4.6–4.7 | 5.0–5.2 |
| 1M: search `.pdf`, `e`, `dir:a` | 39–40, 59–60, 62–63 | 41–42, 65–67, 68–70 |
| 1M: `e` matches (598,832) full sort, first page | 16.6–16.8, 2.0 | 20.5–20.7, 2.4–2.7 |
| 1M: search catch-up of 50,000 new nodes | 11.8–12.5 | 11.1–11.4 |
| 5M: build tree | 1,739–1,791 | 1,661–1,703 |
| 5M: root layout (first, stable repeat) | 2.8, 1.8 | 2.9–3.0, 1.9–2.0 |
| 5M: top 1,000 files under the root | 24.7–25.0 | 27.3–27.8 |
| 5M: search `.pdf`, `e`, `dir:a` | 206–255, 320–387, 428–441 | 268–285, 427–435, 409–483 |
| 5M: `e` full sort, first page | 118–174, 19–22 | 181–231, 20–28 |
| 5M: suggestion index | 411–425 | 439–492 |
| 1M / 5M: folder of equal small files, final layout | 1.4 / 7.2 | 1.6–1.7 / 8.4 |
| 1M / 5M: mixed tree, 2,001 views laid out | 21.7–21.9 / 41.6–42.0 | 19.9–22.4 / 41.6–42.1 |
| Peak memory, 1M / 5M | 158–159 / 804–805 MiB | 159–160 / 818–819 MiB |

Building, catch-up, and laying out the root and many views are unchanged within noise. Reads now go through one more load, and where it sits on a chain of dependent loads it shows: searching 1M nodes is 5–10% slower, sorting matches by size 15–25% (a weight read per comparison, in no particular order), the Contents walk 7–11%, and a folder of millions of files 15–20% (both follow sibling links). The 5M search times vary run to run by more than the difference (other builds on the machine); at 1M the runs are steady. Peak memory at 5M is 1.7% higher here and unchanged within noise in the wide-folder runs; the tree's own estimate fell by a third.

## Interaction latency

`cargo test -p spacebadger --release --lib interaction_latency -- --ignored --nocapture` (3.2M nodes, about 10 s of scanning), two runs of each build alternately. This branch is based on `dev` before #45, so the search mutex still sets most p99s here; the scanner's holds and the tree waits are what this change affects:

| Scenario | Measure | Before | After |
|---|---|---|---|
| All four | Longest tree write hold | 23.6–37.1 ms | 0.75–1.88 ms |
| | p99 tree write hold | 0.02–0.03 ms | 0.02–0.03 ms |
| Filter on, Contents with files only | Tooltip: longest tree wait | 8.6–21.6 ms | 0.6–1.0 ms |
| | folder_files: longest tree wait | 5.7–12.0 ms | 5.1–8.1 ms |
| Filter typed 5 s into the scan | Tooltip: longest tree wait | 13.1–28.3 ms | 4.5 ms |
| | Tooltip: p95 / p99 / max latency | 5.6–8.1 / 8.8–9.1 / 13.2–28.5 ms | 4.6–5.3 / 7.4–8.2 / 8.5–9.2 ms |
| No filter, Contents with folders | Tooltip: p95 / p99 / max latency | 3.4–4.7 / 4.0–6.1 / 4.1–6.8 ms | 3.6–5.4 / 3.8–5.9 / 4.0–7.7 ms |

The tooltip's slowest requests were the ones that met a doubling; those are gone. The remaining tree waits of 4–8 ms are other readers': the lock lets a waiting writer go first, so a reader can wait for a layout's or a ranking step's hold to end and then the scanner's batch. With the search mutex out of the way (#45), these and the 1 ms batches are what's left on the tree.

## Not covered

- The hard-link owner map (`alias_owner`) and the list of changed files still grow by doubling under the lock. Both are small in practice (one entry per hard-link alias or re-measure), but a tree with hundreds of thousands of aliases would see a few ms when they grow.
- Cancelling marks every pending folder in one hold, a pass over every node: a few ms at 5M nodes, once.
- Linux and macOS timings; the change is platform-independent and covered by the same tests in CI.

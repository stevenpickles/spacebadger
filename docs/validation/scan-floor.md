# Scan floor

Date: 2026-10-17. Branch `feature/58/scan-floor`. Phase 0 of the scan-speed work: a ruler that says how far the scan engine is from what the filesystem allows, so each speed change can be gated on it.

Machine: AMD Ryzen 9 5900X (12 cores, 24 threads), 64 GiB RAM, Samsung 980 PRO NVMe, Windows 11 Pro 10.0.26300, NTFS, not elevated. Release build. **The machine was heavily loaded throughout** (several other agents compiling; CPU load 90 %+ during the runs), so wall times below are 1.5–3× what a quiet machine gives. All numbers are measured.

## How to run it

```text
cargo build --release -p sb-bench
target/release/sb-bench --engine <root> --threads 16 --runs 5
target/release/sb-bench --floor <root> --threads 16 [--strategy current|ntdll|relative]... --runs 5
```

- `--floor <root>` lists the folder with N threads and nothing else: open a folder, list it, count its entries, queue its subfolders. No tree, no aggregation, no hard-link work, one 64 KiB buffer per thread. It descends into the same folders the engine does (not links, mount points or other name-surrogate reparse points; cloud-tagged folders are descended; on Unix not other filesystems), so its entry count matches the engine's files + folders + omissions to within what changed on disk between runs (616,350 vs 616,356 on the work tree).
- Strategies: `current` is the adapter's calls today (on Windows `CreateFileW` by `\\?\` path with `FILE_LIST_DIRECTORY`, share all, `BACKUP_SEMANTICS | OPEN_REPARSE_POINT | OPEN_NO_RECALL`, then `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`; on Linux `read_dir` and a `statx` per entry relative to the folder; on macOS `read_dir` and `lstat` per entry). Windows also has `ntdll` (same opens, `NtQueryDirectoryFile` direct) and `relative` (`NtOpenFile` relative to the parent's handle with the name from the listing, then `NtQueryDirectoryFile`; `FILE_OPEN_NO_RECALL` is left out because NTFS rejects it with `FILE_DIRECTORY_FILE` as an invalid parameter).
- `--engine <root>` alternates a floor run (the first `--strategy`, default `current`) and a real engine scan (`Scan` with `NativeFs`, the given worker count, a `HoldProbe` counting write holds) `--runs` times, each in its own child process, then prints a summary.
- `--threads` defaults to the engine's default worker count (8 on this machine) and may be repeated. The coordinator's gates use 16.
- Per run it prints wall time, entries, folders, queries, per-folder open time, per-query time (and the end-of-listing query), thread time summed over threads, and process kernel/user CPU (`GetProcessTimes`, `getrusage`). The engine run adds write-hold count, total and longest, and CPU per thread role (aggregator, workers, other) from the names `sb-scan` already gives its threads, so **no change to `sb-scan` was needed**. On Windows the bench enumerates its own threads every 5 ms with `NtGetNextThread` and keeps each handle open, so a thread's final CPU time is read exactly after it exits; on Linux it samples `/proc/self/task/*/stat`; macOS reports process CPU only. Windows CPU times tick at 15.6 ms.
- `--wide-dir` also prints process CPU and CPU per role now.
- Every child prints a `result:` line and every summary a `summary:` line of `key=value` pairs, for scripts: `ratio_of_mins`, `ratio_median`, `ratio_spread_pct`, `cpu_ratio_median`, `cpu_ratio_spread_pct`, and the medians and minimums.

## Today's numbers

### Work tree (`C:\Users\Steven\work`, ~616K entries, 73.4K folders, 61.5K hard-link aliases)

16 threads, 5 alternating runs each:

| | Min | Median | Max | Spread (max−min)/median |
|---|---:|---:|---:|---:|
| Floor wall | 1,325 ms | 2,584 ms | 3,879 ms | 99 % |
| Engine wall | 5,490 ms | 7,335 ms | 9,253 ms | 51 % |
| Engine/floor wall, per run (2.12, 5.20, 6.02, 2.72, 2.39) | 2.12 | 2.72 | 6.02 | 143 % |
| Floor CPU (kernel + user) | 13.8 s | 15.0 s | 16.1 s | 16 % |
| Engine CPU | 39.3 s | 42.6 s | 44.6 s | 12 % |
| Engine/floor CPU, per run (2.81, 3.05, 3.05, 2.44, 2.85) | 2.44 | 2.85 | 3.05 | 21 % |

8 threads (today's default), 5 alternating runs each:

| | Min | Median | Max | Spread |
|---|---:|---:|---:|---:|
| Floor wall | 2,745 ms | 2,972 ms | 5,629 ms | 97 % |
| Engine wall | 10,330 ms | 13,309 ms | 18,298 ms | 60 % |
| Engine/floor wall, per run (4.85, 2.72, 6.22, 4.29, 3.15) | 2.72 | 4.29 | 6.22 | 82 % |
| Floor CPU | 12.8 s | 13.0 s | 14.3 s | 11 % |
| Engine CPU | 38.3 s | 40.3 s | 41.6 s | 8 % |
| Engine/floor CPU, per run (2.94, 2.99, 3.10, 2.90, 2.92) | 2.90 | **2.94** | 3.10 | **6.7 %** |

Where the engine's CPU goes (16 workers, 5 runs): workers 37.7–42.8 s (kernel 34.8–38.4 s, user 2.9–4.4 s), aggregator 1.44–1.73 s (kernel 0.25–0.67 s, user 1.0–1.33 s), everything else under 0.1 s. Write holds 264.7K per scan, 0.61–0.93 s held in total (7–13 % of the scan), longest 2.6–48 ms. The floor's kernel CPU is 12.3–14.9 s, so the engine spends ~24 s more kernel time than listing needs: the per-file hard-link re-measures the design attributes it to.

Floor strategies, 16 threads, 5 runs each (run as three consecutive blocks, not interleaved, so load drift between blocks is not controlled for; read them as "no strategy is broken", not as a ranking):

| Strategy | Wall min / median / max | CPU median |
|---|---|---:|
| `current` | 1,043 / 1,150 / 1,886 ms | 13.7 s |
| `ntdll` | 1,472 / 1,717 / 2,052 ms | 15.6 s |
| `relative` | 1,112 / 1,344 / 2,699 ms | 14.7 s |

Per-folder open time at 16 threads was 106–263 µs across runs (83 µs on a quieter machine in the design's measurements), per query 34–63 µs.

### `C:\` (4.33M entries, 636K folders, 431.9K hard-link aliases)

16 threads, 5 alternating runs each:

| | Min | Median | Max | Spread |
|---|---:|---:|---:|---:|
| Floor wall | 14.2 s | 19.6 s | 26.6 s | 63 % |
| Engine wall | 40.1 s | 65.6 s | 110.6 s | 107 % |
| Engine/floor wall, per run (2.82, 4.57, 2.46, 4.42, 2.05) | 2.05 | 2.82 | 4.57 | 89 % |
| Floor CPU | 137.3 s | 140.2 s | 142.8 s | 4.0 % |
| Engine CPU | 288.5 s | 300.3 s | 309.3 s | 6.9 % |
| Engine/floor CPU, per run (2.22, 2.17, 2.11, 2.10, 2.09) | 2.09 | **2.11** | 2.22 | **5.9 %** |

Engine CPU by role: workers 277–295 s, aggregator 11.3–13.7 s (kernel 2.6 s, user 9.5 s in the last run). 1.86M write holds, 4.8 s held in total (12 %), longest 17.7 ms. The floor could not list 597–600 folders (protected folders, unelevated; under 0.1 % of folders; the engine counts them among its 3,372 omissions).

The aggregator's 12 s of CPU per `C:\` scan is more than the design's estimate (~8 s); with the walk at ~10 s on a quiet machine it is close to being the bottleneck once the workers get faster.

## Reproducibility

The target was engine/floor within 10 % run to run.

- **CPU ratio: met on this loaded machine** for `C:\` (5.9 %) and the work tree at 8 threads (6.7 %); 21 % for the work tree at 16 threads, where one run (2.44) was an outlier and the other four sit within 8 %. CPU time measures the work done rather than when it got a core, so other processes' load moves it far less than wall time (floor CPU spread 4–16 % against 63–99 % for wall time).
- **Wall ratio: not met here** (82–143 %). Other agents' builds came and went within single runs; a 1.3 s floor run and a 3.9 s one were minutes apart. This needs a quiet machine; the coordinator re-measures there before merging.
- Contended CPU isn't free of load either: kernel time per folder open rises with concurrency on NTFS (33 µs single-threaded to 83 µs at 16 threads on a quieter machine, from the design's probe), and other processes' file activity adds to that contention.

How to gate a change with it:

1. On a quiet machine: `--engine <root> --threads 16 --runs 5` for the base build and the candidate, alternating builds, and compare `ratio_median` (wall). Spread should be within 10 %; if not, the machine wasn't quiet.
2. On a busy machine: compare `cpu_ratio_median` instead, and treat the wall `ratio_of_mins` (fastest engine run over fastest floor run; load only ever adds time) as a rough cross-check. A change that saves syscalls (hard-link rule, relative opens) shows in the CPU ratio; a change that only improves parallelism (more workers) shows only in wall time and needs the quiet machine.
3. The work tree changes as agents work in it (entries grew from 587K to 618K over the session), so compare builds on the same day, alternating, never against a number from this page.

Raw outputs of the runs above are kept in the session's scratch folder (`agent-floor\engine-work.txt`, `floor-work.txt`, `engine-c.txt`).

## Tests

- The floor counts a known tree (40 folders, 160 files) identically with every strategy at 1 and 4 threads, and a missing root is one error, not a hang; on Unix a symlinked folder is counted and not followed.
- CPU per role attributes two named busy threads to aggregator and workers.
- Result lines parse and the spread is (max − min) / median.

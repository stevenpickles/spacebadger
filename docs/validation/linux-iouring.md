# Linux listing: larger getdents64 buffers and io_uring statx

Date: 2026-10-17. Branch `feature/63/linux-iouring`. Idea 2.7 of the scan-speed design, run as its gating experiment. **Result: neither change reaches the 1.3× warm gate, so the Linux adapter stays as it is.**

## Environment

| Item | Value |
|---|---|
| Host | Docker Desktop on Windows 11 Pro 10.0.26300, AMD Ryzen 9 5900X (24 threads), 64 GiB RAM, Samsung 980 PRO NVMe |
| Kernel | 6.18.33.2-microsoft-standard-WSL2 (the Docker Desktop VM), 24 CPUs and 31 GiB visible |
| Container | `rust:1.97.1-bookworm`, `--privileged` (Docker's default seccomp profile blocks `io_uring_setup`) |
| Filesystems | **ext4**: a Docker named volume (`/dev/sdd`, `statfs` reports ext2/3/4), the main measurement. **overlayfs**: the container's root filesystem, over the same ext4. No Windows bind mount (which would measure 9p/virtiofs). |
| Load | Other agents were compiling on the host throughout, so timings are noisy; min and median are given, and runs were interleaved. |

## Tree

A synthetic tree built inside the container: 100 top folders, each with 50 empty files and 20 subfolders; subfolder *i* holds 10 + (7919·*i* mod 991) files (10–1,000, mean ~505), every 16th file 1,500 bytes, plus one hard link and one symlink. In total 1,027,660 entries: 1,023,560 regular files, 2,100 folders, 2,000 symlinks.

## Prototype

A standalone harness (rustix `=1.1.5`, features `fs`, `io_uring`, `mm`; no other crates) walks the tree with N threads sharing a directory queue. Each directory is opened as the adapter does (`openat(O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`), every entry is `statx`ed relative to the directory fd with `AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT` and the adapter's mask (`BASIC_STATS | MNT_ID | BTIME`), and an owned name and per-entry record are built as the adapter does. Three listing modes:

- **base**: today's adapter, `rustix::fs::Dir` (a small heap buffer growing in 768-byte steps to ~24 KiB, allocated per directory) and one `statx` syscall per entry.
- **buf**: `rustix::fs::RawDir` over a 64 KiB per-thread buffer, one `statx` syscall per entry.
- **uring**: the 64 KiB `RawDir`, with names gathered into batches of 32–256 and submitted as `IORING_OP_STATX` SQEs on a per-thread ring (raw `io_uring_setup`/`mmap`/`io_uring_enter` from rustix), reaped with one `io_uring_enter(GETEVENTS)` per batch. Variants with `IORING_SETUP_SINGLE_ISSUER | COOP_TASKRUN` were also run.

rustix's `io_uring` feature is enough: it provides the syscalls, the SQE/CQE/params layouts and `IoringOp::Statx`; the ring mapping (~100 lines of unsafe code with `rustix::mm::mmap`) is left to the caller. No new crate would have been needed.

All modes produced identical totals on every run (entries, files, folders, others, sum of sizes, sum of blocks, an XOR of inode numbers and a sum of modification times), with no failures.

## Results

Seconds per walk of the whole tree; lower is better. Measured.

### Warm, ext4

| Threads | base | buf | uring (batch) |
|---:|---|---|---|
| 1 (5 runs) | min 1.975, median 2.068 | min 1.852, median 2.116 | 32: min 3.516, median 3.973; 256: min 2.405, median 3.323; 128 + single/coop: min 1.908, median 2.679 |
| 8 (7 runs) | min 0.276, median 0.325 | min 0.260, median 0.296 | 32: min 0.706, median 0.960; 256: min 0.424, median 0.573; 128 + single/coop: min 0.461, median 0.593 |
| 8 (9 runs, heavier host load) | min 0.471, median 0.599 | min 0.412, median 0.601 | 128: min 1.102, median 1.573; 256: min 0.607, median 1.352 |
| 8 (11 runs, heavier host load) | min 0.739, median 0.915 | min 0.716, median 0.815 | — |
| 16 (7 runs) | min 0.301, median 0.368 | min 0.309, median 0.363 | 256: min 0.858, median 1.104 |

The engine's default on this machine is 8 workers (`cores.clamp(2, 8)`).

### Warm, overlayfs

| Threads | base | buf | uring (256) |
|---:|---|---|---|
| 8 (7 runs) | min 0.395, median 0.465 | min 0.374, median 0.466 | min 0.655, median 0.804 |
| 1 (5 runs) | min 2.965, median 3.465 | min 3.016, median 3.492 | min 3.024, median 3.926 |

### Cold, ext4

The privileged container can write `/proc/sys/vm/drop_caches`, so each walk below followed `sync; echo 3 > /proc/sys/vm/drop_caches`. This empties the Docker VM's page, dentry and inode caches; the VM's disk is a VHDX file on the Windows host, which may still have parts of it cached, so these are "cold Linux caches", not a cold NVMe.

| Threads | base | buf | uring (batch) |
|---:|---|---|---|
| 8 (5 runs) | min 1.332, median 1.538 | min 1.513, median 1.554 | 128: min 1.330, median 1.507; 256: min 1.261, median 1.428 |
| 1 (3 runs) | min 10.566, median 13.539 | min 10.018, median 16.145 | 256: min 6.193, median 7.069 |

## Why io_uring loses

The kernel runs `IORING_OP_STATX` only in its io-wq worker threads (the request is always punted to async context, because path lookup can block). While a single-threaded uring walk ran, the process had 7 `iou-wrk` threads beside its own two. So every warm `statx`, which would otherwise be a short syscall served from the dentry and inode caches, becomes a hand-off to a kernel worker, a wake-up and a completion post: 1.5–3× slower than the plain syscall, worst with small batches. Batching saves syscall entries, but syscall entry is not what a warm `statx` costs here.

The same mechanism helps when the inode reads are slow: with one thread and cold caches the ring keeps many lookups in flight and walks in about half the time. But the engine already runs 8 workers, and at 8 threads the cold gain shrinks to 1.0–1.08× (within noise), because 8 workers already keep the device queue busy on this tree.

## Decision

- **io_uring batched statx: not adopted.** Warm it is 1.5–3× slower than per-entry `statx` at every thread count measured (gate: ≥ 1.3× faster); cold at the engine's 8 workers it is within noise. It would also add an unsafe ring implementation and a fallback path (Docker's default seccomp profile and some hardened kernels refuse `io_uring_setup`) for no gain.
- **64 KiB getdents64 buffer: not adopted.** 0.88–1.14× by min and 0.84–1.12× by median across the warm and cold sets, with no consistent direction between runs: below the gate and within this host's noise. The tree's folders average ~500 entries, so `Dir`'s growing buffer already needs few `getdents64` calls; the gain could be larger on folders of tens of thousands of entries, but not enough to reach 1.3× on a whole scan, where `statx` dominates.
- Linux listing stays at one `statx` per entry, as in the [milestone 1 decisions](linux-m1.md).

## Not measured

- Real Linux hardware, other filesystems (XFS, Btrfs, NFS) and a truly cold disk; kernels other than 6.18.
- Kernels with CPU mitigations that make syscall entry much more expensive could shift the warm balance somewhat, but not past the io-wq hand-off cost seen here.
- The full engine with the prototype listing: not built, since the bare listing failed the gate.

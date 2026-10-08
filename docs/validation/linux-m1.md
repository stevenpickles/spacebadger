# Milestone 1 — Linux correctness probes

Date: 2026-10-08. Tool: `sb-probe` (this branch). Fixtures: `scripts/probe-fixtures.sh`.

## Environment

| Item | Value |
|---|---|
| Host | Docker Desktop on Windows 11 (WSL2 kernel 6.18.33.2-microsoft-standard-WSL2) |
| Container | `rust:1.97.1-bookworm`, `--privileged` (needed to create mounts) |
| Fixture filesystem | Container root (overlayfs over ext4), plus a bind mount and a tmpfs mounted inside it |

Reproduce:

```sh
docker run --rm --privileged -v "$PWD:/src:ro" -e CARGO_TARGET_DIR=/target rust:1.97.1-bookworm bash -c '
  cd /src && cargo build --locked --release -p sb-probe &&
  bash scripts/probe-fixtures.sh /fx && /target/release/sb-probe walk /fx'
```

## Fixture results

`allocated` is `st_blocks × 512` from a non-following `lstat`. Mount data comes from `statx(AT_SYMLINK_NOFOLLOW, STATX_MNT_ID)`.

| Case | Kind | Size | Allocated | Device | Mount ID | Mount root |
|---|---|---:|---:|---:|---:|---|
| Empty | file | 0 | 0 | 85 | 434 | no |
| 100-byte file | file | 100 | 4,096 | 85 | 434 | no |
| Ordinary 1 MiB + 1 | file | 1,048,577 | 1,052,672 | 85 | 434 | no |
| Sparse, 1 GiB length, 64 KiB data | file | 1,073,741,824 | 65,536 | 85 | 434 | no |
| Hard link (2 names) | file | 262,144 | 262,144 | 85 | 434 | no; `nlink` 2, shared inode |
| File symlink, dir symlink, `a ↔ b` cycle | symlink | 6–12 | 0 | 85 | 434 | no |
| FIFO | fifo | 0 | 0 | 85 | 434 | no |
| `bind/`: bind mount of `target/` | directory | 4,096 | 4,096 | **85** | **318** | **yes** |
| `tmpfs/` | directory | 60 | 0 | **87** | 319 | yes |

Walk of `/fx`:
- 7 regular files were counted: the 6 above plus `target/inside.bin`. The two hard-link names show `multiply_linked_files: 2`.
- 5 symlinks were not followed, including `loop/back → ..` and the two-link cycle.
- The FIFO was not counted as a file.
- Skipped directories:
  - `bind/` as `other_mount_same_device`
  - `tmpfs/` as `other_device`
- No errors.

## Decisions for the Linux scanner (milestone 2)

1. **Allocated = `st_blocks × 512`, logical = `st_size`,** both from `statx`/`fstatat` with `AT_SYMLINK_NOFOLLOW`. Sparse files are reported correctly.
2. **Mount boundaries need the mount ID.** The bind mount has the same `st_dev` as its parent and is caught only by `stx_mnt_id` or `STATX_ATTR_MOUNT_ROOT`. Use `STATX_ATTR_MOUNT_ROOT` on each directory. Fall back to `/proc/self/mountinfo` when the kernel doesn't report it (before Linux 5.8).
3. **Hard links:** when `nlink > 1`, record `(st_dev, st_ino)`; the first path seen owns the allocation.
4. **Exclusions:** symlinks are never followed. FIFOs, sockets, and devices are counted as entries but never opened and never given weight.

## Not yet validated

- Real Linux hardware and filesystems other than overlayfs/ext4 (btrfs subvolumes, XFS, NFS/SMB mounts, FUSE cloud mounts).
- Kernels before 5.8 without `STATX_MNT_ID`.
- The desktop app on Linux (WebKitGTK); only the scanner probes ran here.

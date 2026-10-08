#!/usr/bin/env bash
# Creates Linux filesystem fixtures for sb-probe (milestone 1).
#
# Builds ordinary, empty, tiny, sparse, hard-linked, symlink (file, dir, loop),
# FIFO, same-device bind mount, and separate tmpfs mount cases under ROOT.
# Mount cases need root (e.g. a --privileged container); failures are reported
# and the remaining fixtures are still made.
#
# usage: scripts/probe-fixtures.sh ROOT
set -u
root=${1:?usage: probe-fixtures.sh ROOT}
if [ -e "$root" ]; then echo "Refusing to reuse existing path: $root" >&2; exit 1; fi
mkdir -p "$root" && cd "$root" || exit 1

fixture() {
  local name=$1; shift
  if out=$("$@" 2>&1); then printf '%-55s ok\n' "$name"
  else printf '%-55s failed: %s\n' "$name" "$out"; fi
}

fixture 'empty.txt' touch empty.txt
fixture 'tiny.txt (100 bytes)' sh -c "head -c 100 /dev/zero | tr '\\0' x > tiny.txt"
fixture 'ordinary.bin (1 MiB + 1)' sh -c 'head -c 1048577 /dev/urandom > ordinary.bin'
fixture 'sparse.bin (1 GiB length, 64 KiB data)' sh -c \
  'head -c 65536 /dev/urandom > sparse.bin && truncate -s 1G sparse.bin'
fixture 'hardlink-a.bin + hardlink-b.bin' sh -c \
  'head -c 262144 /dev/urandom > hardlink-a.bin && ln hardlink-a.bin hardlink-b.bin'
fixture 'target/ (2 MiB file)' sh -c 'mkdir target && head -c 2097152 /dev/urandom > target/inside.bin'
fixture 'file-symlink.bin -> ordinary.bin' ln -s ordinary.bin file-symlink.bin
fixture 'dir-symlink -> target' ln -s target dir-symlink
fixture 'loop/back -> .. (directory symlink loop)' sh -c 'mkdir loop && ln -s .. loop/back'
fixture 'self-a <-> self-b (symlink cycle)' sh -c 'ln -s self-b self-a && ln -s self-a self-b'
fixture 'fifo' mkfifo fifo
fixture 'bind/ (bind mount of target, same device)' sh -c 'mkdir bind && mount --bind target bind'
fixture 'tmpfs/ (separate filesystem, 1 MiB file)' sh -c \
  'mkdir tmpfs && mount -t tmpfs -o size=8m tmpfs tmpfs && head -c 1048576 /dev/urandom > tmpfs/inside.bin'

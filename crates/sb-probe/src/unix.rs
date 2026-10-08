//! Linux and macOS probes: non-following `lstat`, plus `statx` mount identity
//! on Linux and BSD file flags (including `SF_DATALESS`) on macOS.

use crate::report::{Outcome, Samples, Tally};
use crate::{FileOpts, WalkOpts};
use serde::Serialize;
use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// macOS `SF_DATALESS`: contents are not present locally (cloud-only).
#[cfg(target_os = "macos")]
const SF_DATALESS: u32 = 0x4000_0000;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Lstat {
    kind: &'static str,
    size: u64,
    /// `st_blocks * 512`.
    allocated: u64,
    block_size: u64,
    device: u64,
    inode: u64,
    links: u64,
    #[cfg(target_os = "macos")]
    flags: String,
    #[cfg(target_os = "macos")]
    dataless: bool,
}

fn kind(meta: &Metadata) -> &'static str {
    let t = meta.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "directory"
    } else if t.is_file() {
        "file"
    } else if t.is_fifo() {
        "fifo"
    } else if t.is_socket() {
        "socket"
    } else if t.is_block_device() || t.is_char_device() {
        "device"
    } else {
        "other"
    }
}

fn lstat(path: &Path) -> std::io::Result<Lstat> {
    let meta = std::fs::symlink_metadata(path)?;
    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    let flags = std::os::macos::fs::MetadataExt::st_flags(&meta);
    Ok(Lstat {
        kind: kind(&meta),
        size: meta.size(),
        allocated: meta.blocks() * 512,
        block_size: meta.blksize(),
        device: meta.dev(),
        inode: meta.ino(),
        links: meta.nlink(),
        #[cfg(target_os = "macos")]
        flags: format!("{flags:#x}"),
        #[cfg(target_os = "macos")]
        dataless: flags & SF_DATALESS != 0,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct MountInfo {
    mount_id: u64,
    mount_root: bool,
}

#[cfg(target_os = "linux")]
fn mount_info(path: &Path) -> std::io::Result<MountInfo> {
    use rustix::fs::{AtFlags, CWD, StatxAttributes, StatxFlags, statx};
    let stx = statx(CWD, path, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::MNT_ID)?;
    if stx.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(std::io::Error::other("statx did not report STATX_MNT_ID"));
    }
    if !stx
        .stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
    {
        return Err(std::io::Error::other(
            "statx did not report STATX_ATTR_MOUNT_ROOT",
        ));
    }
    Ok(MountInfo {
        mount_id: stx.stx_mnt_id,
        mount_root: stx.stx_attributes.contains(StatxAttributes::MOUNT_ROOT),
    })
}

#[cfg(not(target_os = "linux"))]
fn mount_info(_path: &Path) -> std::io::Result<MountInfo> {
    Err(std::io::Error::other(
        "mount identity probe not implemented on this OS",
    ))
}

#[derive(Debug, Serialize)]
struct Snapshot {
    step: &'static str,
    lstat: Outcome<Lstat>,
    mount: Outcome<MountInfo>,
}

fn snapshot(step: &'static str, path: &Path) -> Snapshot {
    Snapshot {
        step,
        lstat: lstat(path).into(),
        mount: mount_info(path).into(),
    }
}

#[derive(Debug, Serialize)]
pub struct FileReport {
    path: PathBuf,
    snapshots: Vec<Snapshot>,
    changed_after: Vec<&'static str>,
}

pub fn probe_file(path: &Path, opts: &FileOpts) -> FileReport {
    let mut snapshots = vec![snapshot("initial", path)];
    std::thread::sleep(Duration::from_millis(opts.settle_ms));
    snapshots.push(snapshot("after_settle", path));
    let key = |s: &Snapshot| format!("{:?}{:?}", s.lstat, s.mount);
    let initial = key(&snapshots[0]);
    let changed_after = snapshots[1..]
        .iter()
        .filter(|s| key(s) != initial)
        .map(|s| s.step)
        .collect();
    FileReport {
        path: path.to_path_buf(),
        snapshots,
        changed_after,
    }
}

#[derive(Debug, Serialize)]
struct ErrorSample {
    path: String,
    error: String,
}

#[derive(Debug, Serialize)]
pub struct WalkReport {
    root: PathBuf,
    elapsed_ms: u128,
    stopped_early: bool,
    directories: u64,
    files: u64,
    files_per_second: f64,
    size_total: u64,
    allocated_total: u64,
    /// Entries by kind; bytes are allocation.
    kinds: Tally,
    /// Directories not descended into, by reason.
    skipped_directories: Tally,
    skipped_samples: Samples<String>,
    multiply_linked_files: u64,
    errors: Samples<ErrorSample>,
}

pub fn walk(root: &Path, opts: &WalkOpts) -> WalkReport {
    let started = Instant::now();
    let mut report = WalkReport {
        root: root.to_path_buf(),
        elapsed_ms: 0,
        stopped_early: false,
        directories: 1,
        files: 0,
        files_per_second: 0.0,
        size_total: 0,
        allocated_total: 0,
        kinds: Tally::default(),
        skipped_directories: Tally::default(),
        skipped_samples: Samples::new(opts.samples),
        multiply_linked_files: 0,
        errors: Samples::new(opts.samples),
    };
    let root_dev = std::fs::symlink_metadata(root).map(|m| m.dev()).ok();
    let root_mount = mount_info(root).ok().map(|m| m.mount_id);
    let mut pending = vec![root.to_path_buf()];
    'walk: while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                report.errors.add("read_dir", || ErrorSample {
                    path: dir.display().to_string(),
                    error: err.to_string(),
                });
                continue;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(err) => {
                    report.errors.add("read_dir_entry", || ErrorSample {
                        path: dir.display().to_string(),
                        error: err.to_string(),
                    });
                    continue;
                }
            };
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) => {
                    report.errors.add("lstat", || ErrorSample {
                        path: path.display().to_string(),
                        error: err.to_string(),
                    });
                    continue;
                }
            };
            let allocated = meta.blocks() * 512;
            report.kinds.add(kind(&meta), allocated);
            if meta.is_dir() {
                let reason = if Some(meta.dev()) != root_dev {
                    Some("other_device")
                } else if root_mount.is_some()
                    && mount_info(&path).ok().map(|m| m.mount_id) != root_mount
                {
                    Some("other_mount_same_device")
                } else {
                    None
                };
                match reason {
                    Some(reason) => {
                        report.skipped_directories.add(reason, 0);
                        report
                            .skipped_samples
                            .add(reason, || path.display().to_string());
                    }
                    None => {
                        report.directories += 1;
                        pending.push(path);
                    }
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if opts.max_files.is_some_and(|max| report.files >= max) {
                report.stopped_early = true;
                break 'walk;
            }
            report.files += 1;
            report.size_total += meta.size();
            report.allocated_total += allocated;
            if meta.nlink() > 1 {
                report.multiply_linked_files += 1;
            }
        }
    }
    report.elapsed_ms = started.elapsed().as_millis();
    report.files_per_second = report.files as f64 / started.elapsed().as_secs_f64().max(1e-9);
    report
}

//! Linux adapter. Decisions and evidence: `docs/validation/linux-m1.md`.
//!
//! - Opens each directory with `O_NOFOLLOW` and `statx`es entries relative to
//!   it with `AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT`, so links are never
//!   followed and automounts are never triggered.
//! - Allocated = `stx_blocks × 512`; logical = `stx_size`.
//! - A directory is a mount boundary if its device differs from the root's,
//!   or `STATX_ATTR_MOUNT_ROOT` is set, or its mount ID differs. Bind mounts
//!   share the device, so the device check alone is insufficient. Kernels that
//!   report neither attribute fall back to `/proc/self/mountinfo`.

use crate::adapter::{
    Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError,
};
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, Statx, StatxAttributes, StatxFlags, openat, statx,
};
use std::collections::HashSet;
use std::ffi::{CStr, OsStr, OsString};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const STAT_FLAGS: AtFlags = AtFlags::SYMLINK_NOFOLLOW.union(AtFlags::NO_AUTOMOUNT);
const STAT_MASK: StatxFlags = StatxFlags::BASIC_STATS.union(StatxFlags::MNT_ID);

struct Boundary {
    dev: u64,
    mount_id: Option<u64>,
    /// Mount points from mountinfo when statx reports neither mount
    /// attribute nor mount ID.
    mount_points: Option<HashSet<PathBuf>>,
}

/// Linux filesystem adapter. Create one per scan.
#[derive(Default)]
pub struct LinuxFs {
    boundary: OnceLock<Boundary>,
}

impl LinuxFs {
    pub fn new() -> Self {
        Self::default()
    }

    fn boundary(&self) -> &Boundary {
        self.boundary
            .get()
            .expect("open_root must run before read_dir")
    }
}

fn dev(stx: &Statx) -> u64 {
    (u64::from(stx.stx_dev_major) << 32) | u64::from(stx.stx_dev_minor)
}

fn mount_id(stx: &Statx) -> Option<u64> {
    (stx.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stx.stx_mnt_id)
}

fn kind_of(stx: &Statx) -> FileType {
    FileType::from_raw_mode(rustix::fs::RawMode::from(stx.stx_mode))
}

fn error(err: rustix::io::Errno) -> ScanError {
    std::io::Error::from(err).into()
}

/// Mount points listed in `/proc/self/mountinfo` (field 5, octal-escaped).
fn read_mount_points() -> Option<HashSet<PathBuf>> {
    let text = std::fs::read("/proc/self/mountinfo").ok()?;
    let mut points = HashSet::new();
    for line in text.split(|&b| b == b'\n') {
        if let Some(field) = line.split(|&b| b == b' ').nth(4) {
            points.insert(PathBuf::from(OsString::from_vec(unescape_octal(field))));
        }
    }
    Some(points)
}

fn unescape_octal(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && i + 3 < field.len()
            && field[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = field[i + 1..i + 4]
                .iter()
                .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            out.push(value as u8);
            i += 4;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    out
}

impl FsAdapter for LinuxFs {
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError> {
        let stx = statx(CWD, root, STAT_FLAGS, STAT_MASK).map_err(error)?;
        let kind = match kind_of(&stx) {
            FileType::Symlink => RootKind::Link,
            FileType::Directory => RootKind::Directory,
            _ => RootKind::NotDirectory,
        };
        let has_mount_attr = stx
            .stx_attributes_mask
            .contains(StatxAttributes::MOUNT_ROOT);
        let mount_id = mount_id(&stx);
        let _ = self.boundary.set(Boundary {
            dev: dev(&stx),
            mount_id,
            mount_points: (!has_mount_attr && mount_id.is_none())
                .then(read_mount_points)
                .flatten(),
        });
        Ok(kind)
    }

    fn read_dir(&self, dir: &Path) -> Result<Vec<Entry>, ScanError> {
        let boundary = self.boundary();
        let fd = openat(
            CWD,
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(error)?;
        let mut entries = Vec::new();
        for item in Dir::read_from(fd.as_fd()).map_err(error)? {
            let item = item.map_err(error)?;
            let name: &CStr = item.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let os_name = OsStr::from_bytes(name.to_bytes()).to_owned();
            let stx = match statx(fd.as_fd(), name, STAT_FLAGS, STAT_MASK) {
                Ok(stx) => stx,
                Err(err) => {
                    let kind = error(err).kind;
                    entries.push(Entry {
                        name: os_name,
                        kind: EntryKind::Failed(kind),
                        logical: 0,
                        allocated: None,
                        identity: Identity::Unavailable,
                        cloud: false,
                    });
                    continue;
                }
            };
            let kind = match kind_of(&stx) {
                FileType::RegularFile => EntryKind::File,
                FileType::Symlink => EntryKind::Symlink,
                FileType::Directory => {
                    let crosses = dev(&stx) != boundary.dev
                        || (stx
                            .stx_attributes_mask
                            .contains(StatxAttributes::MOUNT_ROOT)
                            && stx.stx_attributes.contains(StatxAttributes::MOUNT_ROOT))
                        || matches!((mount_id(&stx), boundary.mount_id), (Some(a), Some(b)) if a != b)
                        || boundary
                            .mount_points
                            .as_ref()
                            .is_some_and(|points| points.contains(&dir.join(&os_name)));
                    if crosses {
                        EntryKind::MountPoint
                    } else {
                        EntryKind::Directory
                    }
                }
                _ => EntryKind::Special,
            };
            entries.push(Entry {
                name: os_name,
                kind,
                logical: stx.stx_size,
                allocated: Some(stx.stx_blocks.saturating_mul(512)),
                identity: if stx.stx_nlink > 1 {
                    Identity::MaybeShared {
                        key: (u128::from(dev(&stx)) << 64) | u128::from(stx.stx_ino),
                    }
                } else {
                    Identity::Unique
                },
                cloud: false,
            });
        }
        Ok(entries)
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        let stx = statx(CWD, file, STAT_FLAGS, StatxFlags::BASIC_STATS).map_err(error)?;
        if kind_of(&stx) != FileType::RegularFile {
            return Err(ScanError::new(
                ErrorKind::Vanished,
                "no longer a regular file",
            ));
        }
        Ok(Measured {
            logical: stx.stx_size,
            allocated: Some(stx.stx_blocks.saturating_mul(512)),
        })
    }
}

/// Whether `path` is on a network or FUSE filesystem, by `statfs` magic.
pub fn is_remote(path: &Path) -> bool {
    const REMOTE: [u32; 10] = [
        0x6969,      // NFS
        0x517B,      // SMB
        0xFF53_4D42, // CIFS
        0xFE53_4D42, // SMB2
        0x0102_1997, // 9P
        0x00C3_6400, // Ceph
        0x5346_414F, // AFS
        0x7375_7245, // Coda
        0x6573_5546, // FUSE (sshfs and others)
        0x0BD0_0BD0, // Lustre
    ];
    rustix::fs::statfs(path).is_ok_and(|s| REMOTE.contains(&(s.f_type as u32)))
}

/// Capacity and free space from `statvfs`. The path is the volume's top
/// folder when statx marks it as a mount root, or its mount ID or device
/// differs from its parent's.
pub fn volume_info(path: &Path) -> Option<super::VolumeInfo> {
    let vfs = rustix::fs::statvfs(path).ok()?;
    let stx = statx(CWD, path, STAT_FLAGS, STAT_MASK).ok()?;
    let marked = stx
        .stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
        && stx.stx_attributes.contains(StatxAttributes::MOUNT_ROOT);
    let parent_differs = || {
        let parent = path.join("..");
        statx(CWD, &parent, STAT_FLAGS, STAT_MASK).is_ok_and(|p| {
            dev(&p) != dev(&stx) || (mount_id(&p).is_some() && mount_id(&p) != mount_id(&stx))
        })
    };
    let is_root = path == Path::new("/") || marked || parent_differs();
    let filesystem = rustix::fs::statfs(path)
        .ok()
        .and_then(|s| filesystem_name(s.f_type as u32))
        .map(str::to_owned);
    Some(super::VolumeInfo {
        is_root,
        capacity: vfs.f_blocks.saturating_mul(vfs.f_frsize),
        free: vfs.f_bfree.saturating_mul(vfs.f_frsize),
        filesystem,
        metadata: None,
    })
}

/// Names for common `statfs` magic numbers.
fn filesystem_name(magic: u32) -> Option<&'static str> {
    Some(match magic {
        0xEF53 => "ext4",
        0x5846_5342 => "XFS",
        0x9123_683E => "Btrfs",
        0x2FC1_2FC1 => "ZFS",
        0xF2F5_2010 => "F2FS",
        0x0102_1994 => "tmpfs",
        0x4D44 => "FAT",
        0x2011_BAB0 => "exFAT",
        0x7366_746E => "NTFS",
        0x794C_7630 => "overlayfs",
        _ => return None,
    })
}

/// Whether the process runs as root, which bypasses permission checks.
pub fn is_privileged() -> bool {
    rustix::process::geteuid().is_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_mountinfo_paths() {
        assert_eq!(unescape_octal(br"/mnt/with\040space"), b"/mnt/with space");
        assert_eq!(unescape_octal(br"/plain"), b"/plain");
        assert_eq!(unescape_octal(br"/tab\011x\134y"), b"/tab\tx\\y");
    }
}

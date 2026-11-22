//! Portable Unix adapter (macOS and other non-Linux Unix).
//!
//! **Unvalidated on a real Mac.** It is built and tested on CI's macOS
//! runners only. Known gaps, tracked in `docs/PLAN.md`:
//! - Mount boundaries use the device ID only. On CI's macOS runner, `lstat`
//!   reports the same device for `/`, the firmlinked data-volume folders
//!   (`/Users`, `/Applications`, ...) and `/System/Volumes/Data`, so a scan
//!   of `/` descends into the firmlinks *and* into `/System/Volumes/Data`,
//!   and may count the data volume twice. Only `/dev` and some
//!   `/System/Volumes/*` volumes are skipped as mount points. `/.nofollow`,
//!   `/.resolve` and `/.vol` list as empty folders. Both listings below keep
//!   this behaviour: the bulk listing hands firmlinks to `lstat`.
//!
//! On macOS directories are listed with `getattrlistbulk` (see `bulk`), which
//! returns the metadata of a 64 KiB buffer of entries in one call, instead of
//! `readdir` plus an `lstat` per entry. The results are the same (a test
//! compares them). Entries the bulk call can't describe exactly as `lstat`
//! does (firmlinks, mount points, autofs triggers, missing attributes) are
//! `lstat`ed, and a filesystem that refuses the call (`ENOTSUP`, `EINVAL`) is
//! listed with `readdir`. Only APFS and HFS+ scans use the bulk call at all:
//! on exFAT it gives other file IDs and folder times than `lstat`, and other
//! filesystems are untested, so they are listed with `readdir`.
//! [`UnixFs::fallbacks`] counts all of these. Setting `SB_MACOS_LISTING=readdir` forces the
//! `readdir` listing, for comparison. Neither listing opens a file.
//!
//! On macOS the process I/O policy is set so dataless (cloud-only) files are
//! never materialized by this process, and `SF_DATALESS` marks cloud files.

use crate::adapter::{
    Batcher, Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError, Sink,
    batched, read_capped, refused,
};
use crate::duplicates::{Fingerprint, Skip};
use sb_core::tree::{birth_secs, unix_object_id};
use std::ffi::OsString;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
mod bulk;

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_int;

    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
    const IOPOL_SCOPE_PROCESS: c_int = 0;
    const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;
    pub const SF_DATALESS: u32 = 0x4000_0000;

    unsafe extern "C" {
        fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
    }

    /// Prevents this process from materializing dataless files.
    pub fn disable_materialization() -> std::io::Result<()> {
        let rc = unsafe {
            setiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_PROCESS,
                IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    pub fn is_dataless(meta: &std::fs::Metadata) -> bool {
        #[allow(deprecated)]
        let flags = std::os::macos::fs::MetadataExt::st_flags(meta);
        flags & SF_DATALESS != 0
    }
}

/// How the adapter lists a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    /// `getattrlistbulk` on macOS, falling back to `readdir` where the
    /// filesystem refuses it; `readdir` elsewhere.
    Bulk,
    /// `readdir` and an `lstat` per entry.
    Readdir,
    /// `getattrlistbulk` even on filesystems it doesn't match `readdir` on;
    /// for tests that record how. Never use it to scan.
    BulkOnAnyVolume,
}

impl Listing {
    /// `Readdir` when `SB_MACOS_LISTING` is `readdir`, otherwise `Bulk`.
    pub fn from_env() -> Self {
        match std::env::var_os("SB_MACOS_LISTING") {
            Some(v) if v == "readdir" => Self::Readdir,
            _ => Self::Bulk,
        }
    }
}

/// How often an adapter's bulk listing (macOS) fell back to the slower way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fallbacks {
    /// The scan root's filesystem isn't one the bulk listing is used on
    /// (see `bulk::trusted`), so every folder is listed with `readdir`.
    pub volume: bool,
    /// Folders listed with `readdir`: on such a volume, or the filesystem
    /// refused `getattrlistbulk`, or gave a record without a name first.
    pub folders: u64,
    /// Entries `lstat`ed: firmlinks, mount points, autofs triggers, errors
    /// and records missing an attribute.
    pub entries: u64,
}

/// Portable Unix adapter. Create one per scan.
pub struct UnixFs {
    root_dev: OnceLock<u64>,
    listing: Listing,
    /// Whether the scan root's filesystem is listed in bulk (macOS).
    bulk_volume: OnceLock<bool>,
    readdir_folders: AtomicU64,
    lstat_entries: AtomicU64,
}

impl Default for UnixFs {
    fn default() -> Self {
        Self::with_listing(Listing::from_env())
    }
}

impl UnixFs {
    /// An adapter listing as `SB_MACOS_LISTING` says ([`Listing::from_env`]).
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_listing(listing: Listing) -> Self {
        Self {
            root_dev: OnceLock::new(),
            listing,
            bulk_volume: OnceLock::new(),
            readdir_folders: AtomicU64::new(0),
            lstat_entries: AtomicU64::new(0),
        }
    }

    /// How often this adapter's bulk listing fell back so far.
    pub fn fallbacks(&self) -> Fallbacks {
        Fallbacks {
            volume: self.listing == Listing::Bulk && self.bulk_volume.get() == Some(&false),
            folders: self.readdir_folders.load(Ordering::Relaxed),
            entries: self.lstat_entries.load(Ordering::Relaxed),
        }
    }
}

fn is_cloud(_meta: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    return macos::is_dataless(_meta);
    #[cfg(not(target_os = "macos"))]
    false
}

impl FsAdapter for UnixFs {
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError> {
        #[cfg(target_os = "macos")]
        macos::disable_materialization().map_err(|err| {
            ScanError::new(
                ErrorKind::Unavailable,
                format!("can't disable cloud file materialization: {err}"),
            )
        })?;
        let meta = std::fs::symlink_metadata(root)?;
        let _ = self.root_dev.set(meta.dev());
        #[cfg(target_os = "macos")]
        if meta.is_dir() {
            let _ = self.bulk_volume.set(match self.listing {
                Listing::Bulk => bulk::trusted(root),
                Listing::BulkOnAnyVolume => true,
                Listing::Readdir => false,
            });
        }
        Ok(if meta.file_type().is_symlink() {
            RootKind::Link
        } else if meta.is_dir() {
            RootKind::Directory
        } else {
            RootKind::NotDirectory
        })
    }

    fn read_dir(&self, dir: &Path, sink: &mut Sink<'_>) -> Result<(), ScanError> {
        let root_dev = *self
            .root_dev
            .get()
            .expect("open_root must run before read_dir");
        batched(sink, |out| {
            #[cfg(target_os = "macos")]
            if self.listing != Listing::Readdir && self.bulk_volume.get() != Some(&true) {
                self.readdir_folders.fetch_add(1, Ordering::Relaxed);
            } else if self.listing != Listing::Readdir {
                match bulk::list(dir, root_dev, &self.lstat_entries, out)? {
                    bulk::Listed::Done => return Ok(()),
                    bulk::Listed::Unsupported => {
                        self.readdir_folders.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = self.listing;
            self.list(dir, root_dev, out)
        })
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        let meta = std::fs::symlink_metadata(file)?;
        if !meta.is_file() {
            return Err(ScanError::new(
                ErrorKind::Vanished,
                "no longer a regular file",
            ));
        }
        Ok(Measured {
            logical: meta.size(),
            allocated: Some(meta.blocks().saturating_mul(512)),
            id: Some((u128::from(meta.dev()) << 64) | u128::from(meta.ino())),
        })
    }

    /// Checked before opening and again on the open file, which must be the
    /// same: a regular file, not dataless (cloud-only). The open refuses a
    /// link and never blocks.
    fn read_small(&self, path: &Path, cap: usize) -> Result<Vec<u8>, ScanError> {
        use std::os::unix::fs::OpenOptionsExt;
        let check = |meta: &Metadata| {
            if meta.file_type().is_symlink() {
                Err(ScanError::new(ErrorKind::Unavailable, refused::LINK))
            } else if is_cloud(meta) {
                Err(ScanError::new(ErrorKind::Unavailable, refused::CLOUD))
            } else if !meta.is_file() {
                Err(ScanError::new(ErrorKind::Unavailable, refused::NOT_FILE))
            } else {
                Ok(())
            }
        };
        let before = std::fs::symlink_metadata(path)?;
        check(&before)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(open_flags::NOFOLLOW_NONBLOCK)
            .open(path)
            .map_err(|e| match e.raw_os_error() {
                Some(code) if code == open_flags::ELOOP => {
                    ScanError::new(ErrorKind::Unavailable, refused::LINK)
                }
                _ => ScanError::from(e),
            })?;
        let meta = file.metadata()?;
        check(&meta)?;
        if (meta.dev(), meta.ino()) != (before.dev(), before.ino()) {
            return Err(ScanError::new(ErrorKind::Vanished, "replaced while read"));
        }
        read_capped(file, cap)
    }

    fn stat_kind(&self, path: &Path) -> Result<EntryKind, ScanError> {
        let file_type = std::fs::symlink_metadata(path)?.file_type();
        Ok(if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Special
        })
    }
}

impl UnixFs {
    /// Reads `dir`, `lstat`ing each entry and passing it on at once; stops
    /// early when the batcher says so.
    fn list(&self, dir: &Path, root_dev: u64, out: &mut Batcher<'_, '_>) -> Result<(), ScanError> {
        for item in std::fs::read_dir(dir)? {
            let item = item?;
            if out
                .push(lstat_entry(&item.path(), item.file_name(), root_dev))
                .is_break()
            {
                return Ok(());
            }
        }
        Ok(())
    }
}

/// The entry for `path` (named `name` in its folder) from `lstat`: a
/// directory on another device than `root_dev` is a mount point.
fn lstat_entry(path: &Path, name: OsString, root_dev: u64) -> Entry {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            return Entry {
                name,
                kind: EntryKind::Failed(ScanError::from(err).kind),
                logical: 0,
                allocated: None,
                identity: Identity::Unavailable,
                id: 0,
                cloud: false,
                modified: None,
                stamp: None,
            };
        }
    };
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        if meta.dev() == root_dev {
            EntryKind::Directory
        } else {
            EntryKind::MountPoint
        }
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Special
    };
    Entry {
        name,
        kind,
        logical: meta.size(),
        allocated: Some(meta.blocks().saturating_mul(512)),
        identity: if meta.nlink() > 1 {
            Identity::MaybeShared {
                key: (u128::from(meta.dev()) << 64) | u128::from(meta.ino()),
            }
        } else {
            Identity::Unique
        },
        // The device is the scan's, checked at delete time.
        id: unix_object_id(meta.ino(), birth_secs(&meta)),
        cloud: is_cloud(&meta),
        modified: meta.modified().ok(),
        stamp: None,
    }
}

/// `open` flags for finding duplicates: `O_NOFOLLOW` refuses a link and
/// `O_NONBLOCK` keeps a FIFO from blocking, with the values macOS and the
/// BSDs share; other systems rely on the checks in [`open_contents`] alone.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
mod open_flags {
    pub const NOFOLLOW_NONBLOCK: i32 = 0x0100 | 0x0004;
    pub const ELOOP: i32 = 62;
}
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
mod open_flags {
    pub const NOFOLLOW_NONBLOCK: i32 = 0;
    pub const ELOOP: i32 = -1;
}

/// Opens a file to read its contents for finding duplicates. The path must
/// be a plain file of the scanned size before opening, the open refuses a
/// link and never blocks, and the opened file must be that same file, so a
/// link is never read through. Dataless (cloud-only) files are refused, and
/// this process never materializes them anyway (see `open_root`).
pub fn open_contents(path: &Path, len: u64) -> Result<(std::fs::File, Fingerprint), Skip> {
    use std::os::unix::fs::OpenOptionsExt;
    let before = examined(path, len)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(open_flags::NOFOLLOW_NONBLOCK)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(code) if code == open_flags::ELOOP => Skip::Changed,
            _ => Skip::of(&e),
        })?;
    let meta = file.metadata().map_err(|e| Skip::of(&e))?;
    if is_cloud(&meta) {
        return Err(Skip::Cloud);
    }
    if !meta.is_file()
        || (meta.dev(), meta.ino()) != (before.dev(), before.ino())
        || meta.size() != len
    {
        return Err(Skip::Changed);
    }
    Ok((file, super::fingerprint(&meta)))
}

/// The fingerprint of the file at `path`, without opening it, refusing
/// what [`open_contents`] refuses before it opens.
pub fn examine_contents(path: &Path, len: u64) -> Result<Fingerprint, Skip> {
    examined(path, len).map(|meta| super::fingerprint(&meta))
}

fn examined(path: &Path, len: u64) -> Result<Metadata, Skip> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| Skip::of(&e))?;
    if is_cloud(&meta) {
        return Err(Skip::Cloud);
    }
    if !meta.is_file() || meta.size() != len {
        return Err(Skip::Changed);
    }
    Ok(meta)
}

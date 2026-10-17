//! Directory listing with `getattrlistbulk` (macOS): one call fills a 64 KiB
//! buffer with the name and metadata of as many entries as fit, where
//! `readdir` + `lstat` costs two calls per entry. Like `lstat`, it reads
//! metadata only and never opens or materializes a file, and it doesn't
//! follow links.
//!
//! Each entry must come out exactly as [`lstat_entry`] makes it. The few the
//! bulk call can't describe the same way are handed to `lstat_entry`:
//! - a folder that is a mount point or autofs trigger, or a firmlink: the
//!   bulk call describes the folder underneath (on the parent's volume),
//!   while `lstat` crosses to what is mounted or linked there, whose device
//!   decides whether it counts as another filesystem;
//! - an entry the call returned an error for, without exactly the
//!   attributes asked for (a filesystem that doesn't keep one), or with a
//!   field that doesn't parse.
//!
//! A record without a name makes the folder fall back to `readdir` when
//! nothing was handed over yet, and is otherwise passed on as an unreadable
//! entry. A listed folder on another device than the scan's (a mount point
//! the parent's listing missed) isn't listed: it fails as unavailable, so
//! the scan never crosses filesystems.
//!
//! Folders are described as `lstat` would except their link count: the bulk
//! call has no `st_nlink` for folders, so their identity is `Unique`. The
//! engine uses only a folder's name, kind and ID.

use super::{lstat_entry, macos::SF_DATALESS};
use crate::adapter::{Batcher, Entry, EntryKind, ErrorKind, Identity, ScanError};
use sb_core::tree::{birth_secs_of, unix_object_id};
use std::cell::RefCell;
use std::ffi::{OsString, c_int, c_void};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What [`list`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listed {
    /// Every entry was handed over, or the batcher asked to stop.
    Done,
    /// The filesystem refused the call, or gave a record without a name,
    /// before any entry was handed over; list the folder another way.
    Unsupported,
}

/// Bytes of entries per call.
const BUFFER: usize = 64 * 1024;

/// `struct attrlist` from `<sys/attr.h>`.
#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

const ATTR_BIT_MAP_COUNT: u16 = 5;

// Common attributes, packed in this order after the returned set (the error
// first).
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_CRTIME: u32 = 0x0000_0200;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
/// Common attributes an entry needs to be described without `lstat`.
const NEEDED_CMN: u32 = ATTR_CMN_NAME
    | ATTR_CMN_DEVID
    | ATTR_CMN_OBJTYPE
    | ATTR_CMN_CRTIME
    | ATTR_CMN_MODTIME
    | ATTR_CMN_FLAGS
    | ATTR_CMN_FILEID;

// Folder attributes (folders only).
const ATTR_DIR_MOUNTSTATUS: u32 = 0x0000_0004;
const ATTR_DIR_ALLOCSIZE: u32 = 0x0000_0008;
const ATTR_DIR_DATALENGTH: u32 = 0x0000_0020;
const NEEDED_DIR: u32 = ATTR_DIR_MOUNTSTATUS | ATTR_DIR_ALLOCSIZE | ATTR_DIR_DATALENGTH;
const DIR_MNTSTATUS_MNTPOINT: u32 = 0x1;
const DIR_MNTSTATUS_TRIGGER: u32 = 0x2;

// File attributes (everything but folders, links and special files
// included).
const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;
const NEEDED_FILE: u32 = ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH;

/// `st_flags` bit of a firmlink (`<sys/stat.h>`).
const SF_FIRMLINK: u32 = 0x0080_0000;

// `enum vtype` from `<sys/vnode.h>`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

const O_NOFOLLOW: i32 = 0x0000_0100;
const O_DIRECTORY: i32 = 0x0010_0000;
const EINTR: i32 = 4;
const EINVAL: i32 = 22;
const ENOTSUP: i32 = 45;
const EOPNOTSUPP: i32 = 102;

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: c_int,
        attr_list: *mut AttrList,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u64,
    ) -> c_int;
}

thread_local! {
    /// Each listing thread's buffer, `u64`s so every field is aligned at
    /// least as the kernel packs it.
    static BUF: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Lists `dir` into `out`, as [`super::UnixFs::list`] would, counting the
/// entries handed to `lstat` in `by_lstat`.
pub fn list(
    dir: &Path,
    root_dev: u64,
    by_lstat: &AtomicU64,
    out: &mut Batcher<'_, '_>,
) -> Result<Listed, ScanError> {
    let listing = Listing {
        dir,
        root_dev,
        by_lstat,
    };
    BUF.with(|buf| match buf.try_borrow_mut() {
        Ok(mut buf) => listing.run(&mut buf, out),
        Err(_) => listing.run(&mut Vec::new(), out),
    })
}

struct Listing<'a> {
    dir: &'a Path,
    root_dev: u64,
    by_lstat: &'a AtomicU64,
}

impl Listing<'_> {
    fn run(&self, buf: &mut Vec<u64>, out: &mut Batcher<'_, '_>) -> Result<Listed, ScanError> {
        buf.resize(BUFFER / 8, 0);
        let folder = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(self.dir)?;
        if folder.metadata()?.dev() != self.root_dev {
            return Err(ScanError::new(
                ErrorKind::Unavailable,
                "another filesystem is mounted here",
            ));
        }
        let mut attrs = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR | NEEDED_CMN,
            volattr: 0,
            dirattr: NEEDED_DIR,
            fileattr: NEEDED_FILE,
            forkattr: 0,
        };
        let mut any = false;
        loop {
            // SAFETY: the buffer is `BUFFER` writable bytes and `attrs` a
            // valid attribute list, both alive for the call.
            let count = unsafe {
                getattrlistbulk(
                    folder.as_raw_fd(),
                    &mut attrs,
                    buf.as_mut_ptr().cast(),
                    BUFFER,
                    0,
                )
            };
            if count < 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(EINTR) => continue,
                    Some(ENOTSUP | EOPNOTSUPP | EINVAL) if !any => return Ok(Listed::Unsupported),
                    _ => return Err(err.into()),
                }
            }
            if count == 0 {
                return Ok(Listed::Done);
            }
            // SAFETY: `BUFFER` initialized bytes (the `u64`s were zeroed).
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), BUFFER) };
            let mut at = 0;
            for _ in 0..count {
                let len = bytes
                    .get(at..at + 4)
                    .map(|b| u32::from_ne_bytes(b.try_into().unwrap()) as usize)
                    .filter(|&len| len >= 4 && at + len <= BUFFER)
                    .ok_or_else(malformed)?;
                let entry = match self.describe(&bytes[at..at + len])? {
                    Some(entry) => entry,
                    None if !any => return Ok(Listed::Unsupported),
                    None => unreadable(),
                };
                at += len;
                any = true;
                if out.push(entry).is_break() {
                    return Ok(Listed::Done);
                }
            }
        }
    }

    /// One entry from its packed record, or from `lstat` where the record
    /// can't stand for it (see the module documentation); `None` for a
    /// record without a name.
    fn describe(&self, record: &[u8]) -> Result<Option<Entry>, ScanError> {
        let mut f = Fields {
            bytes: record,
            at: 4,
        };
        let (Some(common), Some(_), Some(dirattr), Some(fileattr), Some(_)) =
            (f.u32(), f.u32(), f.u32(), f.u32(), f.u32())
        else {
            return Err(malformed());
        };
        let error = if common & ATTR_CMN_ERROR != 0 {
            f.u32().ok_or_else(malformed)?
        } else {
            0
        };
        let name = if common & ATTR_CMN_NAME != 0 {
            name(&mut f)
        } else {
            None
        };
        let Some(name) = name else {
            return Ok(None);
        };
        let described = if error == 0 {
            fields(&mut f, common, dirattr, fileattr, self.root_dev)
        } else {
            None
        };
        Ok(Some(match described {
            Some(entry) => Entry { name, ..entry },
            None => {
                self.by_lstat.fetch_add(1, Ordering::Relaxed);
                lstat_entry(&self.dir.join(&name), name, self.root_dev)
            }
        }))
    }
}

fn malformed() -> ScanError {
    ScanError::new(
        ErrorKind::Unavailable,
        "getattrlistbulk returned a malformed entry",
    )
}

/// An entry whose name the listing didn't give.
fn unreadable() -> Entry {
    Entry {
        name: OsString::new(),
        kind: EntryKind::Failed(ErrorKind::Unavailable),
        logical: 0,
        allocated: None,
        identity: Identity::Unavailable,
        id: 0,
        cloud: false,
        modified: None,
    }
}

/// Reads one packed entry's fields in order.
struct Fields<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Fields<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let field = self.bytes.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(field)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_ne_bytes)
    }

    fn i32(&mut self) -> Option<i32> {
        self.take().map(i32::from_ne_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_ne_bytes)
    }

    fn i64(&mut self) -> Option<i64> {
        self.take().map(i64::from_ne_bytes)
    }

    /// A non-negative `off_t`.
    fn size(&mut self) -> Option<u64> {
        u64::try_from(self.i64()?).ok()
    }

    /// A `struct timespec`, as `std` turns `stat` times into `SystemTime`.
    fn time(&mut self) -> Option<SystemTime> {
        let (secs, nanos) = (self.i64()?, self.i64()?);
        let nanos = u32::try_from(nanos).ok().filter(|&n| n < 1_000_000_000)?;
        if secs >= 0 {
            UNIX_EPOCH.checked_add(Duration::new(secs.unsigned_abs(), nanos))
        } else {
            UNIX_EPOCH
                .checked_sub(Duration::from_secs(secs.unsigned_abs()))?
                .checked_add(Duration::from_nanos(nanos.into()))
        }
    }
}

/// The entry (without its name) the fields after the name describe, or
/// `None` where `lstat` must say: an attribute missing or one not asked
/// for, a field that doesn't parse, or a folder `lstat` may see otherwise.
fn fields(
    f: &mut Fields<'_>,
    common: u32,
    dirattr: u32,
    fileattr: u32,
    root_dev: u64,
) -> Option<Entry> {
    if common & !(ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_ERROR) != NEEDED_CMN {
        return None;
    }
    // `dev_t` is an `i32`, widened with its sign as `MetadataExt::dev` does.
    let dev = f.i32()? as u64;
    let kind = f.u32()?;
    let born = f.time()?;
    let modified = f.time()?;
    let flags = f.u32()?;
    let ino = f.u64()?;
    let (kind, logical, allocated, links) = if kind == VDIR {
        if dirattr != NEEDED_DIR || fileattr != 0 {
            return None;
        }
        let mount = f.u32()?;
        let allocated = f.size()?;
        let logical = f.size()?;
        // Firmlinks, mount points and autofs triggers go to `lstat`, which
        // crosses into what is there. Triggers too, on purpose: that's what
        // the `readdir` listing does with them, so both listings agree.
        if flags & SF_FIRMLINK != 0 || mount & (DIR_MNTSTATUS_MNTPOINT | DIR_MNTSTATUS_TRIGGER) != 0
        {
            return None;
        }
        let kind = if dev == root_dev {
            EntryKind::Directory
        } else {
            EntryKind::MountPoint
        };
        (kind, logical, allocated, None)
    } else {
        if fileattr != NEEDED_FILE || dirattr != 0 {
            return None;
        }
        let links = f.u32()?;
        let allocated = f.size()?;
        let logical = f.size()?;
        let kind = match kind {
            VREG => EntryKind::File,
            VLNK => EntryKind::Symlink,
            _ => EntryKind::Special,
        };
        (kind, logical, allocated, Some(links))
    };
    Some(Entry {
        name: OsString::new(),
        kind,
        logical,
        // As `st_blocks`, which counts the allocation in 512-byte units
        // rounded up.
        allocated: Some(allocated.div_ceil(512).saturating_mul(512)),
        identity: match links {
            Some(links) if links > 1 => Identity::MaybeShared {
                key: (u128::from(dev) << 64) | u128::from(ino),
            },
            _ => Identity::Unique,
        },
        id: unix_object_id(ino, Some(birth_secs_of(born))),
        cloud: flags & SF_DATALESS != 0,
        modified: Some(modified),
    })
}

/// The name an `attrreference_t` points to: an offset from the reference
/// itself and a length counting the terminating NUL.
fn name(f: &mut Fields<'_>) -> Option<OsString> {
    let at = f.at;
    let offset = usize::try_from(f.i32()?).ok()?;
    let len = usize::try_from(f.u32()?).ok()?;
    let bytes = f.bytes.get(at + offset..at + offset + len)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(len);
    let name = &bytes[..end];
    (!name.is_empty()).then(|| OsString::from_vec(name.to_vec()))
}

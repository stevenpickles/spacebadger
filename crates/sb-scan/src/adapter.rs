//! The filesystem interface the scan engine drives. Native adapters implement
//! it per platform; [`crate::fake::FakeFs`] implements it in memory for tests.

use sb_core::git::paths::Style;
use std::ffi::OsString;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::Path;
use std::time::SystemTime;

/// Most entries handed over at once by [`FsAdapter::read_dir`]. Bounds what
/// a listing holds in memory and how long the engine keeps the tree locked
/// per batch, however many entries a directory has.
pub const BATCH_ENTRIES: usize = 4096;

/// What a directory entry is, as far as scanning is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file (including cloud placeholders, sparse and compressed
    /// files). Measured and counted.
    File,
    /// A directory on the scan root's filesystem. Descended into.
    Directory,
    /// A symbolic link (file or directory). Never followed.
    Symlink,
    /// A junction, volume mount point, or directory on another mounted
    /// filesystem. Never followed.
    MountPoint,
    /// A FIFO, socket, device, or other non-regular object. Never opened.
    Special,
    /// A redirection the scanner doesn't understand. Never followed.
    UnknownReparse,
    /// Listed, but its metadata couldn't be read (e.g. it vanished).
    Failed(ErrorKind),
}

/// Identity used to count hard-linked files once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// The entry is known to have exactly one name.
    Unique,
    /// The entry may share its underlying file with other names; `key` is
    /// stable for the underlying file on the scan root's filesystem.
    MaybeShared { key: u128 },
    /// The platform gave no usable identity; counting is per entry.
    Unavailable,
}

/// One directory entry with metadata from the listing itself.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: OsString,
    pub kind: EntryKind,
    /// Reported length. Meaningful for files.
    pub logical: u64,
    /// Locally allocated bytes, or `None` if the platform couldn't say.
    pub allocated: Option<u64>,
    pub identity: Identity,
    /// The filesystem object's ID (file ID with Windows, inode number
    /// elsewhere), so a later delete can tell it from a replacement; 0 when
    /// unknown.
    pub id: u128,
    /// The entry belongs to a cloud provider (placeholder or synced file).
    pub cloud: bool,
    /// Last write time, if the listing gave one. Recorded for files.
    pub modified: Option<SystemTime>,
}

/// Authoritative sizes from a per-file query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub logical: u64,
    pub allocated: Option<u64>,
}

/// Classification used for omission reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorKind {
    PermissionDenied,
    /// The entry disappeared between listing and use.
    Vanished,
    /// The device or share stopped responding or was removed.
    Disconnected,
    /// Metadata couldn't be obtained (unsupported call, unreadable entry).
    Unavailable,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ScanError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ScanError {}

impl From<std::io::Error> for ScanError {
    fn from(err: std::io::Error) -> Self {
        Self::new(classify_io(&err), err.to_string())
    }
}

/// Maps OS errors to omission categories.
pub fn classify_io(err: &std::io::Error) -> ErrorKind {
    use std::io::ErrorKind as K;
    match err.kind() {
        K::PermissionDenied => return ErrorKind::PermissionDenied,
        K::NotFound => return ErrorKind::Vanished,
        _ => {}
    }
    let Some(code) = err.raw_os_error() else {
        return ErrorKind::Other;
    };
    #[cfg(windows)]
    {
        // ERROR_NOT_READY, ERROR_BAD_NETPATH, ERROR_DEV_NOT_EXIST,
        // ERROR_UNEXP_NET_ERR, ERROR_NETNAME_DELETED, ERROR_DEVICE_NOT_CONNECTED,
        // ERROR_NO_SUCH_DEVICE.
        if matches!(code, 21 | 53 | 55 | 59 | 64 | 1167 | 433) {
            return ErrorKind::Disconnected;
        }
        // ERROR_INVALID_FUNCTION, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION,
        // ERROR_NOT_SUPPORTED, ERROR_INVALID_PARAMETER.
        if matches!(code, 1 | 32 | 33 | 50 | 87) {
            return ErrorKind::Unavailable;
        }
    }
    #[cfg(unix)]
    {
        // ENXIO, ENODEV, EIO, ENOTCONN, ESTALE, EHOSTDOWN.
        if matches!(code, 6 | 19 | 5 | 107 | 116 | 112) {
            return ErrorKind::Disconnected;
        }
    }
    let _ = code;
    ErrorKind::Other
}

/// What the root resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Directory,
    /// The root itself is a link or mount redirection; the user must pick the
    /// real folder.
    Link,
    NotDirectory,
}

/// Receives a listing's entries batch by batch; `Break` stops the listing.
pub type Sink<'a> = dyn FnMut(Vec<Entry>) -> ControlFlow<()> + 'a;

/// Gathers entries into batches of [`BATCH_ENTRIES`] for a [`Sink`].
pub struct Batcher<'s, 'a> {
    sink: &'s mut Sink<'a>,
    batch: Vec<Entry>,
    pushed: u64,
    stopped: bool,
}

impl<'s, 'a> Batcher<'s, 'a> {
    pub fn new(sink: &'s mut Sink<'a>) -> Self {
        Self {
            sink,
            batch: Vec::new(),
            pushed: 0,
            stopped: false,
        }
    }

    /// Adds one entry, handing over a full batch. `Break` means the sink
    /// wants no more, so the listing should stop.
    pub fn push(&mut self, entry: Entry) -> ControlFlow<()> {
        if self.stopped {
            return ControlFlow::Break(());
        }
        self.batch.push(entry);
        self.pushed += 1;
        if self.batch.len() >= BATCH_ENTRIES {
            self.flush()
        } else {
            ControlFlow::Continue(())
        }
    }

    /// Entries pushed so far.
    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    /// Hands over the entries not yet passed on.
    fn flush(&mut self) -> ControlFlow<()> {
        if self.stopped || self.batch.is_empty() {
            return ControlFlow::Continue(());
        }
        let flow = (self.sink)(std::mem::take(&mut self.batch));
        self.stopped = flow.is_break();
        flow
    }
}

/// Runs a listing that pushes entries into a [`Batcher`], then hands over
/// the last partial batch, also when the listing failed part way so the
/// entries read before the error still arrive.
pub fn batched(
    sink: &mut Sink<'_>,
    list: impl FnOnce(&mut Batcher<'_, '_>) -> Result<(), ScanError>,
) -> Result<(), ScanError> {
    let mut out = Batcher::new(sink);
    let result = list(&mut out);
    let _ = out.flush();
    result
}

/// Filesystem access for one scan. Implementations must never follow links,
/// read file contents (except through [`FsAdapter::read_small`], used only
/// for Git metadata files: `HEAD`, a `.git` file, `commondir`, `gitdir`,
/// `config`), or trigger cloud hydration.
pub trait FsAdapter: Send + Sync + 'static {
    /// Inspects the root without following it and fixes the scan's
    /// filesystem boundary for later [`FsAdapter::read_dir`] calls.
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError>;

    /// Lists one directory, handing its entries to `sink` in batches of at
    /// most [`BATCH_ENTRIES`] as they are read (see [`batched`]), so a huge
    /// directory is never held whole. Returns `Ok` early once the sink
    /// returns `Break`. An error after some batches means the rest of the
    /// directory couldn't be listed. Directories on other filesystems must
    /// be reported as [`EntryKind::MountPoint`].
    fn read_dir(&self, dir: &Path, sink: &mut Sink<'_>) -> Result<(), ScanError>;

    /// Re-measures one file authoritatively without following links or
    /// hydrating. Used for hard-link owners whose listing may be stale.
    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError>;

    /// Reads a small metadata file whole: never through a link (in the last
    /// component), never a cloud placeholder or offline file, and only a
    /// regular file. Refusals use the messages in [`refused`]; a file over
    /// `cap` bytes fails with [`refused::TOO_LARGE`].
    fn read_small(&self, path: &Path, cap: usize) -> Result<Vec<u8>, ScanError> {
        let _ = (path, cap);
        Err(ScanError::new(ErrorKind::Unavailable, "not supported"))
    }

    /// What `path` is, without following it (`lstat`). A directory is
    /// reported as [`EntryKind::Directory`] whatever filesystem it's on.
    fn stat_kind(&self, path: &Path) -> Result<EntryKind, ScanError> {
        let _ = path;
        Err(ScanError::new(ErrorKind::Unavailable, "not supported"))
    }

    /// How paths written in Git metadata read on this filesystem.
    fn path_style(&self) -> Style {
        Style::native()
    }

    /// Whether drive `letter` (Windows, upper case) is a local fixed disk,
    /// where Git metadata outside the scan may be read. Network, removable
    /// and unknown drives aren't.
    fn local_drive(&self, letter: u8) -> bool {
        let _ = letter;
        false
    }
}

/// The messages [`FsAdapter::read_small`] refuses with, so a caller can tell
/// why without parsing OS errors.
pub mod refused {
    /// A link or other redirection, which is never read through.
    /// [`ErrorKind::Unavailable`](super::ErrorKind::Unavailable).
    pub const LINK: &str = "a link, not followed";
    /// A cloud placeholder or offline file, which reading would download.
    /// [`ErrorKind::Unavailable`](super::ErrorKind::Unavailable).
    pub const CLOUD: &str = "a cloud file, not downloaded";
    /// A folder, device, or file stored behind a filter.
    /// [`ErrorKind::Unavailable`](super::ErrorKind::Unavailable).
    pub const NOT_FILE: &str = "not a plain file";
    /// Larger than the cap. [`ErrorKind::Other`](super::ErrorKind::Other).
    pub const TOO_LARGE: &str = "too large";
}

/// Reads `reader` to its end, failing with [`refused::TOO_LARGE`] past
/// `cap` bytes; for [`FsAdapter::read_small`].
pub fn read_capped(reader: impl Read, cap: usize) -> Result<Vec<u8>, ScanError> {
    let mut bytes = Vec::new();
    reader
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(ScanError::from)?;
    if bytes.len() > cap {
        return Err(ScanError::new(ErrorKind::Other, refused::TOO_LARGE));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: usize) -> Entry {
        Entry {
            name: format!("f{i}").into(),
            kind: EntryKind::File,
            logical: i as u64,
            allocated: Some(0),
            identity: Identity::Unique,
            id: 0,
            cloud: false,
            modified: None,
        }
    }

    #[test]
    fn batches_are_bounded_and_the_remainder_arrives_last() {
        let mut sizes = Vec::new();
        let mut order = Vec::new();
        let result = batched(
            &mut |batch| {
                sizes.push(batch.len());
                order.extend(batch.into_iter().map(|e| e.logical));
                ControlFlow::Continue(())
            },
            |out| {
                for i in 0..2 * BATCH_ENTRIES + 3 {
                    assert!(out.push(entry(i)).is_continue());
                }
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(sizes, [BATCH_ENTRIES, BATCH_ENTRIES, 3]);
        assert!(order.into_iter().eq(0..2 * BATCH_ENTRIES as u64 + 3));
    }

    #[test]
    fn entries_read_before_an_error_still_arrive() {
        let mut got = 0;
        let result = batched(
            &mut |batch| {
                got += batch.len();
                ControlFlow::Continue(())
            },
            |out| {
                for i in 0..BATCH_ENTRIES + 10 {
                    let _ = out.push(entry(i));
                }
                Err(ScanError::new(ErrorKind::Disconnected, "gone"))
            },
        );
        assert_eq!(result.unwrap_err().kind, ErrorKind::Disconnected);
        assert_eq!(got, BATCH_ENTRIES + 10);
    }

    #[test]
    fn a_sink_that_breaks_receives_nothing_more() {
        let mut calls = 0;
        let mut stopped_at = 0;
        let result = batched(
            &mut |_| {
                calls += 1;
                ControlFlow::Break(())
            },
            |out| {
                for i in 0..3 * BATCH_ENTRIES {
                    if out.push(entry(i)).is_break() {
                        stopped_at = out.pushed();
                        break;
                    }
                }
                // A listing that pushes anyway is ignored.
                assert!(out.push(entry(0)).is_break());
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(calls, 1);
        assert_eq!(stopped_at, BATCH_ENTRIES as u64);
    }
}

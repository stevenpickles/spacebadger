//! The filesystem interface the scan engine drives. Native adapters implement
//! it per platform; [`crate::fake::FakeFs`] implements it in memory for tests.

use std::ffi::OsString;
use std::path::Path;

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
    /// The entry belongs to a cloud provider (placeholder or synced file).
    pub cloud: bool,
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

/// Filesystem access for one scan. Implementations must never follow links,
/// read file contents, or trigger cloud hydration.
pub trait FsAdapter: Send + Sync + 'static {
    /// Inspects the root without following it and fixes the scan's
    /// filesystem boundary for later [`FsAdapter::read_dir`] calls.
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError>;

    /// Lists one directory. Directories on other filesystems must be reported
    /// as [`EntryKind::MountPoint`].
    fn read_dir(&self, dir: &Path) -> Result<Vec<Entry>, ScanError>;

    /// Re-measures one file authoritatively without following links or
    /// hydrating. Used for hard-link owners whose listing may be stale.
    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError>;
}

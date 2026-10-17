//! Windows adapter. Decisions and evidence: `docs/validation/windows-m1.md`.
//!
//! - Lists with `FileIdExtdDirectoryInfo` (falling back to
//!   `FileIdBothDirectoryInfo` where unsupported) so sizes, attributes,
//!   reparse tags, and file IDs come from the listing without opening files.
//!   Entries are passed on as each 64 KiB buffer is decoded, in batches, so
//!   a huge folder is never held whole.
//! - Allocated size is the listing's `AllocationSize`; logical is `EndOfFile`.
//!   The modified time is the listing's `LastWriteTime`.
//! - Symlinks and mount points (junctions and volume mounts) are reported, not
//!   followed, including file symlinks. Cloud-tagged entries are measured.
//! - Cloud state comes from attributes because Cloud Files disguises
//!   placeholder reparse tags for ordinary processes.
//! - When the process is already elevated, `SeBackupPrivilege` is enabled so
//!   directories opened with `FILE_FLAG_BACKUP_SEMANTICS` bypass ACL checks
//!   (protected folders become listable). Elevation is never requested.
//! - Per-file opens use `FILE_READ_ATTRIBUTES` with `OPEN_REPARSE_POINT` and
//!   `OPEN_NO_RECALL`, which the probes showed don't hydrate placeholders.
//! - Finding duplicates reads contents only after such an open shows a plain
//!   local file, and reads with `OPEN_REPARSE_POINT` and `OPEN_NO_RECALL` too.

use crate::adapter::{
    Batcher, Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError, Sink,
    batched, classify_io, read_capped, refused,
};
use crate::duplicates::{Fingerprint, Skip};
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{
    ERROR_NO_MORE_FILES, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_BACKUP_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAG_SEQUENTIAL_SCAN, FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_BOTH_DIR_INFO,
    FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo, FileBasicInfo,
    FileIdBothDirectoryInfo, FileIdExtdDirectoryInfo, FileIdInfo, FileStandardInfo,
    GetDiskFreeSpaceExW, GetDriveTypeW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetVolumeInformationW, GetVolumePathNameW, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

mod attr {
    pub const DIRECTORY: u32 = 0x10;
    pub const DEVICE: u32 = 0x40;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const OFFLINE: u32 = 0x1000;
    pub const RECALL_ON_OPEN: u32 = 0x40000;
    pub const PINNED: u32 = 0x80000;
    pub const UNPINNED: u32 = 0x100000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    pub const CLOUD: u32 = OFFLINE | RECALL_ON_OPEN | PINNED | UNPINNED | RECALL_ON_DATA_ACCESS;
}

mod tag {
    pub const MOUNT_POINT: u32 = 0xA000_0003;
    pub const SYMLINK: u32 = 0xA000_000C;
    pub const LX_SYMLINK: u32 = 0xA000_001D;
    pub const WCI_LINK: u32 = 0xA000_0027;
    pub const AF_UNIX: u32 = 0x8000_0023;
    pub const LX_FIFO: u32 = 0x8000_0024;
    pub const LX_CHR: u32 = 0x8000_0025;
    pub const LX_BLK: u32 = 0x8000_0026;
    pub const WOF: u32 = 0x8000_0017;

    /// `IO_REPARSE_TAG_CLOUD` and its `CLOUD_1`..`CLOUD_F` variants.
    pub fn is_cloud(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == 0x9000_001A
    }

    /// Name-surrogate tags redirect to another path (bit 29).
    pub fn is_name_surrogate(tag: u32) -> bool {
        tag & 0x2000_0000 != 0
    }
}

/// Classifies a listed entry from its attributes and reparse tag.
fn classify(attributes: u32, reparse_tag: u32) -> EntryKind {
    let is_dir = attributes & attr::DIRECTORY != 0;
    if attributes & attr::REPARSE_POINT != 0 {
        match reparse_tag {
            tag::SYMLINK | tag::LX_SYMLINK | tag::WCI_LINK => return EntryKind::Symlink,
            tag::MOUNT_POINT => return EntryKind::MountPoint,
            tag::AF_UNIX | tag::LX_FIFO | tag::LX_CHR | tag::LX_BLK => return EntryKind::Special,
            t if tag::is_cloud(t) => {}
            // Other redirections (e.g. global reparse) are not followed.
            t if tag::is_name_surrogate(t) => return EntryKind::UnknownReparse,
            // Unknown directory reparse points may redirect; skip them.
            // Unknown file tags (dedup, WOF, HSM, ...) are measured as files.
            _ if is_dir => return EntryKind::UnknownReparse,
            _ => {}
        }
    }
    if is_dir {
        EntryKind::Directory
    } else if attributes & attr::DEVICE != 0 {
        EntryKind::Special
    } else {
        EntryKind::File
    }
}

fn scan_error(err: windows::core::Error) -> ScanError {
    let code = err.code().0 as u32;
    let kind = if code & 0xFFFF_0000 == 0x8007_0000 {
        classify_io(&std::io::Error::from_raw_os_error((code & 0xFFFF) as i32))
    } else {
        ErrorKind::Other
    };
    ScanError::new(kind, err.message())
}

/// NUL-terminated wide path in the `\\?\` form so long paths work.
fn wide(path: &Path) -> Vec<u16> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let starts = |prefix: &str| text.starts_with(&prefix.encode_utf16().collect::<Vec<_>>());
    let mut out: Vec<u16> = if starts(r"\\?\") || starts(r"\\.\") {
        text
    } else if starts(r"\\") {
        r"\\?\UNC\"
            .encode_utf16()
            .chain(text[2..].iter().copied())
            .collect()
    } else {
        r"\\?\".encode_utf16().chain(text).collect()
    };
    out.push(0);
    out
}

fn open(path: &Path, access: u32) -> Result<OwnedHandle, ScanError> {
    open_with(
        path,
        access,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OPEN_NO_RECALL,
    )
    .map_err(scan_error)
}

fn open_with(
    path: &Path,
    access: u32,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
) -> windows::core::Result<OwnedHandle> {
    open_shared(
        path,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        flags,
    )
}

fn open_shared(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
) -> windows::core::Result<OwnedHandle> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            share,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }?;
    // SAFETY: CreateFileW returned a valid handle that we now own.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

/// The OS error behind a failed call, so [`Skip::of`] can classify it.
fn io_error(err: &windows::core::Error) -> std::io::Error {
    let code = err.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        std::io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        std::io::Error::other(err.message())
    }
}

/// Opens a file to read its contents for finding duplicates. A first open
/// reads only attributes, which never recalls anything; a cloud file
/// (placeholder, pinned, or offline), a link, a file stored behind another
/// reparse point (deduplicated, tiered, or virtualized, whose raw contents
/// aren't the file's), or a file whose size changed since the scan is
/// refused there. The open that reads is made with `OPEN_NO_RECALL` and
/// (except for WOF-compressed files, which only read right through their
/// filter) `OPEN_REPARSE_POINT`, must be the same file, and is checked
/// again.
pub fn open_contents(path: &Path, len: u64) -> Result<(std::fs::File, Fingerprint), Skip> {
    let skip = |e: windows::core::Error| Skip::of(&io_error(&e));
    let probe = probe_contents(path)?;
    let compressed = check_contents(&probe, len)?;
    let id = file_id(&probe);
    drop(probe);
    let mut flags = FILE_FLAG_OPEN_NO_RECALL | FILE_FLAG_SEQUENTIAL_SCAN;
    if !compressed {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let handle = open_with(path, FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0, flags).map_err(skip)?;
    // A filesystem that gives no ID (FAT, some shares) relies on the
    // other checks.
    if let (Some(before), Some(now)) = (id, file_id(&handle))
        && before != now
    {
        return Err(Skip::Changed);
    }
    check_contents(&handle, len)?;
    let print = fingerprint(&handle)?;
    Ok((std::fs::File::from(handle), print))
}

/// Opens the file at `path` for reading while sharing only reading, so
/// until the handle is closed no one can write to, rename, replace, or
/// delete it, and checks it's the file `print` describes. Refused (as in
/// use) while another program has it open to write or delete. Checked
/// first with an attributes-only open, as [`open_contents`] does, so a
/// cloud file is never opened for its data.
pub fn hold_contents(path: &Path, print: &Fingerprint) -> Result<OwnedHandle, Skip> {
    let probe = probe_contents(path)?;
    let compressed = check_contents(&probe, print.len)?;
    drop(probe);
    let mut flags = FILE_FLAG_OPEN_NO_RECALL;
    if !compressed {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let handle = open_shared(
        path,
        FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0,
        FILE_SHARE_READ,
        flags,
    )
    .map_err(|e| Skip::of(&io_error(&e)))?;
    check_contents(&handle, print.len)?;
    if !print.unchanged(&fingerprint(&handle)?) {
        return Err(Skip::Changed);
    }
    Ok(handle)
}

/// The fingerprint of the file at `path`, read with the attributes-only
/// open [`open_contents`] starts with, refusing what it refuses.
pub fn examine_contents(path: &Path, len: u64) -> Result<Fingerprint, Skip> {
    let probe = probe_contents(path)?;
    check_contents(&probe, len)?;
    fingerprint(&probe)
}

/// Opens a file for its attributes only, which never recalls anything.
fn probe_contents(path: &Path) -> Result<OwnedHandle, Skip> {
    open_with(
        path,
        FILE_READ_ATTRIBUTES.0,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OPEN_NO_RECALL,
    )
    .map_err(|e| Skip::of(&io_error(&e)))
}

/// A handle's file ID, length, last write time, and number of names.
pub fn fingerprint(handle: &impl AsRawHandle) -> Result<Fingerprint, Skip> {
    let standard = info::<FILE_STANDARD_INFO>(handle, FileStandardInfo).map_err(info_skip)?;
    let basic = info::<FILE_BASIC_INFO>(handle, FileBasicInfo).map_err(info_skip)?;
    Ok(Fingerprint {
        id: file_id(handle).map(|(volume, id)| (volume, u128::from_le_bytes(id))),
        len: standard.EndOfFile as u64,
        modified: filetime(basic.LastWriteTime),
        links: Some(standard.NumberOfLinks),
    })
}

/// Whether contents can be read from a handle with these attributes and
/// reparse tag: `Ok(true)` for a WOF-compressed file, `Ok(false)` for a
/// plain one.
fn readable(attributes: u32, reparse_tag: u32) -> Result<bool, Skip> {
    let reparse = attributes & attr::REPARSE_POINT != 0;
    if attributes & attr::CLOUD != 0 || (reparse && tag::is_cloud(reparse_tag)) {
        return Err(Skip::Cloud);
    }
    if classify(attributes, reparse_tag) != EntryKind::File {
        return Err(Skip::Changed);
    }
    match reparse_tag {
        _ if !reparse => Ok(false),
        tag::WOF => Ok(true),
        _ => Err(Skip::Special),
    }
}

fn info_skip(e: ScanError) -> Skip {
    match e.kind {
        ErrorKind::Vanished => Skip::Vanished,
        ErrorKind::PermissionDenied => Skip::Denied,
        _ => Skip::Unreadable,
    }
}

/// Checks a handle's file against the scan; see [`readable`].
fn check_contents(handle: &OwnedHandle, len: u64) -> Result<bool, Skip> {
    let tag_info =
        info::<FILE_ATTRIBUTE_TAG_INFO>(handle, FileAttributeTagInfo).map_err(info_skip)?;
    let compressed = readable(tag_info.FileAttributes, tag_info.ReparseTag)?;
    let standard = info::<FILE_STANDARD_INFO>(handle, FileStandardInfo).map_err(info_skip)?;
    if standard.EndOfFile as u64 != len {
        return Err(Skip::Changed);
    }
    Ok(compressed)
}

/// The volume and file ID of a handle's file: the 128-bit ID where the
/// filesystem has one, else the older 64-bit index, else `None`.
fn file_id(handle: &impl AsRawHandle) -> Option<(u64, [u8; 16])> {
    if let Ok(id) = info::<FILE_ID_INFO>(handle, FileIdInfo) {
        return Some((id.VolumeSerialNumber, id.FileId.Identifier));
    }
    let mut by_handle = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(raw(handle), &mut by_handle) }.ok()?;
    let index = (u64::from(by_handle.nFileIndexHigh) << 32) | u64::from(by_handle.nFileIndexLow);
    if index == 0 {
        return None;
    }
    let mut id = [0; 16];
    id[..8].copy_from_slice(&index.to_le_bytes());
    Some((u64::from(by_handle.dwVolumeSerialNumber), id))
}

fn raw(handle: &impl AsRawHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn info<T: Default>(
    handle: &impl AsRawHandle,
    class: FILE_INFO_BY_HANDLE_CLASS,
) -> Result<T, ScanError> {
    let mut value = T::default();
    unsafe {
        GetFileInformationByHandleEx(
            raw(handle),
            class,
            (&raw mut value).cast::<c_void>(),
            size_of::<T>() as u32,
        )
    }
    .map_err(scan_error)?;
    Ok(value)
}

/// Fields shared by both directory-information classes.
struct Listed {
    name: OsString,
    end_of_file: u64,
    allocation_size: u64,
    attributes: u32,
    reparse_tag: u32,
    file_id: u128,
    /// `LastWriteTime`: 100 ns intervals since 1601, 0 when not set.
    last_write: i64,
}

/// Seconds from 1601-01-01, where `FILETIME` counts from, to the Unix epoch.
const FILETIME_EPOCH_OFFSET: u64 = 11_644_473_600;

/// A `FILETIME` as a time; `None` for 0, which means the filesystem has none.
fn filetime(ticks: i64) -> Option<SystemTime> {
    let ticks = u64::try_from(ticks).ok().filter(|&t| t != 0)?;
    let since_1601 = Duration::new(ticks / 10_000_000, (ticks % 10_000_000) as u32 * 100);
    let offset = Duration::from_secs(FILETIME_EPOCH_OFFSET);
    match since_1601.checked_sub(offset) {
        Some(after) => UNIX_EPOCH.checked_add(after),
        None => UNIX_EPOCH.checked_sub(offset - since_1601),
    }
}

/// Windows filesystem adapter. Create one per scan.
#[derive(Default)]
pub struct WindowsFs {
    /// Set once the extended class is found unsupported on this volume.
    use_both_dir_info: AtomicBool,
    /// Per drive letter: 0 not asked yet, 1 local, 2 not.
    local_drives: Mutex<[u8; 26]>,
}

impl WindowsFs {
    /// Also enables backup access when the process is already elevated;
    /// see [`enable_backup_privilege`].
    pub fn new() -> Self {
        enable_backup_privilege();
        Self::default()
    }

    fn list(&self, dir: &Path, out: &mut Batcher<'_, '_>) -> Result<(), ScanError> {
        let handle = open(dir, FILE_LIST_DIRECTORY.0)?;
        if !self.use_both_dir_info.load(Ordering::Relaxed) {
            match list_with::<FILE_ID_EXTD_DIR_INFO>(&handle, FileIdExtdDirectoryInfo, out) {
                // Only before any entry was read, so none is listed twice.
                Err(err) if err.kind == ErrorKind::Unavailable && out.pushed() == 0 => {
                    self.use_both_dir_info.store(true, Ordering::Relaxed);
                }
                result => return result,
            }
        }
        list_with::<FILE_ID_BOTH_DIR_INFO>(&handle, FileIdBothDirectoryInfo, out)
    }
}

impl Listed {
    fn entry(self) -> Entry {
        Entry {
            kind: classify(self.attributes, self.reparse_tag),
            logical: self.end_of_file,
            allocated: Some(self.allocation_size),
            // Listings carry no link count, so every file may be shared.
            identity: if self.file_id == 0 {
                Identity::Unavailable
            } else {
                Identity::MaybeShared { key: self.file_id }
            },
            id: self.file_id,
            cloud: self.attributes & attr::CLOUD != 0
                || (self.attributes & attr::REPARSE_POINT != 0 && tag::is_cloud(self.reparse_tag)),
            modified: filetime(self.last_write),
            name: self.name,
        }
    }
}

/// A directory-information record with a trailing UTF-16 name.
trait DirInfo: Copy {
    const NAME_OFFSET: usize;
    fn next(&self) -> u32;
    /// Name length in UTF-16 units.
    fn name_len(&self) -> usize;
    fn listed(&self, name: &[u16]) -> Listed;
}

impl DirInfo for FILE_ID_EXTD_DIR_INFO {
    const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);

    fn next(&self) -> u32 {
        self.NextEntryOffset
    }

    fn name_len(&self) -> usize {
        self.FileNameLength as usize / 2
    }

    fn listed(&self, name: &[u16]) -> Listed {
        Listed {
            name: OsString::from_wide(name),
            end_of_file: self.EndOfFile as u64,
            allocation_size: self.AllocationSize as u64,
            attributes: self.FileAttributes,
            reparse_tag: self.ReparsePointTag,
            file_id: u128::from_le_bytes(self.FileId.Identifier),
            last_write: self.LastWriteTime,
        }
    }
}

impl DirInfo for FILE_ID_BOTH_DIR_INFO {
    const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);

    fn next(&self) -> u32 {
        self.NextEntryOffset
    }

    fn name_len(&self) -> usize {
        self.FileNameLength as usize / 2
    }

    fn listed(&self, name: &[u16]) -> Listed {
        Listed {
            name: OsString::from_wide(name),
            end_of_file: self.EndOfFile as u64,
            allocation_size: self.AllocationSize as u64,
            attributes: self.FileAttributes,
            // For reparse points this class reports the tag in EaSize.
            reparse_tag: self.EaSize,
            file_id: self.FileId as u64 as u128,
            last_write: self.LastWriteTime,
        }
    }
}

/// Reads a directory a buffer at a time, passing each entry on as it's
/// decoded; stops early when the batcher says so.
fn list_with<T: DirInfo>(
    handle: &OwnedHandle,
    class: FILE_INFO_BY_HANDLE_CLASS,
    out: &mut Batcher<'_, '_>,
) -> Result<(), ScanError> {
    let mut buffer = vec![0u64; 8192];
    loop {
        let result = unsafe {
            GetFileInformationByHandleEx(
                raw(handle),
                class,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * size_of::<u64>()) as u32,
            )
        };
        match result {
            Ok(()) => {}
            Err(err) if err.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(()),
            Err(err) => return Err(scan_error(err)),
        }
        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the API fills the buffer with 8-byte-aligned records
            // chained by NextEntryOffset, each followed by its name.
            let record = unsafe { base.add(offset).cast::<T>().read() };
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + T::NAME_OFFSET).cast::<u16>(),
                    record.name_len(),
                )
            };
            if name != [0x2e]
                && name != [0x2e, 0x2e]
                && out.push(record.listed(name).entry()).is_break()
            {
                return Ok(());
            }
            if record.next() == 0 {
                break;
            }
            offset += record.next() as usize;
        }
    }
}

impl FsAdapter for WindowsFs {
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError> {
        let handle = open(root, FILE_READ_ATTRIBUTES.0)?;
        let tag_info = info::<FILE_ATTRIBUTE_TAG_INFO>(&handle, FileAttributeTagInfo)?;
        Ok(
            match classify(tag_info.FileAttributes, tag_info.ReparseTag) {
                EntryKind::Directory => RootKind::Directory,
                EntryKind::Symlink | EntryKind::MountPoint | EntryKind::UnknownReparse => {
                    RootKind::Link
                }
                _ => RootKind::NotDirectory,
            },
        )
    }

    fn read_dir(&self, dir: &Path, sink: &mut Sink<'_>) -> Result<(), ScanError> {
        batched(sink, |out| self.list(dir, out))
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        let handle = open(file, FILE_READ_ATTRIBUTES.0)?;
        let standard = info::<FILE_STANDARD_INFO>(&handle, FileStandardInfo)?;
        Ok(Measured {
            logical: standard.EndOfFile as u64,
            allocated: Some(standard.AllocationSize as u64),
        })
    }

    /// Checked first with an attributes-only open, as finding duplicates
    /// does, so a link, a cloud file or a file behind another filter is
    /// refused before anything opens its data; the open that reads uses
    /// `OPEN_NO_RECALL` (and `OPEN_REPARSE_POINT` but for a WOF-compressed
    /// file) and must be the same file.
    fn read_small(&self, path: &Path, cap: usize) -> Result<Vec<u8>, ScanError> {
        // Backup semantics so a folder or junction opens too, to be refused
        // for what it is.
        let probe = open_metadata(path, FILE_READ_ATTRIBUTES.0)?;
        let compressed = small_readable(&probe)?;
        let id = file_id(&probe);
        drop(probe);
        let mut flags = FILE_FLAG_OPEN_NO_RECALL
            | FILE_FLAG_SEQUENTIAL_SCAN
            | SECURITY_SQOS_PRESENT
            | SECURITY_IDENTIFICATION;
        if !compressed {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        let handle = open_with(path, FILE_READ_DATA.0 | FILE_READ_ATTRIBUTES.0, flags)
            .map_err(scan_error)?;
        if let (Some(before), Some(now)) = (id, file_id(&handle))
            && before != now
        {
            return Err(ScanError::new(ErrorKind::Vanished, "replaced while read"));
        }
        small_readable(&handle)?;
        read_capped(std::fs::File::from(handle), cap)
    }

    fn stat_kind(&self, path: &Path) -> Result<EntryKind, ScanError> {
        let handle = open_metadata(path, FILE_READ_ATTRIBUTES.0)?;
        let tag_info = info::<FILE_ATTRIBUTE_TAG_INFO>(&handle, FileAttributeTagInfo)?;
        Ok(classify(tag_info.FileAttributes, tag_info.ReparseTag))
    }

    /// A fixed or RAM disk, by `GetDriveTypeW`, asked once per drive.
    fn local_drive(&self, letter: u8) -> bool {
        if !letter.is_ascii_uppercase() {
            return false;
        }
        let slot = usize::from(letter - b'A');
        let mut known = self.local_drives.lock().unwrap();
        if known[slot] == 0 {
            let root: Vec<u16> = format!("{}:\\", char::from(letter))
                .encode_utf16()
                .chain([0])
                .collect();
            const DRIVE_FIXED: u32 = 3;
            const DRIVE_RAMDISK: u32 = 6;
            let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
            known[slot] = if matches!(kind, DRIVE_FIXED | DRIVE_RAMDISK) {
                1
            } else {
                2
            };
        }
        known[slot] == 1
    }
}

/// Opens Git metadata for its attributes as [`open`] does, and as the
/// file's owner only identifies us: a path that reaches a server anyway
/// can't act with our credentials.
fn open_metadata(path: &Path, access: u32) -> Result<OwnedHandle, ScanError> {
    open_with(
        path,
        access,
        FILE_FLAG_BACKUP_SEMANTICS
            | FILE_FLAG_OPEN_REPARSE_POINT
            | FILE_FLAG_OPEN_NO_RECALL
            | SECURITY_SQOS_PRESENT
            | SECURITY_IDENTIFICATION,
    )
    .map_err(scan_error)
}

/// Whether [`FsAdapter::read_small`] may read a handle's file, as
/// [`readable`] decides: `Ok(true)` for a WOF-compressed file.
fn small_readable(handle: &OwnedHandle) -> Result<bool, ScanError> {
    let tag_info = info::<FILE_ATTRIBUTE_TAG_INFO>(handle, FileAttributeTagInfo)?;
    let (attributes, reparse_tag) = (tag_info.FileAttributes, tag_info.ReparseTag);
    readable(attributes, reparse_tag).map_err(|skip| {
        let why = match (skip, classify(attributes, reparse_tag)) {
            (Skip::Cloud, _) => refused::CLOUD,
            (_, EntryKind::Symlink | EntryKind::MountPoint | EntryKind::UnknownReparse) => {
                refused::LINK
            }
            _ => refused::NOT_FILE,
        };
        ScanError::new(ErrorKind::Unavailable, why)
    })
}

/// Enables `SeBackupPrivilege` when this process already holds it (it was
/// started elevated), so protected folders list like ordinary ones. Never
/// requests elevation. Returns whether the privilege is enabled.
pub fn enable_backup_privilege() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| try_enable_backup_privilege().unwrap_or(false))
}

fn try_enable_backup_privilege() -> windows::core::Result<bool> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }?;
    // SAFETY: OpenProcessToken returned a valid handle that we now own.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let mut luid = LUID::default();
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_BACKUP_NAME, &mut luid) }?;
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    unsafe { AdjustTokenPrivileges(raw(&token), false, Some(&privileges), 0, None, None) }?;
    // The call succeeds without enabling anything when the token lacks the
    // privilege; only the last error says so.
    Ok(unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED)
}

/// Whether `path` is on a network share (UNC path or mapped network drive).
pub fn is_remote(path: &Path) -> bool {
    let path = wide(path);
    let unc: Vec<u16> = r"\?\UNC\".encode_utf16().collect();
    if path.starts_with(&unc) {
        return true;
    }
    let mut volume = vec![0u16; path.len() + 1];
    if unsafe { GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut volume) }.is_err() {
        return false;
    }
    const DRIVE_REMOTE: u32 = 4;
    unsafe { GetDriveTypeW(PCWSTR(volume.as_ptr())) == DRIVE_REMOTE }
}

/// Capacity and free space of the volume holding `path`, whether `path` is
/// its top folder, and the NTFS master file table's size when the volume
/// can be queried (usually only when elevated).
pub fn volume_info(path: &Path) -> Option<super::VolumeInfo> {
    let path = wide(path);
    let mut volume = vec![0u16; path.len() + 1];
    unsafe { GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut volume) }.ok()?;
    let volume_len = volume.iter().position(|&c| c == 0)?;
    let (mut capacity, mut free) = (0u64, 0u64);
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(volume.as_ptr()),
            None,
            Some(&mut capacity),
            Some(&mut free),
        )
    }
    .ok()?;
    let mut fs_name = [0u16; 64];
    let filesystem = unsafe {
        GetVolumeInformationW(
            PCWSTR(volume.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
    }
    .ok()
    .map(|()| {
        let len = fs_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fs_name.len());
        String::from_utf16_lossy(&fs_name[..len])
    });
    let metadata = if filesystem.as_deref() == Some("NTFS") {
        ntfs_mft_bytes(&volume[..volume_len])
    } else {
        None
    };
    let trim = |s: &[u16]| -> Vec<u16> {
        let mut s = s.to_vec();
        while s.last().is_some_and(|&c| c == 0 || c == u16::from(b'\\')) {
            s.pop();
        }
        s.iter()
            .map(|&c| {
                if c < 128 {
                    u16::from((c as u8).to_ascii_lowercase())
                } else {
                    c
                }
            })
            .collect()
    };
    Some(super::VolumeInfo {
        is_root: trim(&path) == trim(&volume[..volume_len]),
        capacity,
        free,
        filesystem,
        metadata,
    })
}

/// Valid length of the NTFS master file table, read from the volume device.
/// Opening a volume needs administrator rights, so this is usually `None`
/// for ordinary processes.
fn ntfs_mft_bytes(volume: &[u16]) -> Option<u64> {
    // "\\?\C:\" names the root folder; "\\?\C:" names the volume device.
    let mut device: Vec<u16> = volume.to_vec();
    if device.last() == Some(&u16::from(b'\\')) {
        device.pop();
    }
    device.push(0);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(device.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    }
    .ok()?;
    // SAFETY: CreateFileW returned a valid handle that we now own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut data = NTFS_VOLUME_DATA_BUFFER::default();
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            raw(&handle),
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some((&raw mut data).cast::<c_void>()),
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .ok()?;
    u64::try_from(data.MftValidDataLength).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_filetimes() {
        assert_eq!(filetime(0), None);
        assert_eq!(filetime(-5), None);
        let epoch = FILETIME_EPOCH_OFFSET as i64 * 10_000_000;
        assert_eq!(filetime(epoch), Some(UNIX_EPOCH));
        assert_eq!(
            filetime(epoch + 15_000_000),
            Some(UNIX_EPOCH + Duration::from_millis(1500))
        );
        assert_eq!(
            filetime(epoch - 10_000_000),
            UNIX_EPOCH.checked_sub(Duration::from_secs(1))
        );
    }

    #[test]
    fn classifies_reparse_tags_by_category() {
        let rp = attr::REPARSE_POINT;
        let dir = attr::DIRECTORY;
        assert_eq!(classify(0x20, 0), EntryKind::File);
        assert_eq!(classify(dir, 0), EntryKind::Directory);
        assert_eq!(classify(rp, tag::SYMLINK), EntryKind::Symlink);
        assert_eq!(classify(dir | rp, tag::SYMLINK), EntryKind::Symlink);
        assert_eq!(classify(dir | rp, tag::MOUNT_POINT), EntryKind::MountPoint);
        assert_eq!(classify(rp, 0x9000_301A), EntryKind::File, "cloud file");
        assert_eq!(
            classify(dir | rp, 0x9000_001A),
            EntryKind::Directory,
            "cloud dir"
        );
        assert_eq!(classify(rp, 0x8000_0013), EntryKind::File, "dedup file");
        assert_eq!(classify(dir | rp, 0x8000_0013), EntryKind::UnknownReparse);
        assert_eq!(
            classify(rp, 0xA000_0019),
            EntryKind::UnknownReparse,
            "global reparse"
        );
        assert_eq!(classify(rp, tag::AF_UNIX), EntryKind::Special);
    }

    #[test]
    fn file_ids_match_for_one_file_and_differ_between_files() {
        let dir = std::env::temp_dir().join(format!("sb-file-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let id = |p: &Path| file_id(&open(p, FILE_READ_ATTRIBUTES.0).unwrap());
        assert!(id(&a).is_some());
        assert_eq!(id(&a), id(&a));
        assert_ne!(id(&a), id(&b));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_contents_only_of_plain_and_compressed_files() {
        let rp = attr::REPARSE_POINT;
        assert_eq!(readable(0x20, 0), Ok(false));
        assert_eq!(readable(0x20 | rp, tag::WOF), Ok(true));
        assert_eq!(
            readable(0x20 | rp, 0x8000_0013),
            Err(Skip::Special),
            "dedup"
        );
        assert_eq!(readable(0x20 | rp, 0x8000_0012), Err(Skip::Special), "SIS");
        assert_eq!(
            readable(0x20 | rp, 0x9000_001C),
            Err(Skip::Special),
            "ProjFS"
        );
        assert_eq!(readable(0x20 | rp, 0x9000_301A), Err(Skip::Cloud));
        assert_eq!(
            readable(0x20 | attr::RECALL_ON_DATA_ACCESS, 0),
            Err(Skip::Cloud)
        );
        assert_eq!(readable(0x20 | attr::OFFLINE, 0), Err(Skip::Cloud));
        assert_eq!(readable(rp, tag::SYMLINK), Err(Skip::Changed));
        assert_eq!(readable(attr::DIRECTORY, 0), Err(Skip::Changed));
    }
}

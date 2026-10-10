//! Windows adapter. Decisions and evidence: `docs/validation/windows-m1.md`.
//!
//! - Lists with `FileIdExtdDirectoryInfo` (falling back to
//!   `FileIdBothDirectoryInfo` where unsupported) so sizes, attributes,
//!   reparse tags, and file IDs come from the listing without opening files.
//! - Allocated size is the listing's `AllocationSize`; logical is `EndOfFile`.
//! - Symlinks and mount points (junctions and volume mounts) are reported, not
//!   followed, including file symlinks. Cloud-tagged entries are measured.
//! - Cloud state comes from attributes because Cloud Files disguises
//!   placeholder reparse tags for ordinary processes.
//! - When the process is already elevated, `SeBackupPrivilege` is enabled so
//!   directories opened with `FILE_FLAG_BACKUP_SEMANTICS` bypass ACL checks
//!   (protected folders become listable). Elevation is never requested.
//! - Per-file opens use `FILE_READ_ATTRIBUTES` with `OPEN_REPARSE_POINT` and
//!   `OPEN_NO_RECALL`, which the probes showed don't hydrate placeholders.

use crate::adapter::{
    Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError, classify_io,
};
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{
    ERROR_NO_MORE_FILES, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_BACKUP_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO,
    FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo,
    FileIdBothDirectoryInfo, FileIdExtdDirectoryInfo, FileStandardInfo, GetDiskFreeSpaceExW,
    GetDriveTypeW, GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumePathNameW,
    OPEN_EXISTING,
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
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OPEN_NO_RECALL,
            None,
        )
    }
    .map_err(scan_error)?;
    // SAFETY: CreateFileW returned a valid handle that we now own.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn info<T: Default>(
    handle: &OwnedHandle,
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
}

/// Windows filesystem adapter. Create one per scan.
#[derive(Default)]
pub struct WindowsFs {
    /// Set once the extended class is found unsupported on this volume.
    use_both_dir_info: AtomicBool,
}

impl WindowsFs {
    /// Also enables backup access when the process is already elevated;
    /// see [`enable_backup_privilege`].
    pub fn new() -> Self {
        enable_backup_privilege();
        Self::default()
    }

    fn list(&self, dir: &Path) -> Result<Vec<Listed>, ScanError> {
        let handle = open(dir, FILE_LIST_DIRECTORY.0)?;
        if !self.use_both_dir_info.load(Ordering::Relaxed) {
            match list_with::<FILE_ID_EXTD_DIR_INFO>(&handle, FileIdExtdDirectoryInfo) {
                Err(err) if err.kind == ErrorKind::Unavailable => {
                    self.use_both_dir_info.store(true, Ordering::Relaxed);
                }
                result => return result,
            }
        }
        list_with::<FILE_ID_BOTH_DIR_INFO>(&handle, FileIdBothDirectoryInfo)
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
        }
    }
}

fn list_with<T: DirInfo>(
    handle: &OwnedHandle,
    class: FILE_INFO_BY_HANDLE_CLASS,
) -> Result<Vec<Listed>, ScanError> {
    let mut out = Vec::new();
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
            Err(err) if err.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(out),
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
            if name != [0x2e] && name != [0x2e, 0x2e] {
                out.push(record.listed(name));
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

    fn read_dir(&self, dir: &Path) -> Result<Vec<Entry>, ScanError> {
        Ok(self
            .list(dir)?
            .into_iter()
            .map(|l| Entry {
                kind: classify(l.attributes, l.reparse_tag),
                logical: l.end_of_file,
                allocated: Some(l.allocation_size),
                // Listings carry no link count, so every file may be shared.
                identity: if l.file_id == 0 {
                    Identity::Unavailable
                } else {
                    Identity::MaybeShared { key: l.file_id }
                },
                cloud: l.attributes & attr::CLOUD != 0
                    || (l.attributes & attr::REPARSE_POINT != 0 && tag::is_cloud(l.reparse_tag)),
                name: l.name,
            })
            .collect())
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        let handle = open(file, FILE_READ_ATTRIBUTES.0)?;
        let standard = info::<FILE_STANDARD_INFO>(&handle, FileStandardInfo)?;
        Ok(Measured {
            logical: standard.EndOfFile as u64,
            allocated: Some(standard.AllocationSize as u64),
        })
    }
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
}

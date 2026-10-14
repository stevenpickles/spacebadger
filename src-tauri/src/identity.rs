//! What is on disk at a path right now, read without following links, for
//! telling the item the scan found from one put in its place. IDs are
//! compared as [`fingerprint`]s, the form the scan keeps.

use sb_core::tree::fingerprint;
use std::io;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
    /// A symbolic link, junction, mount point, or other redirection.
    Link,
    /// A device, pipe, or socket.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: Kind,
    /// The volume serial number with Windows, the device elsewhere.
    pub volume: u64,
    /// The fingerprints the object may have been recorded under (the scan
    /// lists a 128-bit or a 64-bit file ID depending on the filesystem); 0
    /// for none.
    pub ids: [u64; 2],
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl Found {
    /// Whether the object is the one recorded as `id`.
    pub fn has_id(&self, id: u64) -> bool {
        id != 0 && self.ids.contains(&id)
    }
}

/// Where a held item is after a delete.
#[derive(Debug, Clone, PartialEq, Eq)]
// Only Windows can tell where an item went.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Whereabouts {
    /// Still where it was when it was opened.
    Unmoved,
    /// Moved to this path on its volume (no drive), such as into the
    /// Recycle Bin.
    Moved(String),
    /// Deleted outright: NTFS moves a file deleted while open into
    /// `\$Extend\$Deleted` until its last handle closes.
    Deleted,
    /// The platform can't say (macOS, Linux) or the query failed.
    Unknown,
}

/// What [`hold`] opened; closed when dropped.
pub struct Held {
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    /// Where it was when opened, as the handle names it.
    #[cfg(windows)]
    at: Option<String>,
}

/// Reads what is at `path` without following a link. With Windows it stays
/// open while the returned [`Held`] lives, and with `lock` it also can't be
/// renamed or deleted meanwhile (unless someone else already has it open to
/// do so); folders above an item are held that way while it is deleted.
pub fn hold(path: &Path, lock: bool) -> io::Result<(Found, Held)> {
    imp::hold(path, lock)
}

impl Held {
    /// Where the held item is now. Only Windows can tell.
    pub fn whereabouts(&self) -> Whereabouts {
        #[cfg(windows)]
        {
            let Some(at) = &self.at else {
                return Whereabouts::Unknown;
            };
            match imp::final_path(&self.handle) {
                Some(now) if now.eq_ignore_ascii_case(at) => Whereabouts::Unmoved,
                Some(now) if starts_with_ignoring_case(&now, r"\$Extend\$Deleted\") => {
                    Whereabouts::Deleted
                }
                Some(now) => Whereabouts::Moved(now),
                None => Whereabouts::Unknown,
            }
        }
        #[cfg(not(windows))]
        Whereabouts::Unknown
    }
}

/// Whether `a` and `b` are the same object on the same drive, each read as
/// [`hold`] reads it: by file ID, or for a drive that gives none, by where
/// Windows says each is.
#[cfg(windows)]
pub fn same_object(a: &Path, b: &Path) -> io::Result<bool> {
    let (found_a, held_a) = hold(a, false)?;
    let (found_b, held_b) = hold(b, false)?;
    if found_a.volume != found_b.volume || found_a.kind != found_b.kind {
        return Ok(false);
    }
    if found_a.ids.iter().any(|&id| found_b.has_id(id)) {
        return Ok(true);
    }
    let unidentified = found_a.ids == [0; 2] && found_b.ids == [0; 2];
    Ok(unidentified
        && held_a
            .at
            .as_deref()
            .zip(held_b.at.as_deref())
            .is_some_and(|(x, y)| x.eq_ignore_ascii_case(y)))
}

/// Whether a path from [`Whereabouts::Moved`] is in its volume's Recycle
/// Bin.
pub fn in_recycle_bin(path: &str) -> bool {
    starts_with_ignoring_case(path, r"\$Recycle.Bin\")
}

fn starts_with_ignoring_case(path: &str, prefix: &str) -> bool {
    path.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_recycle_bin() {
        assert!(in_recycle_bin(r"\$RECYCLE.BIN\S-1-5-21-1\$R1AB2CD.txt"));
        assert!(in_recycle_bin(r"\$Recycle.Bin\S-1-5-21-1\$RX.bin"));
        assert!(!in_recycle_bin(r"\Users\me\$Recycle.Bin\a"));
        assert!(!in_recycle_bin(r"\$Recycle"));
    }

    /// A file deleted outright while held shows where NTFS parks it.
    #[cfg(windows)]
    #[test]
    fn tells_a_moved_item_from_a_deleted_one() {
        let dir = std::env::temp_dir().join(format!("sb-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        let (_, held_a) = hold(&a, false).unwrap();
        let (_, held_b) = hold(&b, false).unwrap();
        assert_eq!(held_a.whereabouts(), Whereabouts::Unmoved);
        std::fs::rename(&a, dir.join("c.txt")).unwrap();
        std::fs::remove_file(&b).unwrap();
        let moved = held_a.whereabouts();
        let deleted = held_b.whereabouts();
        drop((held_a, held_b));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(moved, Whereabouts::Moved(p) if p.ends_with(r"\c.txt")));
        assert!(matches!(
            deleted,
            Whereabouts::Deleted | Whereabouts::Unknown
        ));
    }
}

#[cfg(windows)]
mod imp {
    use super::{Found, Held, Kind, fingerprint};
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use windows::Win32::Foundation::{ERROR_SHARING_VIOLATION, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileAttributeTagInfo, FileIdInfo, GETFINALPATHNAMEBYHANDLE_FLAGS,
        GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
        OPEN_EXISTING, VOLUME_NAME_NONE,
    };
    use windows::core::PCWSTR;

    const DIRECTORY: u32 = 0x10;
    const DEVICE: u32 = 0x40;
    const REPARSE_POINT: u32 = 0x400;

    fn io_error(err: windows::core::Error) -> io::Error {
        let code = err.code().0 as u32;
        if code & 0xFFFF_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::other(err.message())
        }
    }

    /// NUL-terminated wide path in the `\\?\` form, so long paths work and
    /// the path names exactly the item: an absolute path is used as it is,
    /// since making it absolute again (`GetFullPathNameW`) would strip a
    /// trailing dot or space from a name and reach a different item. Only a
    /// relative path is made absolute first.
    fn wide(path: &Path) -> Vec<u16> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
        };
        let text: Vec<u16> = absolute
            .as_os_str()
            .encode_wide()
            .map(|c| {
                if c == u16::from(b'/') {
                    u16::from(b'\\')
                } else {
                    c
                }
            })
            .collect();
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

    /// Opens for attributes only, without following a reparse point or
    /// recalling a cloud file.
    fn open(path: &Path, share: FILE_SHARE_MODE) -> io::Result<OwnedHandle> {
        let path = wide(path);
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                share,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS
                    | FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL,
                None,
            )
        }
        .map_err(io_error)?;
        // SAFETY: CreateFileW returned a valid handle that we now own.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }

    pub fn hold(path: &Path, lock: bool) -> io::Result<(Found, Held)> {
        let all = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let handle = if lock {
            match open(path, FILE_SHARE_READ | FILE_SHARE_WRITE) {
                // Someone has it open to rename or delete it: check it
                // without the lock.
                Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32) => {
                    open(path, all)?
                }
                result => result?,
            }
        } else {
            open(path, all)?
        };
        let found = found(&handle)?;
        let at = final_path(&handle);
        Ok((found, Held { handle, at }))
    }

    fn raw(handle: &OwnedHandle) -> HANDLE {
        HANDLE(handle.as_raw_handle())
    }

    fn info<T: Default>(handle: &OwnedHandle, class: FILE_INFO_BY_HANDLE_CLASS) -> Option<T> {
        let mut value = T::default();
        // SAFETY: `value` is a `T`, the type `class` fills, of the size given.
        unsafe {
            GetFileInformationByHandleEx(
                raw(handle),
                class,
                (&raw mut value).cast::<c_void>(),
                size_of::<T>() as u32,
            )
        }
        .ok()?;
        Some(value)
    }

    fn found(handle: &OwnedHandle) -> io::Result<Found> {
        let mut by_handle = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: a valid handle and a structure to fill.
        unsafe { GetFileInformationByHandle(raw(handle), &mut by_handle) }.map_err(io_error)?;
        let attributes = by_handle.dwFileAttributes;
        let tag = if attributes & REPARSE_POINT != 0 {
            info::<FILE_ATTRIBUTE_TAG_INFO>(handle, FileAttributeTagInfo)
                .map_or(0, |t| t.ReparseTag)
        } else {
            0
        };
        let index =
            (u64::from(by_handle.nFileIndexHigh) << 32) | u64::from(by_handle.nFileIndexLow);
        let (volume, id) = match info::<FILE_ID_INFO>(handle, FileIdInfo) {
            Some(id) => (
                id.VolumeSerialNumber,
                u128::from_le_bytes(id.FileId.Identifier),
            ),
            None => (u64::from(by_handle.dwVolumeSerialNumber), 0),
        };
        let ticks = (u64::from(by_handle.ftLastWriteTime.dwHighDateTime) << 32)
            | u64::from(by_handle.ftLastWriteTime.dwLowDateTime);
        Ok(Found {
            kind: kind(attributes, tag),
            volume,
            ids: [fingerprint(id), fingerprint(u128::from(index))],
            len: (u64::from(by_handle.nFileSizeHigh) << 32) | u64::from(by_handle.nFileSizeLow),
            modified: filetime(ticks),
        })
    }

    /// As the scanner classifies entries: cloud files are files, any other
    /// folder reparse point and any name surrogate redirect.
    fn kind(attributes: u32, tag: u32) -> Kind {
        let folder = attributes & DIRECTORY != 0;
        if attributes & REPARSE_POINT != 0 {
            let cloud = tag & 0xFFFF_0FFF == 0x9000_001A;
            let surrogate = tag & 0x2000_0000 != 0;
            // AF_UNIX and the WSL FIFO, character and block device tags.
            if (0x8000_0023..=0x8000_0026).contains(&tag) {
                return Kind::Other;
            }
            if !cloud && (folder || surrogate) {
                return Kind::Link;
            }
        }
        if folder {
            Kind::Folder
        } else if attributes & DEVICE != 0 {
            Kind::Other
        } else {
            Kind::File
        }
    }

    /// A `FILETIME` as a time; `None` for 0, which means none was set.
    fn filetime(ticks: u64) -> Option<SystemTime> {
        if ticks == 0 {
            return None;
        }
        let since_1601 = Duration::new(ticks / 10_000_000, (ticks % 10_000_000) as u32 * 100);
        let offset = Duration::from_secs(11_644_473_600);
        match since_1601.checked_sub(offset) {
            Some(after) => UNIX_EPOCH.checked_add(after),
            None => UNIX_EPOCH.checked_sub(offset - since_1601),
        }
    }

    /// The handle's file's path on its volume, without the drive.
    pub fn final_path(handle: &OwnedHandle) -> Option<String> {
        let mut buffer = vec![0u16; 1024];
        loop {
            // SAFETY: a valid handle and a buffer of the given length.
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    raw(handle),
                    &mut buffer,
                    GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_NONE.0),
                )
            } as usize;
            if len == 0 {
                return None;
            }
            if len < buffer.len() {
                return Some(String::from_utf16_lossy(&buffer[..len]));
            }
            buffer.resize(len + 1, 0);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn classifies_like_the_scanner() {
            assert_eq!(kind(0x20, 0), Kind::File);
            assert_eq!(kind(DIRECTORY, 0), Kind::Folder);
            // Junction, symlink, cloud placeholder, WOF-compressed file.
            assert_eq!(kind(DIRECTORY | REPARSE_POINT, 0xA000_0003), Kind::Link);
            assert_eq!(kind(REPARSE_POINT, 0xA000_000C), Kind::Link);
            assert_eq!(kind(DIRECTORY | REPARSE_POINT, 0x9000_101A), Kind::Folder);
            assert_eq!(kind(REPARSE_POINT, 0x8000_0017), Kind::File);
            assert_eq!(kind(REPARSE_POINT, 0x8000_0023), Kind::Other);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Found, Held, Kind, fingerprint};
    use sb_core::tree::{birth_secs, unix_object_id};
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub fn hold(path: &Path, _lock: bool) -> io::Result<(Found, Held)> {
        let meta = std::fs::symlink_metadata(path)?;
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            Kind::Link
        } else if file_type.is_dir() {
            Kind::Folder
        } else if file_type.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        let found = Found {
            kind,
            volume: meta.dev(),
            // With the birth time as the scan recorded it, and without, for
            // a scan made where it wasn't available.
            ids: [
                fingerprint(unix_object_id(meta.ino(), birth_secs(&meta))),
                fingerprint(unix_object_id(meta.ino(), None)),
            ],
            len: meta.len(),
            modified: meta.modified().ok(),
        };
        Ok((found, Held {}))
    }
}

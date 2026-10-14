//! Writing a file in full before it replaces anything.
//!
//! A [`TempFile`] is a new file in the destination's folder, created
//! exclusively under a name like `.<name>.<pid>-<random>.partial` (with at
//! most the first [`NAME_BYTES`] bytes of the name, so a long name still
//! fits the 255-character limit): if anything already has that name, a file
//! or a link (dangling or not), another name is tried, so nothing that was
//! there is truncated or written through.
//!
//! Putting the finished file in place is tied to the file that was written,
//! not to its temporary name, so a process that renames the temporary file
//! away and puts another in its place can't have that one published:
//!
//! - On Windows the file is renamed through the handle it was written with
//!   (`SetFileInformationByHandle`), wherever it has been moved.
//! - Elsewhere no system renames by handle, so the name above is a private
//!   folder (mode 0700) holding the file, and the folder and the
//!   destination's folder are held open: the file is created and renamed
//!   relative to them (`openat`, `renameat`), so swapping either folder
//!   for another changes nothing, and only this user (or an administrator)
//!   can change what's inside the private one. The file is also checked to
//!   be the one written, by device and inode, just before the rename.
//!   File systems without Unix modes or owners of their own (FAT and exFAT
//!   drives, NTFS, SMB shares, NFS mapping users) can't make a private
//!   folder; there the file is made directly in the destination's folder,
//!   and only that check stands between a substitute and the destination,
//!   which narrows the window but can't close it. Outside Linux, a folder
//!   this user may write to but not list can't be held open, so it's used
//!   by path, as before.
//!
//! [`TempFile::persist`] replaces the destination, as a user choosing to
//! overwrite a file asked; renaming replaces a destination that is itself a
//! link with the file, rather than writing where the link points.
//! [`TempFile::persist_new`] never replaces anything, so saves at once can
//! each claim a name of their own.
//!
//! A temporary file that isn't persisted is removed when dropped, only if
//! it's still ours: through the handle on Windows, and elsewhere only while
//! the name in the private folder still refers to the file we created; the
//! folder is then removed if it's still ours. Removing never follows a
//! link. Nothing here stops a process running as the same user, which can
//! change any of these files anyway.

use std::collections::hash_map::RandomState;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// The extension of every temporary file (or, outside Windows, the private
/// folder holding it).
pub const EXTENSION: &str = "partial";

/// Names tried before giving up; a clash at all is unlikely.
const ATTEMPTS: usize = 16;

/// The most of the destination's name a temporary name holds.
const NAME_BYTES: usize = 64;

/// A new file being written, not yet in its place.
#[derive(Debug)]
pub struct TempFile {
    /// The destination's name in its folder, for [`TempFile::persist`].
    name: OsString,
    /// The temporary name in the destination's folder.
    #[cfg(test)]
    path: PathBuf,
    /// Taken when persisted or dropped.
    staged: Option<sys::Staged>,
    /// Given up once persisted.
    file: Option<File>,
}

impl TempFile {
    /// Creates a temporary file in `dir` for a destination called `name`.
    pub fn new_in(dir: &Path, name: &str) -> io::Result<Self> {
        let short = shortened(name);
        let mut last = None;
        for _ in 0..ATTEMPTS {
            let entry = format!(
                ".{short}.{}-{:016x}.{EXTENSION}",
                std::process::id(),
                unique()
            );
            match sys::Staged::create(dir, &entry) {
                Ok((staged, file)) => {
                    return Ok(Self {
                        name: name.into(),
                        #[cfg(test)]
                        path: dir.join(&entry),
                        staged: Some(staged),
                        file: Some(file),
                    });
                }
                Err(e) if taken(&e, &dir.join(&entry)) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no free temporary name")))
    }

    /// A temporary file beside `dest`.
    pub fn beside(dest: &Path) -> io::Result<Self> {
        let name = dest
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file name"))?;
        let dir = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        Self::new_in(dir, &name.to_string_lossy())
    }

    /// The file being written; not to be used once persisted.
    pub fn file(&self) -> &File {
        self.file.as_ref().expect("open until persisted or dropped")
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flushes the file to disk and renames it over the destination named
    /// when it was created. On failure the temporary file is removed and the
    /// destination is as it was.
    pub fn persist(mut self) -> io::Result<()> {
        let name = std::mem::take(&mut self.name);
        self.commit(&name, true)
    }

    /// Flushes the file to disk and renames it to `name`, in the same
    /// folder, only if nothing has that name. When something does, the error
    /// is [`io::ErrorKind::AlreadyExists`] and the file is kept, to try
    /// another name; it's removed when dropped.
    pub fn persist_new(&mut self, name: &str) -> io::Result<()> {
        self.commit(OsStr::new(name), false)
    }

    fn commit(&mut self, name: &OsStr, replace: bool) -> io::Result<()> {
        self.file().sync_all()?;
        let staged = self.staged.as_ref().expect("staged until persisted");
        staged.commit(self.file(), name, replace)?;
        // Persisted: the handle is given up, and what's left of the staging
        // removed.
        self.file = None;
        if let Some(staged) = self.staged.take() {
            staged.close(None);
        }
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(staged) = self.staged.take() {
            staged.close(self.file.take());
        }
    }
}

/// Whether creating `path` failed with `e` because something has that
/// name: Windows refuses a folder or junction there as access denied.
fn taken(e: &io::Error, path: &Path) -> bool {
    e.kind() == io::ErrorKind::AlreadyExists
        || (e.kind() == io::ErrorKind::PermissionDenied && std::fs::symlink_metadata(path).is_ok())
}

#[cfg(windows)]
mod sys {
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_INFO_BY_HANDLE_CLASS, FILE_RENAME_INFO, FileRenameInfo, FileRenameInfoEx,
        SetFileInformationByHandle,
    };

    /// `FILE_RENAME_FLAG_*`, from `winbase.h`.
    const REPLACE_IF_EXISTS: u32 = 0x1;
    const POSIX_SEMANTICS: u32 = 0x2;

    /// Errors meaning `FileRenameInfoEx` isn't supported: by Windows before
    /// 10 1607, FAT, and some network shares.
    const NOT_SUPPORTED: [i32; 3] = [1, 50, 87];

    /// The temporary file's folder, also the destination's.
    #[derive(Debug)]
    pub struct Staged {
        /// Absolute, as the folder was when the file was created.
        dir: PathBuf,
    }

    impl Staged {
        /// Creates the file `entry` in `dir`, only if nothing has that name.
        pub fn create(dir: &Path, entry: &str) -> io::Result<(Self, File)> {
            // Resolved now, so a later change of working folder can't move
            // the destination.
            let dir = std::path::absolute(dir)?;
            let file = create_new(&dir.join(entry))?;
            Ok((Self { dir }, file))
        }

        /// Renames `file` itself to `name` in the folder.
        pub fn commit(&self, file: &File, name: &OsStr, replace: bool) -> io::Result<()> {
            rename_through(file, &self.dir.join(name), replace, true)
        }

        /// Removes `file`, if it wasn't persisted.
        pub fn close(self, file: Option<File>) {
            if let Some(file) = file {
                remove_through(file);
            }
        }
    }

    /// Opens `path` for writing only if nothing has that name, never
    /// following a link there.
    pub(super) fn create_new(path: &Path) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
            FILE_SHARE_READ,
        };
        let mut options = OpenOptions::new();
        // `create_new` is CREATE_NEW, which fails on any existing name.
        // A link at the name, even one to nothing, is opened as itself (and
        // so refused), never followed to create its target. std adds this
        // flag itself with `create_new`; it's repeated here so that doesn't
        // rest on std's internals. DELETE lets the file be renamed or
        // removed through this handle. No one else may write to it while
        // it's open; reading and renaming are still shared.
        options
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .access_mode(FILE_GENERIC_WRITE.0 | DELETE.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0);
        options.open(path)
    }

    /// Renames the file `file` was opened on to `dest`, wherever it is now.
    /// With POSIX semantics (`ex`) a destination open elsewhere can be
    /// replaced too; where they aren't supported, the older call is used.
    pub(super) fn rename_through(
        file: &File,
        dest: &Path,
        replace: bool,
        ex: bool,
    ) -> io::Result<()> {
        let name = wide(dest);
        let name_bytes = name.len() * size_of::<u16>();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        // The name follows the fixed fields, with room for a terminator.
        let size = (offset + name_bytes + size_of::<u16>()).max(size_of::<FILE_RENAME_INFO>());
        let mut buf = vec![0u64; size.div_ceil(size_of::<u64>())];
        let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: `buf` is zeroed, aligned for the struct (8 bytes), and at
        // least `size` bytes, which holds the fixed fields and the name
        // after them, so every write is in bounds.
        unsafe {
            (*info).FileNameLength = name_bytes as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
                name.len(),
            );
        }
        let set = move |class: FILE_INFO_BY_HANDLE_CLASS| {
            // SAFETY: the handle is open for the call, and `info` is a
            // FILE_RENAME_INFO of `size` bytes with its name in place.
            unsafe {
                SetFileInformationByHandle(
                    HANDLE(file.as_raw_handle()),
                    class,
                    info.cast_const().cast(),
                    size as u32,
                )
            }
            .map_err(io_error)
        };
        if ex {
            let flags = POSIX_SEMANTICS | if replace { REPLACE_IF_EXISTS } else { 0 };
            // SAFETY: as above; `Flags` spans the union.
            unsafe { (*info).Anonymous.Flags = flags };
            match set(FileRenameInfoEx) {
                Err(e) if e.raw_os_error().is_some_and(|c| NOT_SUPPORTED.contains(&c)) => {}
                done => return done,
            }
        }
        // SAFETY: as above; clearing `Flags` first leaves only the flag.
        unsafe {
            (*info).Anonymous.Flags = 0;
            (*info).Anonymous.ReplaceIfExists = replace;
        }
        set(FileRenameInfo)
    }

    /// Removes the file `file` was opened on, and nothing else.
    fn remove_through(file: File) {
        use windows::Win32::Storage::FileSystem::{FILE_DISPOSITION_INFO, FileDispositionInfo};
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the handle is open for the call, and `info` is the struct
        // the class names, with its size.
        let _ = unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                std::ptr::from_ref(&info).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        // Deleted as the handle closes.
        drop(file);
    }

    fn io_error(err: windows::core::Error) -> io::Error {
        let code = err.code().0 as u32;
        if code & 0xFFFF_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::other(err.message())
        }
    }

    /// The absolute `path` as a wide path in the `\\?\` form (no
    /// terminator), so long paths work.
    fn wide(path: &Path) -> Vec<u16> {
        let text: Vec<u16> = path
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
        if starts(r"\\?\") || starts(r"\\.\") {
            text
        } else if starts(r"\\") {
            r"\\?\UNC\"
                .encode_utf16()
                .chain(text[2..].iter().copied())
                .collect()
        } else {
            r"\\?\".encode_utf16().chain(text).collect()
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
    use rustix::fs::{AtFlags, CWD, Mode, OFlags, Stat};
    use rustix::io::Errno;
    use std::borrow::Cow;
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::path::{Path, PathBuf};

    /// The file's name in its private folder.
    const FILE: &str = "data";

    /// The destination's folder: held open, or by its path where it can't
    /// be opened (outside Linux, a folder this user may write to but not
    /// list).
    #[derive(Debug)]
    enum Dir {
        Open(OwnedFd),
        Path(PathBuf),
    }

    impl Dir {
        fn open(path: &Path) -> io::Result<Self> {
            // O_PATH needs no permission on the folder itself.
            #[cfg(target_os = "linux")]
            let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
            #[cfg(not(target_os = "linux"))]
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            match rustix::fs::open(path, flags, Mode::empty()) {
                Ok(fd) => Ok(Self::Open(fd)),
                Err(Errno::ACCESS) => Ok(Self::Path(path.into())),
                Err(e) => Err(e.into()),
            }
        }

        /// `name` in the folder, for the `*at` calls.
        fn at<'a>(&'a self, name: &'a Path) -> (BorrowedFd<'a>, Cow<'a, Path>) {
            match self {
                Self::Open(fd) => (fd.as_fd(), Cow::Borrowed(name)),
                Self::Path(dir) => (CWD, Cow::Owned(dir.join(name))),
            }
        }
    }

    /// Where the file is.
    #[derive(Debug)]
    enum Holder {
        /// In the private folder, held open; `id` is what that folder is,
        /// to remove only it.
        Private { stage: OwnedFd, id: (u64, u64) },
        /// Directly in the destination's folder, where the file system
        /// can't make a private folder.
        Plain,
    }

    /// The temporary file, and the destination's folder.
    #[derive(Debug)]
    pub struct Staged {
        dir: Dir,
        /// The temporary name in `dir`.
        entry: PathBuf,
        holder: Holder,
        /// What the file is, to publish or remove only it.
        file_id: (u64, u64),
    }

    impl Staged {
        /// Makes the private folder `entry` in `dir`, only if nothing has
        /// that name, and the file in it. Where the folder can't be made
        /// private, the file is `entry` itself.
        pub fn create(dir: &Path, entry: &str) -> io::Result<(Self, File)> {
            let dir = Dir::open(dir)?;
            let entry = PathBuf::from(entry);
            let (holder, file) = {
                let (at, path) = dir.at(&entry);
                // Fails on any existing name, links included.
                rustix::fs::mkdirat(at, &*path, Mode::RWXU)?;
                match private(at, &path) {
                    Ok(Some(made)) => made,
                    Ok(None) => (Holder::Plain, create_file(at, &path)?),
                    Err(e) => {
                        // Only ever removes an empty folder.
                        let _ = rustix::fs::unlinkat(at, &*path, AtFlags::REMOVEDIR);
                        return Err(e);
                    }
                }
            };
            let file_id = id(&rustix::fs::fstat(&file)?);
            let staged = Self {
                dir,
                entry,
                holder,
                file_id,
            };
            Ok((staged, file))
        }

        /// Renames the file, checked to be still the one written, to `name`
        /// in the destination's folder.
        pub fn commit(&self, _file: &File, name: &OsStr, replace: bool) -> io::Result<()> {
            let (from_at, from) = self.file_at();
            let there = rustix::fs::statat(from_at, &*from, AtFlags::SYMLINK_NOFOLLOW)?;
            if id(&there) != self.file_id {
                return Err(io::Error::other("the temporary file was replaced"));
            }
            let name = Path::new(name);
            let (to_at, to) = self.dir.at(name);
            if replace {
                rustix::fs::renameat(from_at, &*from, to_at, &*to)?;
                return Ok(());
            }
            #[cfg(any(target_os = "linux", target_vendor = "apple"))]
            {
                use rustix::fs::RenameFlags;
                match rustix::fs::renameat_with(
                    from_at,
                    &*from,
                    to_at,
                    &*to,
                    RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => return Ok(()),
                    // Not supported by this file system: linked instead.
                    Err(e)
                        if [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP]
                            .contains(&e) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            // A new link fails on any existing name, as renaming would. If
            // the temporary name can't be removed now, `close` tries again.
            rustix::fs::linkat(from_at, &*from, to_at, &*to, AtFlags::empty())?;
            let _ = rustix::fs::unlinkat(from_at, &*from, AtFlags::empty());
            Ok(())
        }

        /// Removes the temporary name if it's still the file: all of it if
        /// it wasn't persisted, or a second name a commit by linking left.
        /// Then the private folder, if it's still ours.
        pub fn close(self, _file: Option<File>) {
            let (at, path) = self.file_at();
            let ours = rustix::fs::statat(at, &*path, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|st| id(&st) == self.file_id);
            if ours {
                let _ = rustix::fs::unlinkat(at, &*path, AtFlags::empty());
            }
            if let Holder::Private { id: stage_id, .. } = &self.holder {
                let (at, path) = self.dir.at(&self.entry);
                let ours = rustix::fs::statat(at, &*path, AtFlags::SYMLINK_NOFOLLOW)
                    .is_ok_and(|st| id(&st) == *stage_id);
                if ours {
                    // Only ever removes an empty folder.
                    let _ = rustix::fs::unlinkat(at, &*path, AtFlags::REMOVEDIR);
                }
            }
        }

        /// Where the file is, for the `*at` calls.
        fn file_at(&self) -> (BorrowedFd<'_>, Cow<'_, Path>) {
            match &self.holder {
                Holder::Private { stage, .. } => (stage.as_fd(), Cow::Borrowed(Path::new(FILE))),
                Holder::Plain => self.dir.at(&self.entry),
            }
        }
    }

    /// Opens the folder just made at `path` and the file in it, if the
    /// folder is private. File systems without Unix modes or owners of their
    /// own (FAT, NTFS, SMB, NFS mapping users) can't make one, or another
    /// user may have swapped in a folder of theirs; then the folder is
    /// removed if it's ours, and it's `None`.
    fn private(at: BorrowedFd<'_>, path: &Path) -> io::Result<Option<(Holder, File)>> {
        let stage = rustix::fs::openat(
            at,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let st = rustix::fs::fstat(&stage)?;
        let private = st.st_uid == rustix::process::geteuid().as_raw()
            && st.st_mode & 0o077 == 0
            && !super::not_private();
        if !private {
            let ours = rustix::fs::statat(at, path, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|there| id(&there) == id(&st));
            if ours {
                let _ = rustix::fs::unlinkat(at, path, AtFlags::REMOVEDIR);
            }
            return Ok(None);
        }
        let file = create_file(stage.as_fd(), Path::new(FILE))?;
        Ok(Some((Holder::Private { stage, id: id(&st) }, file)))
    }

    /// Creates the file `path` for writing, only if nothing has that name.
    fn create_file(at: BorrowedFd<'_>, path: &Path) -> io::Result<File> {
        let fd = rustix::fs::openat(
            at,
            path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        )?;
        Ok(File::from(fd))
    }

    /// Device and inode.
    #[allow(clippy::unnecessary_cast)]
    fn id(st: &Stat) -> (u64, u64) {
        // Their types differ between systems.
        (st.st_dev as u64, st.st_ino as u64)
    }
}

/// Whether to act as if a private folder couldn't be made, as on file
/// systems without Unix modes; set in tests, for this thread only.
#[cfg(all(not(windows), test))]
fn not_private() -> bool {
    tests::NOT_PRIVATE.get()
}

#[cfg(all(not(windows), not(test)))]
fn not_private() -> bool {
    false
}

/// The start of `name`, at most [`NAME_BYTES`] bytes, cut between
/// characters.
fn shortened(name: &str) -> &str {
    let mut end = name.len().min(NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// A number no other temporary name in this process has had, and that
/// other processes are unlikely to pick.
fn unique() -> u64 {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    // Randomly keyed per process.
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNT.fetch_add(1, Ordering::Relaxed));
    if let Ok(t) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        h.write_u128(t.as_nanos());
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("sb-atomic-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn replaces_the_destination_only_when_persisted() {
        let tmp = temp("persist");
        let dest = tmp.0.join("out.csv");
        fs::write(&dest, "old").unwrap();
        let t = TempFile::beside(&dest).unwrap();
        assert_eq!(t.path().extension().unwrap(), EXTENSION);
        let name = t.path().file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".out.csv."), "{name}");
        t.file().write_all(b"new").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
        t.persist().unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
        assert_eq!(names(&tmp.0), ["out.csv"]);
    }

    #[test]
    fn long_names_are_shortened() {
        let tmp = temp("long");
        // 229 characters, near the limit, with ones of several bytes.
        let name = format!("{}{}.csv", "a".repeat(200), "\u{e9}".repeat(25));
        assert_eq!(name.chars().count(), 229);
        let dest = tmp.0.join(&name);
        let t = TempFile::beside(&dest).unwrap();
        let temp_name = t.path().file_name().unwrap().to_str().unwrap().to_owned();
        assert!(temp_name.starts_with(&format!(".{}.", "a".repeat(64))));
        t.file().write_all(b"long").unwrap();
        t.persist().unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "long");
        assert_eq!(names(&tmp.0), [name]);
        // Never cut inside a character.
        let wide = "\u{e9}".repeat(40);
        assert_eq!(shortened(&wide), "\u{e9}".repeat(32));
        assert_eq!(
            shortened(&format!("a{wide}")),
            format!("a{}", "\u{e9}".repeat(31))
        );
        assert_eq!(shortened("short"), "short");
    }

    #[test]
    fn a_dropped_file_removes_only_itself() {
        let tmp = temp("drop");
        let dest = tmp.0.join("out.csv");
        fs::write(&dest, "old").unwrap();
        // What the old fixed name was: someone else's file.
        fs::write(tmp.0.join(".out.csv.partial"), "theirs").unwrap();
        let a = TempFile::beside(&dest).unwrap();
        let b = TempFile::beside(&dest).unwrap();
        assert_ne!(a.path(), b.path());
        a.file().write_all(b"a").unwrap();
        drop(a);
        assert_eq!(names(&tmp.0).len(), 3, "b's is still there");
        drop(b);
        assert_eq!(names(&tmp.0), [".out.csv.partial", "out.csv"]);
        assert_eq!(
            fs::read_to_string(tmp.0.join(".out.csv.partial")).unwrap(),
            "theirs"
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
    }

    #[test]
    fn a_failed_rename_removes_the_temporary_file() {
        let tmp = temp("fail");
        // A folder can't be replaced by a file.
        let dest = tmp.0.join("taken");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("inside"), "x").unwrap();
        let t = TempFile::beside(&dest).unwrap();
        t.file().write_all(b"data").unwrap();
        assert!(t.persist().is_err());
        assert_eq!(names(&tmp.0), ["taken"]);
        assert_eq!(names(&dest), ["inside"]);
    }

    /// Where the file being written is: its temporary name on Windows, and
    /// inside the private folder elsewhere.
    fn staged_file(t: &TempFile) -> PathBuf {
        if cfg!(windows) {
            t.path().to_owned()
        } else {
            t.path().join("data")
        }
    }

    #[test]
    fn a_swapped_name_isnt_removed() {
        let tmp = temp("swapped");
        let t = TempFile::new_in(&tmp.0, "x").unwrap();
        let path = staged_file(&t);
        fs::rename(&path, tmp.0.join("moved")).unwrap();
        fs::write(&path, "someone else's").unwrap();
        drop(t);
        assert_eq!(fs::read_to_string(&path).unwrap(), "someone else's");
    }

    /// The review's probe: the temporary file renamed away and another put
    /// in its place must never be what's published.
    #[test]
    fn a_swapped_temporary_file_is_never_published() {
        let tmp = temp("substitute");
        let dest = tmp.0.join("out.csv");
        fs::write(&dest, "old").unwrap();
        let t = TempFile::beside(&dest).unwrap();
        t.file().write_all(b"ours").unwrap();
        let path = staged_file(&t);
        let moved = tmp.0.join("moved");
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, "substitute").unwrap();
        let persisted = t.persist();
        let now = fs::read_to_string(&dest).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "substitute");
        if cfg!(windows) {
            // Renamed through the handle, from wherever it was moved.
            persisted.unwrap();
            assert_eq!(now, "ours");
            assert!(!moved.exists());
        } else {
            // Refused: the name in the private folder isn't ours any more.
            assert!(persisted.is_err());
            assert_eq!(now, "old");
        }
    }

    /// Outside Windows, the private folder itself swapped for another is
    /// never used: everything goes through the folder as opened.
    #[cfg(not(windows))]
    #[test]
    fn a_swapped_private_folder_is_never_used() {
        let tmp = temp("swapped-folder");
        let dest = tmp.0.join("out.csv");
        let t = TempFile::beside(&dest).unwrap();
        t.file().write_all(b"ours").unwrap();
        let folder = t.path().to_owned();
        let moved = tmp.0.join("moved");
        fs::rename(&folder, &moved).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("data"), "substitute").unwrap();
        t.persist().unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "ours");
        assert_eq!(
            fs::read_to_string(folder.join("data")).unwrap(),
            "substitute"
        );
        assert_eq!(names(&moved).len(), 0, "ours, emptied");
    }

    #[test]
    fn persist_new_never_replaces() {
        let tmp = temp("new");
        let taken = tmp.0.join("a.txt");
        fs::write(&taken, "theirs").unwrap();
        let mut t = TempFile::new_in(&tmp.0, "a.txt").unwrap();
        t.file().write_all(b"ours").unwrap();
        let err = t.persist_new("a.txt").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err:?}");
        assert_eq!(fs::read_to_string(&taken).unwrap(), "theirs");
        // Kept for another name.
        t.persist_new("b.txt").unwrap();
        assert_eq!(fs::read_to_string(tmp.0.join("b.txt")).unwrap(), "ours");
        drop(t);
        assert_eq!(names(&tmp.0), ["a.txt", "b.txt"]);

        // Dropped without a name of its own: nothing is left.
        let mut t = TempFile::new_in(&tmp.0, "a.txt").unwrap();
        assert!(t.persist_new("b.txt").is_err());
        drop(t);
        assert_eq!(names(&tmp.0), ["a.txt", "b.txt"]);
        assert_eq!(fs::read_to_string(&taken).unwrap(), "theirs");
    }

    /// The older rename Windows falls back to (FAT, some network shares)
    /// also replaces or not as asked.
    #[cfg(windows)]
    #[test]
    fn the_fallback_rename_works_too() {
        let tmp = temp("fallback");
        let dest = tmp.0.join("out.txt");
        fs::write(&dest, "old").unwrap();
        for (replace, text) in [(false, "no"), (true, "yes")] {
            let mut t = TempFile::beside(&dest).unwrap();
            t.file().write_all(text.as_bytes()).unwrap();
            let done = sys::rename_through(t.file(), &dest, replace, false);
            if replace {
                done.unwrap();
                assert_eq!(fs::read_to_string(&dest).unwrap(), "yes");
                // Already in place: dropping must not remove it.
                t.file = None;
            } else {
                assert_eq!(done.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
                assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
            }
        }
        assert_eq!(names(&tmp.0), ["out.txt"]);
    }

    /// Makes a link at `link` to `target`, if this system allows it
    /// (Windows needs a privilege or Developer Mode).
    fn symlink(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(target, link);
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(target, link);
        match made {
            Ok(()) => true,
            Err(e) => {
                eprintln!("SKIPPED the symbolic link checks: can't make one here ({e})");
                false
            }
        }
    }

    #[test]
    fn existing_names_and_links_are_never_written_through() {
        let tmp = temp("links");
        let target = tmp.0.join("target.txt");
        fs::write(&target, "keep").unwrap();
        let create = |name: &str| sys::Staged::create(&tmp.0, name).map(|_| ());
        // Each kind of name already there is refused, not opened.
        assert_eq!(
            create("target.txt").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");

        let link = tmp.0.join("link");
        if !symlink(&target, &link) {
            return;
        }
        assert_eq!(
            create("link").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let dangling = tmp.0.join("dangling");
        let nowhere = tmp.0.join("nowhere.txt");
        assert!(symlink(&nowhere, &dangling));
        assert_eq!(
            create("dangling").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!nowhere.exists(), "a dangling link's target isn't created");
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");

        // A destination that is a link is replaced, not written through.
        let t = TempFile::beside(&link).unwrap();
        t.file().write_all(b"new").unwrap();
        t.persist().unwrap();
        assert!(!fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&link).unwrap(), "new");
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }

    /// Junctions need no privilege, so this always runs on Windows.
    #[cfg(windows)]
    #[test]
    fn junctions_are_never_followed() {
        let tmp = temp("junctions");
        let junction = |link: &Path, target: &Path| {
            let made = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(made.status.success(), "{made:?}");
        };
        let folder = tmp.0.join("folder");
        fs::create_dir(&folder).unwrap();
        let to_folder = tmp.0.join("to-folder");
        junction(&to_folder, &folder);
        // Refused as a folder (access denied) rather than as a name taken,
        // which `new_in` also takes as a name to skip.
        let refused = sys::create_new(&to_folder).unwrap_err();
        assert!(taken(&refused, &to_folder), "{refused:?}");
        assert_eq!(names(&folder).len(), 0);

        // One to a folder that's gone: its target isn't created either.
        let gone = tmp.0.join("gone");
        fs::create_dir(&gone).unwrap();
        let to_gone = tmp.0.join("to-gone");
        junction(&to_gone, &gone);
        fs::remove_dir(&gone).unwrap();
        let refused = sys::create_new(&to_gone).unwrap_err();
        assert!(taken(&refused, &to_gone), "{refused:?}");
        assert!(!gone.exists(), "a dangling junction's target isn't created");
        assert!(fs::symlink_metadata(&to_gone).is_ok(), "the junction stays");
    }

    #[test]
    fn concurrent_writers_each_finish() {
        let tmp = temp("concurrent");
        let dest = tmp.0.join("out.txt");
        let contents: Vec<String> = (0..8).map(|i| format!("{i}").repeat(100_000)).collect();
        std::thread::scope(|s| {
            for text in &contents {
                let dest = &dest;
                s.spawn(move || {
                    // Windows refuses to replace a file another process
                    // holds without delete sharing (a sharing violation),
                    // which a virus scanner or the indexer may briefly do to
                    // a file just written: the product reports that, and
                    // here it's tried again.
                    for attempt in 1.. {
                        let t = TempFile::beside(dest).unwrap();
                        let mut w = io::BufWriter::new(t.file());
                        for chunk in text.as_bytes().chunks(4096) {
                            w.write_all(chunk).unwrap();
                        }
                        w.flush().unwrap();
                        drop(w);
                        match t.persist() {
                            Ok(()) => break,
                            Err(e)
                                if cfg!(windows)
                                    && e.raw_os_error() == Some(32)
                                    && attempt < 20 =>
                            {
                                eprintln!("retrying a rename refused while held: {e}");
                                std::thread::sleep(std::time::Duration::from_millis(50));
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                });
            }
        });
        let result = fs::read_to_string(&dest).unwrap();
        assert!(contents.contains(&result), "one writer's whole file");
        assert_eq!(names(&tmp.0), ["out.txt"]);
    }

    thread_local! {
        /// Makes this thread's temporary files act as if a private folder
        /// couldn't be made.
        pub(super) static NOT_PRIVATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Where a private folder can't be made (FAT, NTFS, SMB, NFS mapping
    /// users), the file is made directly in the destination's folder, and
    /// still published only if it's the file written, and only where asked.
    #[cfg(not(windows))]
    #[test]
    fn without_a_private_folder_the_file_is_plain() {
        let tmp = temp("plain");
        NOT_PRIVATE.set(true);
        let dest = tmp.0.join("out.csv");
        fs::write(&dest, "old").unwrap();

        let t = TempFile::beside(&dest).unwrap();
        assert!(fs::symlink_metadata(t.path()).unwrap().is_file());
        assert_eq!(names(&tmp.0).len(), 2);
        t.file().write_all(b"new").unwrap();
        t.persist().unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
        assert_eq!(names(&tmp.0), ["out.csv"]);

        // Never replacing, and kept for another name.
        let mut t = TempFile::beside(&dest).unwrap();
        t.file().write_all(b"other").unwrap();
        let err = t.persist_new("out.csv").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err:?}");
        t.persist_new("second.csv").unwrap();
        drop(t);
        assert_eq!(names(&tmp.0), ["out.csv", "second.csv"]);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");

        // A substitute at the temporary name isn't published or removed.
        let t = TempFile::beside(&dest).unwrap();
        t.file().write_all(b"ours").unwrap();
        let path = t.path().to_owned();
        fs::rename(&path, tmp.0.join("moved")).unwrap();
        fs::write(&path, "substitute").unwrap();
        assert!(t.persist().is_err());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new");
        assert_eq!(fs::read_to_string(&path).unwrap(), "substitute");

        // Dropped: removed.
        let t = TempFile::beside(&dest).unwrap();
        let path = t.path().to_owned();
        drop(t);
        assert!(!path.exists());
        NOT_PRIVATE.set(false);
    }

    /// A folder this user may write to but not list still takes exports, as
    /// it did before.
    #[cfg(not(windows))]
    #[test]
    fn a_write_only_folder_still_works() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = temp("write-only");
        let drop_box = tmp.0.join("box");
        fs::create_dir(&drop_box).unwrap();
        fs::set_permissions(&drop_box, fs::Permissions::from_mode(0o333)).unwrap();
        let dest = drop_box.join("out.csv");
        let result = (|| -> io::Result<()> {
            let t = TempFile::beside(&dest)?;
            t.file().write_all(b"dropped")?;
            t.persist()?;
            let mut u = TempFile::beside(&dest)?;
            u.file().write_all(b"second")?;
            u.persist_new("second.csv")
        })();
        fs::set_permissions(&drop_box, fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "dropped");
        assert_eq!(names(&drop_box), ["out.csv", "second.csv"]);
    }

    /// Nothing else can write into a temporary file while it's open.
    #[cfg(windows)]
    #[test]
    fn no_one_else_writes_to_a_temporary_file() {
        let tmp = temp("share");
        let t = TempFile::new_in(&tmp.0, "x").unwrap();
        let err = fs::OpenOptions::new()
            .write(true)
            .open(t.path())
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(32), "{err:?}");
        // Reading is still shared.
        fs::read(t.path()).unwrap();
    }
}

//! Writing a file in full before it replaces anything.
//!
//! A [`TempFile`] is a new file beside the destination, named
//! `.<name>.<pid>-<random>.partial` (with at most the first
//! [`NAME_BYTES`] bytes of the name, so a long name still fits the
//! 255-character limit) and created exclusively: if anything
//! already has that name, a file or a link (dangling or not), another name
//! is tried, so nothing that was there is truncated or written through.
//! The handle is kept from creation to the end, and [`TempFile::persist`]
//! renames the file over the destination. Renaming replaces a destination
//! that is itself a link with the file, rather than writing where the link
//! points, as renaming does everywhere.
//!
//! A temporary file that isn't persisted is removed when dropped, only if
//! it's still ours: through the handle on Windows, and on other systems
//! only while the name still refers to the file we created. Removing never
//! follows a link.

use std::collections::hash_map::RandomState;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// The extension of every temporary file.
pub const EXTENSION: &str = "partial";

/// Names tried before giving up; a clash at all is unlikely.
const ATTEMPTS: usize = 16;

/// The most of the destination's name a temporary name holds.
const NAME_BYTES: usize = 64;

/// A new file being written, not yet in its place.
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
    file: Option<File>,
}

impl TempFile {
    /// Creates a temporary file in `dir` for a destination called `name`.
    pub fn new_in(dir: &Path, name: &str) -> io::Result<Self> {
        let name = shortened(name);
        let mut last = None;
        for _ in 0..ATTEMPTS {
            let path = dir.join(format!(
                ".{name}.{}-{:016x}.{EXTENSION}",
                std::process::id(),
                unique()
            ));
            match create_new(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                    });
                }
                Err(e) if taken(&e, &path) => last = Some(e),
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

    pub fn file(&self) -> &File {
        self.file.as_ref().expect("open until persisted or dropped")
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flushes the file to disk and renames it over `dest`. On failure the
    /// temporary file is removed and `dest` is as it was.
    pub fn persist(mut self, dest: &Path) -> io::Result<()> {
        self.file().sync_all()?;
        // The handle stays open through the rename (Windows allows it, as
        // the file is shared for deleting), so a failed one can still
        // remove exactly this file.
        fs::rename(&self.path, dest)?;
        self.file = None;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Persisted files have given up their handle.
        if let Some(file) = self.file.take() {
            remove_through(&self.path, file);
        }
    }
}

/// Whether creating `path` failed with `e` because something has that
/// name: Windows refuses a folder or junction there as access denied.
fn taken(e: &io::Error, path: &Path) -> bool {
    e.kind() == io::ErrorKind::AlreadyExists
        || (e.kind() == io::ErrorKind::PermissionDenied && fs::symlink_metadata(path).is_ok())
}

/// Opens `path` for writing only if nothing has that name, never following
/// a link there.
fn create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    // `create_new` is O_CREAT | O_EXCL, which fails on any existing name,
    // links included, or CREATE_NEW on Windows.
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE,
        };
        // A link at the name, even one to nothing, is opened as itself
        // (and so refused), never followed to create its target. std adds
        // this flag itself with `create_new`; it's repeated here so that
        // doesn't rest on std's internals. DELETE lets a discarded file be
        // removed through this handle.
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .access_mode(FILE_GENERIC_WRITE.0 | DELETE.0);
    }
    options.open(path)
}

/// Removes the file `file` was opened on, and nothing else.
#[cfg(windows)]
fn remove_through(_path: &Path, file: File) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle is open for the call, and `info` is the struct the
    // class names, with its size.
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

/// Removes `path` if it's still the file `file` was opened on.
#[cfg(not(windows))]
fn remove_through(path: &Path, file: File) {
    use std::os::unix::fs::MetadataExt;
    let (Ok(ours), Ok(there)) = (file.metadata(), fs::symlink_metadata(path)) else {
        return;
    };
    if ours.dev() == there.dev() && ours.ino() == there.ino() {
        let _ = fs::remove_file(path);
    }
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
        t.persist(&dest).unwrap();
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
        t.persist(&dest).unwrap();
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
        assert!(t.persist(&dest).is_err());
        assert_eq!(names(&tmp.0), ["taken"]);
        assert_eq!(names(&dest), ["inside"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_swapped_name_isnt_removed() {
        let tmp = temp("swapped");
        let t = TempFile::new_in(&tmp.0, "x").unwrap();
        let path = t.path().to_owned();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "someone else's").unwrap();
        drop(t);
        assert_eq!(fs::read_to_string(&path).unwrap(), "someone else's");
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
        // Each kind of name already there is refused, not opened.
        assert_eq!(
            create_new(&target).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");

        let link = tmp.0.join("link");
        if !symlink(&target, &link) {
            return;
        }
        assert_eq!(
            create_new(&link).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let dangling = tmp.0.join("dangling");
        let nowhere = tmp.0.join("nowhere.txt");
        assert!(symlink(&nowhere, &dangling));
        assert_eq!(
            create_new(&dangling).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!nowhere.exists(), "a dangling link's target isn't created");
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");

        // A destination that is a link is replaced, not written through.
        let t = TempFile::beside(&link).unwrap();
        t.file().write_all(b"new").unwrap();
        t.persist(&link).unwrap();
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
        let refused = create_new(&to_folder).unwrap_err();
        assert!(taken(&refused, &to_folder), "{refused:?}");
        assert_eq!(names(&folder).len(), 0);

        // One to a folder that's gone: its target isn't created either.
        let gone = tmp.0.join("gone");
        fs::create_dir(&gone).unwrap();
        let to_gone = tmp.0.join("to-gone");
        junction(&to_gone, &gone);
        fs::remove_dir(&gone).unwrap();
        let refused = create_new(&to_gone).unwrap_err();
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
                    let t = TempFile::beside(dest).unwrap();
                    let mut w = io::BufWriter::new(t.file());
                    for chunk in text.as_bytes().chunks(4096) {
                        w.write_all(chunk).unwrap();
                    }
                    w.flush().unwrap();
                    drop(w);
                    t.persist(dest).unwrap();
                });
            }
        });
        let result = fs::read_to_string(&dest).unwrap();
        assert!(contents.contains(&result), "one writer's whole file");
        assert_eq!(names(&tmp.0), ["out.txt"]);
    }
}

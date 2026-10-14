//! Moving scanned items to the Recycle Bin or Trash, one at a time so each
//! gets its own outcome. Nothing is ever deleted permanently.
//!
//! Just before each move, the item and every folder above it up to the
//! scan root are checked against the scan: the same file ID (inode number
//! on Linux and macOS) on the same drive, the same kind and not a link, and
//! for a file the same size and modified time. Anything else is left alone
//! as changed. With Windows those folders are kept open so they can't be
//! renamed or replaced during the move, and the item is kept open so that
//! afterwards it can be confirmed that it, and not something put in its
//! place, is what reached the Recycle Bin. The move itself goes by path, so
//! an item swapped in at that instant could go instead; it would be in the
//! Recycle Bin, and the outcome says so.

use crate::identity::{self, Found, Kind, Whereabouts};
use sb_core::tree::epoch_seconds;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// What the scan recorded for an item or a folder above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned {
    pub path: PathBuf,
    pub folder: bool,
    /// The fingerprints it was recorded under, 0 for none (see
    /// [`Found::ids`]).
    pub ids: [u64; 2],
    /// A file's size.
    pub len: u64,
    /// A file's modified time, in [`epoch_seconds`] form.
    pub modified: Option<u32>,
}

/// One item to delete, with the folders above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// From the scan root down to the item itself.
    pub chain: Vec<Scanned>,
    /// The drive the scan root was on, when known.
    pub volume: Option<u64>,
}

impl Target {
    pub fn path(&self) -> &Path {
        self.chain.last().map_or(Path::new(""), |s| &s.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Moved to the Recycle Bin or Trash.
    Recycled,
    /// Nothing was there any more; nothing was moved.
    AlreadyMissing,
    /// Not the item the scan found, so it was left alone.
    Changed(String),
    /// Still where it was.
    Failed(String),
    /// Gone from where it was, but not confirmed in the Recycle Bin or
    /// Trash.
    Uncertain(String),
}

/// Why a move to the Recycle Bin or Trash didn't go as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
// Only the Windows shell refuses or deletes without a copy.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum RecycleError {
    /// Left where it was on purpose, since it couldn't be recycled.
    Refused(String),
    /// Windows deleted something without keeping a copy.
    Unrecycled,
    Failed(String),
}

impl RecycleError {
    fn message(&self) -> String {
        match self {
            Self::Refused(m) | Self::Failed(m) => m.clone(),
            Self::Unrecycled => UNRECYCLED.into(),
        }
    }
}

pub const UNRECYCLED: &str = "Windows deleted it without keeping a copy in the Recycle Bin.";

const BIN: &str = if cfg!(windows) {
    "Recycle Bin"
} else {
    "Trash"
};

/// Where deleted items go: the Recycle Bin or Trash, or a stand-in so the
/// checks around a move can be tested without filling the trash.
pub trait Bin {
    fn recycle(&self, path: &Path) -> Result<(), RecycleError>;

    /// Whether an item [`Whereabouts::Moved`] to `path` is in the bin.
    fn holds(&self, path: &str) -> bool;
}

/// The Recycle Bin or Trash.
pub struct SystemBin;

impl Bin for SystemBin {
    fn recycle(&self, path: &Path) -> Result<(), RecycleError> {
        recycle(path)
    }

    fn holds(&self, path: &str) -> bool {
        identity::in_recycle_bin(path)
    }
}

/// Moves the item to the Recycle Bin or Trash if it is still the one the
/// scan found.
pub fn delete(target: &Target) -> Outcome {
    delete_with(target, &SystemBin)
}

/// [`delete`] into the given bin.
pub fn delete_with(target: &Target, bin: &impl Bin) -> Outcome {
    let Some((item, above)) = target.chain.split_last() else {
        return Outcome::Failed("Nothing to delete.".into());
    };
    let mut folders = Vec::with_capacity(above.len());
    for folder in above {
        match identity::hold(&folder.path, true) {
            Ok((found, held)) => {
                if let Some(why) = changed(folder, &found, target.volume) {
                    return Outcome::Changed(format!(
                        "The folder {} above it {why}.",
                        display(&folder.path)
                    ));
                }
                folders.push(held);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Outcome::AlreadyMissing,
            Err(e) => {
                return Outcome::Failed(format!(
                    "Couldn't reach the folder {} above it: {e}.",
                    display(&folder.path)
                ));
            }
        }
    }
    let held = match identity::hold(&item.path, false) {
        Ok((found, held)) => {
            if let Some(why) = changed(item, &found, target.volume) {
                return Outcome::Changed(format!("It {why}."));
            }
            held
        }
        Err(e) if e.kind() == ErrorKind::NotFound => return Outcome::AlreadyMissing,
        Err(e) => return Outcome::Failed(format!("Couldn't reach it: {e}.")),
    };
    let result = bin.recycle(&item.path);
    let place = match held.whereabouts() {
        Whereabouts::Moved(to) if bin.holds(&to) => Place::Bin,
        Whereabouts::Moved(_) => Place::Elsewhere,
        Whereabouts::Unmoved => Place::Unmoved,
        Whereabouts::Deleted => Place::Deleted,
        Whereabouts::Unknown => Place::Unknown,
    };
    drop(held);
    drop(folders);
    let present = !matches!(
        std::fs::symlink_metadata(&item.path),
        Err(e) if e.kind() == ErrorKind::NotFound
    );
    settle(result, present, place)
}

/// How what is on disk differs from what the scan found, if it does.
fn changed(scanned: &Scanned, found: &Found, volume: Option<u64>) -> Option<&'static str> {
    match (found.kind, scanned.folder) {
        (Kind::Link, _) => return Some("is now a link"),
        (Kind::Folder, false) => return Some("is now a folder"),
        (Kind::File, true) => return Some("is now a file"),
        (Kind::Other, _) => return Some("is now a different kind of item"),
        _ => {}
    }
    if volume.is_some_and(|v| v != found.volume) {
        return Some("is now on a different drive");
    }
    let recorded = scanned.ids.iter().filter(|&&id| id != 0);
    if recorded.clone().next().is_some() && !recorded.clone().any(|&id| found.has_id(id)) {
        return Some("was replaced since the scan");
    }
    if !scanned.folder {
        let modified = scanned.modified.zip(found.modified);
        if found.len != scanned.len
            || modified.is_some_and(|(then, now)| then != epoch_seconds(Some(now)))
        {
            return Some("was modified since the scan");
        }
    }
    None
}

/// Where the item held open during a move ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Bin,
    Unmoved,
    Elsewhere,
    Deleted,
    /// The platform can't say (macOS, Linux).
    Unknown,
}

/// The outcome from what the move returned, whether something is still at
/// the item's path, and where the item held open went. The filesystem is
/// trusted over the move's result, and an error is never dropped.
fn settle(result: Result<(), RecycleError>, present: bool, place: Place) -> Outcome {
    let deleted = || {
        format!(
            "It's gone, but it didn't go to the {BIN}: it may have been deleted without a copy."
        )
    };
    match (result, place) {
        (Err(RecycleError::Unrecycled), _) if !present || place == Place::Deleted => {
            Outcome::Uncertain(UNRECYCLED.into())
        }
        // Whatever else happened, the scanned item is in the bin; anything
        // at its path now came after.
        (_, Place::Bin) => Outcome::Recycled,
        (Ok(()), Place::Deleted) => Outcome::Uncertain(deleted()),
        // Gone, though still at its path when last seen: deleted, not moved.
        (Ok(()), Place::Unmoved) if !present => Outcome::Uncertain(deleted()),
        (Ok(()), Place::Elsewhere) => Outcome::Uncertain(format!(
            "It was moved elsewhere during the delete, so whatever was in its place may be in the {BIN} instead."
        )),
        (Ok(()), _) if !present => Outcome::Recycled,
        (Ok(()), _) => Outcome::Failed("It is still there after deleting.".into()),
        (Err(e), _) if present && place != Place::Deleted => Outcome::Failed(e.message()),
        (Err(e), _) => Outcome::Uncertain(format!(
            "{} It's no longer where it was, so it may have been deleted without a copy in the {BIN}.",
            e.message()
        )),
    }
}

fn display(path: &Path) -> String {
    format!(
        "“{}”",
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
    )
}

#[cfg(windows)]
fn recycle(path: &Path) -> Result<(), RecycleError> {
    crate::recycle_windows::recycle(path)
}

/// The trash crate moves items into the freedesktop Trash (copying across
/// filesystems before removing the original) or uses the macOS Trash; it
/// never deletes without keeping a copy.
#[cfg(not(windows))]
fn recycle(path: &Path) -> Result<(), RecycleError> {
    // The library can panic if the platform service is unavailable.
    match std::panic::catch_unwind(|| trash::delete(path)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(RecycleError::Failed(format!(
            "Couldn't move it to the Trash: {}",
            trash_error(&e)
        ))),
        Err(_) => Err(RecycleError::Failed("The Trash is unavailable.".into())),
    }
}

#[cfg(not(windows))]
fn trash_error(e: &trash::Error) -> String {
    match e {
        trash::Error::Unknown { description } | trash::Error::Os { description, .. } => {
            description.clone()
        }
        trash::Error::CouldNotAccess { .. } => "access was denied.".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "sb-delete-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Stands in for the trash in tests: a folder items are moved into.
    pub struct TestBin(TempDir);

    impl TestBin {
        pub fn new() -> Self {
            Self(TempDir::new("bin"))
        }
    }

    impl Bin for TestBin {
        fn recycle(&self, path: &Path) -> Result<(), RecycleError> {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let to = self
                .0
                .0
                .join(NEXT.fetch_add(1, Ordering::Relaxed).to_string());
            std::fs::rename(path, to).map_err(|e| RecycleError::Failed(e.to_string()))
        }

        fn holds(&self, path: &str) -> bool {
            // Paths on the volume, without the drive.
            let dir = self.0.0.to_string_lossy();
            let dir = dir.get(2..).unwrap_or_default();
            path.to_lowercase().starts_with(&dir.to_lowercase())
        }
    }

    /// A bin whose move does whatever `f` does.
    struct Fake<F>(F);

    impl<F: Fn(&Path) -> Result<(), RecycleError>> Bin for Fake<F> {
        fn recycle(&self, path: &Path) -> Result<(), RecycleError> {
            (self.0)(path)
        }

        fn holds(&self, _: &str) -> bool {
            false
        }
    }

    /// Deletes outright, as the shell does when it can't recycle.
    fn permanently(path: &Path) -> Result<(), RecycleError> {
        std::fs::remove_file(path).map_err(|e| RecycleError::Failed(e.to_string()))
    }

    /// What a scan of `path` would record, now.
    fn scanned(path: &Path) -> Scanned {
        let (found, _) = identity::hold(path, false).unwrap();
        Scanned {
            path: path.to_owned(),
            folder: found.kind == Kind::Folder,
            ids: found.ids,
            len: if found.kind == Kind::Folder {
                0
            } else {
                found.len
            },
            modified: Some(epoch_seconds(found.modified)),
        }
    }

    /// The item with every folder above it from `root`.
    fn target(root: &Path, item: &Path) -> Target {
        let mut chain = vec![scanned(item)];
        let mut at = item;
        while at != root {
            at = at.parent().unwrap();
            chain.push(scanned(at));
        }
        chain.reverse();
        let volume = identity::hold(root, false).unwrap().0.volume;
        Target {
            chain,
            volume: Some(volume),
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn counts_an_item_deleted_only_once_it_is_gone() {
        let tmp = TempDir::new("gone");
        let file = tmp.0.join("a.txt");
        write(&file, "x");
        let t = target(&tmp.0, &file);
        assert_eq!(
            delete_with(&t, &Fake(|_: &Path| Ok(()))),
            Outcome::Failed("It is still there after deleting.".into())
        );
        assert_eq!(
            delete_with(
                &t,
                &Fake(|_: &Path| Err(RecycleError::Failed("refused".into())))
            ),
            Outcome::Failed("refused".into())
        );
        let bin = TestBin::new();
        assert_eq!(delete_with(&t, &bin), Outcome::Recycled);
        assert_eq!(delete_with(&t, &bin), Outcome::AlreadyMissing);
    }

    /// A move that reports success but deleted the item outright is caught
    /// where the platform can tell (Windows), never called recycled there.
    #[test]
    fn an_item_deleted_without_a_copy_is_not_called_recycled() {
        let tmp = TempDir::new("outright");
        let file = tmp.0.join("a.txt");
        write(&file, "x");
        let outcome = delete_with(&target(&tmp.0, &file), &Fake(permanently));
        if cfg!(windows) {
            assert!(matches!(outcome, Outcome::Uncertain(_)), "{outcome:?}");
        } else {
            assert_eq!(outcome, Outcome::Recycled);
        }
    }

    #[test]
    fn a_file_put_in_place_of_the_scanned_one_is_left_alone() {
        let tmp = TempDir::new("replaced");
        let file = tmp.0.join("a.txt");
        write(&file, "same");
        let t = target(&tmp.0, &file);
        // Same name, size, and (to the second) modified time; another file.
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::rename(&file, tmp.0.join("moved.txt")).unwrap();
        write(&file, "same");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(
            delete_with(&t, &TestBin::new()),
            Outcome::Changed("It was replaced since the scan.".into())
        );
        assert!(file.exists());
    }

    #[test]
    fn a_replaced_folder_above_the_item_is_noticed() {
        let tmp = TempDir::new("ancestor");
        let file = tmp.0.join("dir/inner/a.txt");
        write(&file, "x");
        let t = target(&tmp.0, &file);
        // Another folder holding a file of the same name and contents.
        std::fs::rename(tmp.0.join("dir"), tmp.0.join("old")).unwrap();
        write(&file, "x");
        assert_eq!(
            delete_with(&t, &TestBin::new()),
            Outcome::Changed("The folder “dir” above it was replaced since the scan.".into())
        );
        assert!(file.exists());
    }

    #[test]
    fn a_modified_file_is_left_alone() {
        let tmp = TempDir::new("modified");
        let file = tmp.0.join("a.txt");
        write(&file, "x");
        let t = target(&tmp.0, &file);
        std::fs::write(&file, "longer").unwrap();
        assert_eq!(
            delete_with(&t, &TestBin::new()),
            Outcome::Changed("It was modified since the scan.".into())
        );
    }

    #[test]
    fn a_missing_folder_above_means_already_missing() {
        let tmp = TempDir::new("missing");
        let file = tmp.0.join("dir/a.txt");
        write(&file, "x");
        let t = target(&tmp.0, &file);
        std::fs::remove_dir_all(tmp.0.join("dir")).unwrap();
        assert_eq!(delete_with(&t, &TestBin::new()), Outcome::AlreadyMissing);
    }

    #[test]
    fn items_that_changed_kind_are_left_alone() {
        let tmp = TempDir::new("kind");
        let path = tmp.0.join("was-a-folder");
        std::fs::create_dir(&path).unwrap();
        let t = target(&tmp.0, &path);
        std::fs::remove_dir(&path).unwrap();
        write(&path, "x");
        assert_eq!(
            delete_with(&t, &TestBin::new()),
            Outcome::Changed("It is now a file.".into())
        );
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_left_alone() {
        let tmp = TempDir::new("link");
        let target_dir = tmp.0.join("target");
        std::fs::create_dir(&target_dir).unwrap();
        let link = tmp.0.join("link");
        std::fs::create_dir(&link).unwrap();
        let t = target(&tmp.0, &link);
        std::fs::remove_dir(&link).unwrap();
        std::os::unix::fs::symlink(&target_dir, &link).unwrap();
        assert_eq!(
            delete_with(&t, &TestBin::new()),
            Outcome::Changed("It is now a link.".into())
        );
        assert!(target_dir.exists() && link.exists());
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_left_alone() {
        let tmp = TempDir::new("junction");
        let target_dir = tmp.0.join("target");
        std::fs::create_dir(&target_dir).unwrap();
        let junction = tmp.0.join("inner");
        std::fs::create_dir(&junction).unwrap();
        let t = target(&tmp.0, &junction);
        std::fs::remove_dir(&junction).unwrap();
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&junction)
            .arg(&target_dir)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert_eq!(
            delete_with(&t, &TestBin::new()),
            Outcome::Changed("It is now a link.".into())
        );
        assert!(target_dir.exists() && junction.exists());
    }

    /// Folders above an item can't be renamed while it is being deleted.
    #[cfg(windows)]
    #[test]
    fn folders_above_are_locked_during_the_move() {
        let tmp = TempDir::new("locked");
        let file = tmp.0.join("dir/a.txt");
        write(&file, "x");
        let t = target(&tmp.0, &file);
        let renamed = std::cell::Cell::new(None);
        let bin = TestBin::new();
        let outcome = delete_with(
            &t,
            &Fake(|p: &Path| {
                renamed.set(Some(
                    std::fs::rename(tmp.0.join("dir"), tmp.0.join("other")).is_ok(),
                ));
                bin.recycle(p)
            }),
        );
        assert_eq!(renamed.get(), Some(false));
        // This fake bin doesn't recognise its own folder.
        assert!(matches!(outcome, Outcome::Uncertain(_)), "{outcome:?}");
        assert!(!file.exists());
    }

    /// Puts a small file and a folder in the real Recycle Bin or Trash,
    /// with the checks' handles held, in `SB_RECYCLE_TEST_DIR` (else the
    /// temporary folder). Run by hand:
    /// `cargo test -p spacebadger recycles_through_the_checks -- --ignored`.
    #[test]
    #[ignore]
    fn recycles_through_the_checks() {
        let base = std::env::var_os("SB_RECYCLE_TEST_DIR")
            .map_or_else(std::env::temp_dir, PathBuf::from)
            .join(format!("sb-recycle-checks-{}", std::process::id()));
        let file = base.join("dir/a.txt");
        let folder = base.join("dir/folder");
        write(&file, "recycle me");
        write(&folder.join("inside.txt"), "and me");
        let outcomes = (
            delete(&target(&base, &file)),
            delete(&target(&base, &folder)),
        );
        let gone = !file.exists() && !folder.exists();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(outcomes, (Outcome::Recycled, Outcome::Recycled));
        assert!(gone);
    }

    #[test]
    fn errors_are_kept_when_the_item_is_gone() {
        let unknown = Place::Unknown;
        let failed = || Err(RecycleError::Failed("Access is denied.".into()));
        assert_eq!(settle(Ok(()), false, unknown), Outcome::Recycled);
        assert_eq!(
            settle(failed(), true, unknown),
            Outcome::Failed("Access is denied.".into())
        );
        assert!(matches!(
            settle(failed(), false, unknown),
            Outcome::Uncertain(m) if m.starts_with("Access is denied. It's no longer where it was")
        ));
        assert_eq!(
            settle(Err(RecycleError::Unrecycled), false, unknown),
            Outcome::Uncertain(UNRECYCLED.into())
        );
        // Even when the held item says it reached the Recycle Bin.
        assert_eq!(
            settle(Err(RecycleError::Unrecycled), false, Place::Bin),
            Outcome::Uncertain(UNRECYCLED.into())
        );
        assert!(matches!(
            settle(
                Err(RecycleError::Refused("no bin".into())),
                true,
                Place::Unmoved
            ),
            Outcome::Failed(m) if m == "no bin"
        ));
    }

    #[test]
    fn the_held_item_decides_what_was_recycled() {
        assert_eq!(settle(Ok(()), false, Place::Bin), Outcome::Recycled);
        // Something new at its path came after the scanned item left.
        assert_eq!(settle(Ok(()), true, Place::Bin), Outcome::Recycled);
        assert!(matches!(
            settle(Ok(()), false, Place::Elsewhere),
            Outcome::Uncertain(_)
        ));
        assert!(matches!(
            settle(Ok(()), false, Place::Unmoved),
            Outcome::Uncertain(_)
        ));
    }
}

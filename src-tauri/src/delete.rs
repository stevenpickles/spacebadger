//! Moving scanned items to the Recycle Bin or Trash, one at a time so each
//! gets its own outcome. Nothing is ever deleted permanently.

use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Deleted,
    /// Nothing was at the path any more.
    AlreadyGone,
    Failed(String),
}

/// Moves the item the scan found at `path` to the Recycle Bin or Trash.
pub fn delete(path: &Path, folder: bool) -> Outcome {
    delete_with(path, folder, recycle)
}

/// [`delete`] with the step that removes the item from its place passed
/// in, so the checks around it can be tested without filling the trash.
///
/// `folder` is what the scan saw at `path`; the item is left alone if it
/// has become something else since, including a link, so a delete never
/// reaches past what was scanned.
pub fn delete_with(
    path: &Path,
    folder: bool,
    remove: impl FnOnce(&Path) -> Result<(), String>,
) -> Outcome {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Outcome::AlreadyGone,
        Err(e) => return Outcome::Failed(format!("Couldn't reach it: {e}.")),
    };
    if meta.file_type().is_symlink() || meta.is_dir() != folder {
        return Outcome::Failed("It has changed since the scan. Refresh, then try again.".into());
    }
    let result = remove(path);
    // Trust the filesystem over the call's result.
    match (result, std::fs::symlink_metadata(path)) {
        (_, Err(e)) if e.kind() == ErrorKind::NotFound => Outcome::Deleted,
        (Err(message), _) => Outcome::Failed(message),
        (Ok(()), _) => Outcome::Failed("It is still there after deleting.".into()),
    }
}

#[cfg(windows)]
fn recycle(path: &Path) -> Result<(), String> {
    crate::recycle_windows::recycle(path)
}

/// The trash crate moves items into the freedesktop Trash (copying across
/// filesystems before removing the original) or uses the macOS Trash; it
/// never deletes without keeping a copy.
#[cfg(not(windows))]
fn recycle(path: &Path) -> Result<(), String> {
    // The library can panic if the platform service is unavailable.
    match std::panic::catch_unwind(|| trash::delete(path)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!(
            "Couldn't move it to the Trash: {}",
            trash_error(&e)
        )),
        Err(_) => Err("The Trash is unavailable.".into()),
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
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
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

    /// Stands in for the trash in tests.
    fn remove(path: &Path) -> Result<(), String> {
        let r = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        r.map_err(|e| e.to_string())
    }

    #[test]
    fn counts_an_item_deleted_only_once_it_is_gone() {
        let tmp = TempDir::new("gone");
        let file = tmp.0.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            delete_with(&file, false, |_| Ok(())),
            Outcome::Failed("It is still there after deleting.".into())
        );
        assert_eq!(
            delete_with(&file, false, |_| Err("refused".into())),
            Outcome::Failed("refused".into())
        );
        assert_eq!(delete_with(&file, false, remove), Outcome::Deleted);
        assert_eq!(delete_with(&file, false, remove), Outcome::AlreadyGone);
    }

    #[test]
    fn items_that_changed_kind_are_left_alone() {
        let tmp = TempDir::new("kind");
        let file = tmp.0.join("was-a-folder");
        std::fs::write(&file, b"x").unwrap();
        let folder = tmp.0.join("was-a-file");
        std::fs::create_dir(&folder).unwrap();
        assert!(matches!(
            delete_with(&file, true, remove),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            delete_with(&folder, false, remove),
            Outcome::Failed(_)
        ));
        assert!(file.exists() && folder.exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_left_alone() {
        let tmp = TempDir::new("link");
        let target = tmp.0.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            delete_with(&link, true, remove),
            Outcome::Failed(_)
        ));
        assert!(target.exists() && link.exists());
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_left_alone() {
        let tmp = TempDir::new("junction");
        let target = tmp.0.join("target");
        std::fs::create_dir(&target).unwrap();
        let junction = tmp.0.join("inner");
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert!(matches!(
            delete_with(&junction, true, remove),
            Outcome::Failed(_)
        ));
        assert!(target.exists() && junction.exists());
    }
}

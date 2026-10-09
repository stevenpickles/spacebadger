//! Deleting scanned items from disk, one at a time so each gets its own
//! outcome.

use sb_protocol::DeleteMode;
use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Deleted,
    /// Nothing was at the path any more.
    AlreadyGone,
    Failed(String),
}

/// Deletes the item the scan found at `path`. `folder` is what the scan saw
/// there; the item is left alone if it has become something else since,
/// including a link, so a delete never reaches past what was scanned.
pub fn delete(path: &Path, folder: bool, mode: DeleteMode) -> Outcome {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Outcome::AlreadyGone,
        Err(e) => return Outcome::Failed(format!("Couldn't reach it: {}.", describe(&e))),
    };
    if meta.file_type().is_symlink() || meta.is_dir() != folder {
        return Outcome::Failed("It has changed since the scan. Refresh, then try again.".into());
    }
    let result = match mode {
        DeleteMode::Recycle => recycle(path),
        DeleteMode::Permanent => remove(path, folder),
    };
    // Trust the filesystem over the call's result.
    match (result, std::fs::symlink_metadata(path)) {
        (_, Err(e)) if e.kind() == ErrorKind::NotFound => Outcome::Deleted,
        (Err(message), _) => Outcome::Failed(message),
        (Ok(()), _) => Outcome::Failed("It is still there after deleting.".into()),
    }
}

/// What the platform calls the place deleted items are kept.
pub fn trash_name() -> &'static str {
    if cfg!(windows) {
        "Recycle Bin"
    } else {
        "Trash"
    }
}

fn recycle(path: &Path) -> Result<(), String> {
    // The library can panic if the platform service is unavailable.
    match std::panic::catch_unwind(|| trash::delete(path)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!(
            "Couldn't move it to the {}: {}",
            trash_name(),
            trash_error(&e)
        )),
        Err(_) => Err(format!("The {} is unavailable.", trash_name())),
    }
}

fn trash_error(e: &trash::Error) -> String {
    match e {
        trash::Error::Unknown { description } if description.contains("aborted") => {
            "the move was cancelled or blocked. It may be in use.".into()
        }
        trash::Error::Unknown { description } | trash::Error::Os { description, .. } => {
            description.clone()
        }
        trash::Error::CouldNotAccess { .. } => "access was denied.".into(),
        other => other.to_string(),
    }
}

fn remove(path: &Path, folder: bool) -> Result<(), String> {
    let result = if folder {
        // Removes links inside the folder without following them.
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path).or_else(|e| {
            // Windows refuses to delete read-only files; Explorer clears the
            // attribute first, so do the same.
            if cfg!(windows) && e.kind() == ErrorKind::PermissionDenied && clear_read_only(path) {
                std::fs::remove_file(path)
            } else {
                Err(e)
            }
        })
    };
    result.map_err(|e| {
        if folder && path.exists() {
            format!(
                "Couldn't delete all of it: {}. Some contents may be gone already; Refresh to see what's left.",
                describe(&e)
            )
        } else {
            format!("Couldn't delete it: {}.", describe(&e))
        }
    })
}

/// Clears the read-only attribute; `false` if it wasn't set or can't be.
fn clear_read_only(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let mut permissions = meta.permissions();
    if !permissions.readonly() {
        return false;
    }
    // Only reached on Windows, where this clears the attribute alone.
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).is_ok()
}

fn describe(e: &std::io::Error) -> String {
    match e.kind() {
        ErrorKind::PermissionDenied => {
            "access was denied. It may be in use, protected, or need administrator rights".into()
        }
        _ => e.to_string(),
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

    fn read_only(path: &Path) {
        let mut p = std::fs::metadata(path).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(path, p).unwrap();
    }

    #[test]
    fn permanently_deletes_files_and_folders() {
        let tmp = TempDir::new("perm");
        let file = tmp.0.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let folder = tmp.0.join("d");
        std::fs::create_dir_all(folder.join("e")).unwrap();
        std::fs::write(folder.join("e").join("b.bin"), b"yy").unwrap();
        let locked = folder.join("read-only.txt");
        std::fs::write(&locked, b"z").unwrap();
        read_only(&locked);

        assert_eq!(
            delete(&file, false, DeleteMode::Permanent),
            Outcome::Deleted
        );
        assert_eq!(
            delete(&folder, true, DeleteMode::Permanent),
            Outcome::Deleted
        );
        assert!(!file.exists() && !folder.exists());
        assert_eq!(
            delete(&file, false, DeleteMode::Permanent),
            Outcome::AlreadyGone
        );
    }

    #[test]
    fn read_only_files_are_deleted() {
        let tmp = TempDir::new("ro");
        let file = tmp.0.join("ro.txt");
        std::fs::write(&file, b"x").unwrap();
        read_only(&file);
        assert_eq!(
            delete(&file, false, DeleteMode::Permanent),
            Outcome::Deleted
        );
    }

    #[test]
    fn items_that_changed_kind_are_left_alone() {
        let tmp = TempDir::new("kind");
        let file = tmp.0.join("was-a-folder");
        std::fs::write(&file, b"x").unwrap();
        let folder = tmp.0.join("was-a-file");
        std::fs::create_dir(&folder).unwrap();
        assert!(matches!(
            delete(&file, true, DeleteMode::Permanent),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            delete(&folder, false, DeleteMode::Permanent),
            Outcome::Failed(_)
        ));
        assert!(file.exists() && folder.exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed() {
        let tmp = TempDir::new("link");
        let target = tmp.0.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"x").unwrap();
        let link = tmp.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            delete(&link, true, DeleteMode::Permanent),
            Outcome::Failed(_)
        ));
        // A folder holding a link to elsewhere loses the link, not the target.
        let holder = tmp.0.join("holder");
        std::fs::create_dir(&holder).unwrap();
        std::os::unix::fs::symlink(&target, holder.join("inner")).unwrap();
        assert_eq!(
            delete(&holder, true, DeleteMode::Permanent),
            Outcome::Deleted
        );
        assert!(target.join("keep.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_never_followed() {
        let tmp = TempDir::new("junction");
        let target = tmp.0.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"x").unwrap();
        let holder = tmp.0.join("holder");
        std::fs::create_dir(&holder).unwrap();
        let junction = holder.join("inner");
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
            delete(&junction, true, DeleteMode::Permanent),
            Outcome::Failed(_)
        ));
        assert_eq!(
            delete(&holder, true, DeleteMode::Permanent),
            Outcome::Deleted
        );
        assert!(target.join("keep.txt").exists());
    }
}

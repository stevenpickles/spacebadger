//! Native filesystem adapters. [`NativeFs`] is the adapter for the current OS.

#[cfg(windows)]
use crate::duplicates::Held;
use crate::duplicates::{Contents, Fingerprint, Skip};
use crate::engine::ScanConfig;
use std::path::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(unix, not(target_os = "linux")))]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::LinuxFs as NativeFs;
#[cfg(all(unix, not(target_os = "linux")))]
pub use unix::UnixFs as NativeFs;
#[cfg(windows)]
pub use windows::WindowsFs as NativeFs;

/// Concurrent listings for roots on network filesystems, where many parallel
/// metadata requests mostly queue on the server.
pub const REMOTE_WORKERS: usize = 3;

/// Whether `root` is on a network share. Unknown on macOS (always `false`
/// until a Mac is available to validate it).
pub fn is_remote(root: &Path) -> bool {
    #[cfg(windows)]
    return windows::is_remote(root);
    #[cfg(target_os = "linux")]
    return linux::is_remote(root);
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = root;
        false
    }
}

/// Capacity of the volume that holds a scan root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// The root is the volume's top folder (a drive or mount point), so the
    /// scan covers the whole volume.
    pub is_root: bool,
    pub capacity: u64,
    /// Free bytes on the volume, including any reserved for administrators.
    pub free: u64,
    /// Filesystem name, such as "NTFS", where the OS reports one.
    pub filesystem: Option<String>,
    /// Bytes the filesystem uses for its own structures that could be
    /// measured (on NTFS, the master file table when elevated).
    pub metadata: Option<u64>,
}

/// The volume holding `root`; `None` where it can't be read or on platforms
/// not yet supported (macOS).
pub fn volume_info(root: &Path) -> Option<VolumeInfo> {
    #[cfg(windows)]
    return windows::volume_info(root);
    #[cfg(target_os = "linux")]
    return linux::volume_info(root);
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = root;
        None
    }
}

/// Scan settings suited to `root`: fewer workers for network shares.
pub fn scan_config(root: &Path) -> ScanConfig {
    let mut config = ScanConfig::default();
    if is_remote(root) {
        config.workers = config.workers.min(REMOTE_WORKERS);
    }
    config
}

/// Whether scans bypass permission checks because the process was started
/// with administrator or root rights. On Windows this enables the backup
/// privilege the process already holds; nothing ever requests elevation.
pub fn privileged_access() -> bool {
    #[cfg(windows)]
    return windows::enable_backup_privilege();
    #[cfg(target_os = "linux")]
    return linux::is_privileged();
    #[cfg(all(unix, not(target_os = "linux")))]
    false
}

/// Opens files to read their contents, for finding duplicates: never
/// through a link, never a cloud file, and only a plain file of the size
/// the scan measured.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeContents;

impl Contents for NativeContents {
    type File = std::fs::File;

    fn open(&self, path: &Path, len: u64) -> Result<(std::fs::File, Fingerprint), Skip> {
        #[cfg(windows)]
        return windows::open_contents(path, len);
        #[cfg(target_os = "linux")]
        return linux::open_contents(path, len);
        #[cfg(all(unix, not(target_os = "linux")))]
        return unix::open_contents(path, len);
    }

    fn fingerprint(&self, file: &std::fs::File) -> Result<Fingerprint, Skip> {
        #[cfg(windows)]
        return windows::fingerprint(file);
        #[cfg(unix)]
        return file
            .metadata()
            .map(|m| fingerprint(&m))
            .map_err(|e| Skip::of(&e));
    }

    fn examine(&self, path: &Path, len: u64) -> Result<Fingerprint, Skip> {
        #[cfg(windows)]
        return windows::examine_contents(path, len);
        #[cfg(target_os = "linux")]
        return linux::examine_contents(path, len);
        #[cfg(all(unix, not(target_os = "linux")))]
        return unix::examine_contents(path, len);
    }

    /// On Windows the file is opened sharing only reading; other systems
    /// have no such lock, so it's only checked.
    #[cfg(windows)]
    fn hold(&self, path: &Path, print: &Fingerprint) -> Result<Held, Skip> {
        Ok(Box::new(windows::hold_contents(path, print)?))
    }
}

/// A file's device and inode, length, modified time, and number of names.
#[cfg(unix)]
fn fingerprint(meta: &std::fs::Metadata) -> Fingerprint {
    use std::os::unix::fs::MetadataExt;
    Fingerprint {
        id: Some((meta.dev(), u128::from(meta.ino()))),
        len: meta.len(),
        modified: meta.modified().ok(),
        links: u32::try_from(meta.nlink()).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn opens_plain_files_of_the_scanned_size_only() {
        let dir = std::env::temp_dir().join(format!("sb-contents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.bin");
        std::fs::write(&file, b"hello").unwrap();

        let mut text = String::new();
        let (mut opened, print) = NativeContents.open(&file, 5).unwrap();
        opened.read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");
        assert!(print.id.is_some() && print.modified.is_some());
        assert_eq!((print.len, print.links), (5, Some(1)));
        assert_eq!(NativeContents.fingerprint(&opened).unwrap(), print);
        assert_eq!(NativeContents.examine(&file, 5).unwrap(), print);
        drop(opened);
        // Rewritten to the same length at another time: same file, but
        // not unchanged.
        let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        std::io::Write::write_all(&mut &f, b"HELLO").unwrap();
        f.set_modified(mtime).unwrap();
        drop(f);
        let now = NativeContents.examine(&file, 5).unwrap();
        assert_eq!((now.id, now.modified), (print.id, Some(mtime)));
        assert!(!print.unchanged(&now));
        let other = dir.join("b.bin");
        std::fs::write(&other, b"hello").unwrap();
        assert_ne!(NativeContents.examine(&other, 5).unwrap().id, print.id);
        std::fs::hard_link(&other, dir.join("c.bin")).unwrap();
        assert_eq!(NativeContents.examine(&other, 5).unwrap().links, Some(2));
        assert_eq!(NativeContents.open(&file, 4).unwrap_err(), Skip::Changed);
        assert_eq!(
            NativeContents.open(&dir.join("gone"), 5).unwrap_err(),
            Skip::Vanished
        );
        #[cfg(unix)]
        {
            let link = dir.join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert_eq!(NativeContents.open(&link, 5).unwrap_err(), Skip::Changed);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_held_file_cant_change_but_others_can_go() {
        let dir = std::env::temp_dir().join(format!("sb-hold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (kept, other) = (dir.join("kept.bin"), dir.join("other.bin"));
        std::fs::write(&kept, b"hello").unwrap();
        std::fs::write(&other, b"hello").unwrap();
        let print = NativeContents.examine(&kept, 5).unwrap();

        let held = NativeContents.hold(&kept, &print).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&kept).is_err());
        assert!(std::fs::remove_file(&kept).is_err());
        assert!(std::fs::rename(&kept, dir.join("moved.bin")).is_err());
        assert!(std::fs::rename(&other, &kept).is_err(), "nor replaced");
        assert_eq!(std::fs::read(&kept).unwrap(), b"hello", "still readable");
        let (mut reread, _) = NativeContents.open(&kept, 5).unwrap();
        let mut text = String::new();
        reread.read_to_string(&mut text).unwrap();
        drop(reread);
        std::fs::remove_file(&other).unwrap();
        drop(held);
        std::fs::write(&kept, b"HELLO").unwrap();

        // Changed since compared, or open to write elsewhere: not held.
        assert_eq!(
            NativeContents.hold(&kept, &print).err(),
            Some(Skip::Changed)
        );
        let print = NativeContents.examine(&kept, 5).unwrap();
        let writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&kept)
            .unwrap();
        assert_eq!(NativeContents.hold(&kept, &print).err(), Some(Skip::InUse));
        drop(writer);
        assert!(NativeContents.hold(&kept, &print).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Native filesystem adapters. [`NativeFs`] is the adapter for the current OS.

use crate::duplicates::{Contents, Skip};
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

    fn open(&self, path: &Path, len: u64) -> Result<std::fs::File, Skip> {
        #[cfg(windows)]
        return windows::open_contents(path, len);
        #[cfg(target_os = "linux")]
        return linux::open_contents(path, len);
        #[cfg(all(unix, not(target_os = "linux")))]
        return unix::open_contents(path, len);
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
        NativeContents
            .open(&file, 5)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello");
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
}

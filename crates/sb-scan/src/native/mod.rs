//! Native filesystem adapters. [`NativeFs`] is the adapter for the current OS.

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

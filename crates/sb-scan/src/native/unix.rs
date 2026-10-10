//! Portable Unix adapter (macOS and other non-Linux Unix).
//!
//! **Unvalidated on macOS.** It compiles in CI, but no probes have run on a
//! real Mac. Known gaps, tracked in `docs/PLAN.md`:
//! - Uses `readdir` + `lstat` rather than `getattrlistbulk`.
//! - Mount boundaries use the device ID only. Scanning `/` on macOS will treat
//!   firmlinked data-volume folders as other filesystems.
//!
//! On macOS the process I/O policy is set so dataless (cloud-only) files are
//! never materialized by this process, and `SF_DATALESS` marks cloud files.

use crate::adapter::{
    Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind, ScanError,
};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_int;

    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
    const IOPOL_SCOPE_PROCESS: c_int = 0;
    const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;
    pub const SF_DATALESS: u32 = 0x4000_0000;

    unsafe extern "C" {
        fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
    }

    /// Prevents this process from materializing dataless files.
    pub fn disable_materialization() -> std::io::Result<()> {
        let rc = unsafe {
            setiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_PROCESS,
                IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    pub fn is_dataless(meta: &std::fs::Metadata) -> bool {
        #[allow(deprecated)]
        let flags = std::os::macos::fs::MetadataExt::st_flags(meta);
        flags & SF_DATALESS != 0
    }
}

/// Portable Unix adapter. Create one per scan.
#[derive(Default)]
pub struct UnixFs {
    root_dev: OnceLock<u64>,
}

impl UnixFs {
    pub fn new() -> Self {
        Self::default()
    }
}

fn is_cloud(_meta: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    return macos::is_dataless(_meta);
    #[cfg(not(target_os = "macos"))]
    false
}

impl FsAdapter for UnixFs {
    fn open_root(&self, root: &Path) -> Result<RootKind, ScanError> {
        #[cfg(target_os = "macos")]
        macos::disable_materialization().map_err(|err| {
            ScanError::new(
                ErrorKind::Unavailable,
                format!("can't disable cloud file materialization: {err}"),
            )
        })?;
        let meta = std::fs::symlink_metadata(root)?;
        let _ = self.root_dev.set(meta.dev());
        Ok(if meta.file_type().is_symlink() {
            RootKind::Link
        } else if meta.is_dir() {
            RootKind::Directory
        } else {
            RootKind::NotDirectory
        })
    }

    fn read_dir(&self, dir: &Path) -> Result<Vec<Entry>, ScanError> {
        let root_dev = *self
            .root_dev
            .get()
            .expect("open_root must run before read_dir");
        let mut entries = Vec::new();
        for item in std::fs::read_dir(dir)? {
            let item = item?;
            let name = item.file_name();
            let meta = match std::fs::symlink_metadata(item.path()) {
                Ok(meta) => meta,
                Err(err) => {
                    entries.push(Entry {
                        name,
                        kind: EntryKind::Failed(ScanError::from(err).kind),
                        logical: 0,
                        allocated: None,
                        identity: Identity::Unavailable,
                        cloud: false,
                    });
                    continue;
                }
            };
            let file_type = meta.file_type();
            let kind = if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_dir() {
                if meta.dev() == root_dev {
                    EntryKind::Directory
                } else {
                    EntryKind::MountPoint
                }
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                EntryKind::Special
            };
            entries.push(Entry {
                name,
                kind,
                logical: meta.size(),
                allocated: Some(meta.blocks().saturating_mul(512)),
                identity: if meta.nlink() > 1 {
                    Identity::MaybeShared {
                        key: (u128::from(meta.dev()) << 64) | u128::from(meta.ino()),
                    }
                } else {
                    Identity::Unique
                },
                cloud: is_cloud(&meta),
            });
        }
        Ok(entries)
    }

    fn measure_file(&self, file: &Path) -> Result<Measured, ScanError> {
        let meta = std::fs::symlink_metadata(file)?;
        if !meta.is_file() {
            return Err(ScanError::new(
                ErrorKind::Vanished,
                "no longer a regular file",
            ));
        }
        Ok(Measured {
            logical: meta.size(),
            allocated: Some(meta.blocks().saturating_mul(512)),
        })
    }
}

//! NTFS master file table parsing for the elevated whole-volume scan
//! (design idea 2.3). Prototype: only `sb-probe mft` uses it until its
//! agreement test has passed elevated; see `docs/validation/mft-probe.md`.
//!
//! Everything here works on bytes, so it builds and is tested on every
//! platform; reading the volume is the caller's (Windows-only) job.
//!
//! - [`record`]: update-sequence fixups, the attribute walk, run lists, and
//!   the per-record fields a scan needs.
//! - [`table`]: extension records merged into their base, names per
//!   record, and a walk with bottom-up subtree totals.
//! - [`usn`]: USN journal records, to tell which records changed while the
//!   table was read.
//!
//! Kinds come from [`classify`], which decides exactly as the Windows
//! listing adapter does from the same attributes and reparse tag.

pub mod record;
pub mod table;
pub mod usn;

#[cfg(test)]
mod tests;

use crate::adapter::EntryKind;

/// `FILE_ATTRIBUTE_*` flags the scan looks at.
pub mod attr {
    pub const DIRECTORY: u32 = 0x10;
    pub const DEVICE: u32 = 0x40;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const OFFLINE: u32 = 0x1000;
    pub const RECALL_ON_OPEN: u32 = 0x40000;
    pub const PINNED: u32 = 0x80000;
    pub const UNPINNED: u32 = 0x100000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    pub const CLOUD: u32 = OFFLINE | RECALL_ON_OPEN | PINNED | UNPINNED | RECALL_ON_DATA_ACCESS;
}

/// Reparse tags the scan tells apart.
pub mod tag {
    pub const MOUNT_POINT: u32 = 0xA000_0003;
    pub const SYMLINK: u32 = 0xA000_000C;
    pub const LX_SYMLINK: u32 = 0xA000_001D;
    pub const WCI_LINK: u32 = 0xA000_0027;
    pub const AF_UNIX: u32 = 0x8000_0023;
    pub const LX_FIFO: u32 = 0x8000_0024;
    pub const LX_CHR: u32 = 0x8000_0025;
    pub const LX_BLK: u32 = 0x8000_0026;
    pub const WOF: u32 = 0x8000_0017;

    /// `IO_REPARSE_TAG_CLOUD` and its `CLOUD_1`..`CLOUD_F` variants.
    pub fn is_cloud(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == 0x9000_001A
    }

    /// Name-surrogate tags redirect to another path (bit 29).
    pub fn is_name_surrogate(tag: u32) -> bool {
        tag & 0x2000_0000 != 0
    }
}

/// Classifies an entry from its attributes and reparse tag, with the same
/// decisions as the Windows listing adapter: links and mount points are
/// reported and never followed, cloud placeholders are ordinary files and
/// folders, unknown directory redirections are skipped, and unknown file
/// tags (dedup, WOF, HSM) are measured as files.
pub fn classify(attributes: u32, reparse_tag: u32) -> EntryKind {
    let is_dir = attributes & attr::DIRECTORY != 0;
    if attributes & attr::REPARSE_POINT != 0 {
        match reparse_tag {
            tag::SYMLINK | tag::LX_SYMLINK | tag::WCI_LINK => return EntryKind::Symlink,
            tag::MOUNT_POINT => return EntryKind::MountPoint,
            tag::AF_UNIX | tag::LX_FIFO | tag::LX_CHR | tag::LX_BLK => return EntryKind::Special,
            t if tag::is_cloud(t) => {}
            t if tag::is_name_surrogate(t) => return EntryKind::UnknownReparse,
            _ if is_dir => return EntryKind::UnknownReparse,
            _ => {}
        }
    }
    if is_dir {
        EntryKind::Directory
    } else if attributes & attr::DEVICE != 0 {
        EntryKind::Special
    } else {
        EntryKind::File
    }
}

/// Whether an entry belongs to a cloud provider, as the listing adapter
/// decides: a cloud attribute, or a cloud reparse tag.
pub fn is_cloud(attributes: u32, reparse_tag: u32) -> bool {
    attributes & attr::CLOUD != 0
        || (attributes & attr::REPARSE_POINT != 0 && tag::is_cloud(reparse_tag))
}

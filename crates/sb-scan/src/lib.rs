//! Scoped filesystem traversal with per-platform metadata, mount, link, and
//! cloud-placeholder adapters. See `docs/validation/` for the probe results
//! behind each adapter's choices.

pub mod adapter;
pub mod duplicates;
pub mod engine;
pub mod fake;
pub mod git;
#[cfg(feature = "mft")]
pub mod mft;
pub mod native;

pub use adapter::{
    BATCH_ENTRIES, Batcher, Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind,
    ScanError, Sink, batched, read_capped, refused,
};
pub use engine::{
    GIT_GRACE, HoldProbe, OmissionReason, OmissionSample, Omissions, Progress, Scan, ScanConfig,
    ScanState,
};
pub use native::NativeFs;

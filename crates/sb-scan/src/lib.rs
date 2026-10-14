//! Scoped filesystem traversal with per-platform metadata, mount, link, and
//! cloud-placeholder adapters. See `docs/validation/` for the probe results
//! behind each adapter's choices.

pub mod adapter;
pub mod duplicates;
pub mod engine;
pub mod fake;
pub mod native;

pub use adapter::{
    BATCH_ENTRIES, Batcher, Entry, EntryKind, ErrorKind, FsAdapter, Identity, Measured, RootKind,
    ScanError, Sink, batched,
};
pub use engine::{
    HoldProbe, OmissionReason, OmissionSample, Omissions, Progress, Scan, ScanConfig, ScanState,
};
pub use native::NativeFs;

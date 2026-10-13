//! Comparing the current scan with a snapshot of an earlier scan of the
//! same folder. Snapshots are saved when a scan completes; see
//! `sb_core::snapshot` for what they hold.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// An earlier scan of the current root that the scan can be compared with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SnapshotInfo {
    /// Names the snapshot in later requests.
    pub id: String,
    /// When that scan finished, in milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub created_ms: u64,
    /// Logical bytes then.
    #[ts(type = "number")]
    pub logical: u64,
    #[ts(type = "number")]
    pub files: u64,
}

/// The comparison now on, returned when it's turned on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CompareSummary {
    #[ts(type = "number")]
    pub generation: u64,
    pub snapshot: SnapshotInfo,
    /// Files smaller than this were recorded without their size, so their
    /// changes aren't known.
    #[ts(type = "number")]
    pub threshold: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ChangeKind {
    Grown,
    Shrunk,
    Unchanged,
    /// Not there before.
    New,
    /// There before, gone now (only in lists of changes).
    Removed,
    /// The earlier size isn't known: a file below the threshold then and
    /// now, or inside a folder that couldn't be read then or now.
    Unknown,
}

/// How one scanned item changed since the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ItemChange {
    #[ts(type = "number")]
    pub generation: u64,
    pub node: u32,
    pub kind: ChangeKind,
    /// Logical bytes then, when known and present.
    #[ts(type = "number | null")]
    pub before: Option<u64>,
    /// For a file that was below the snapshot's threshold then: that
    /// threshold, which its earlier size was under. It counts as grown by
    /// at least the difference.
    #[ts(type = "number | null")]
    pub before_below: Option<u64>,
    #[ts(type = "number")]
    pub after: u64,
}

/// Asks for the change marks of the nodes in a drawn layout, for the
/// "Change" colors. The binary reply has one byte per node, in order: see
/// [`change_mark`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChangeMarksRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub snapshot: String,
    pub nodes: Vec<u32>,
}

/// Bytes in a change-marks reply.
pub mod change_mark {
    /// Earlier size unknown, or the node isn't in the scan.
    pub const UNKNOWN: u8 = 0;
    pub const UNCHANGED: u8 = 1;
    pub const NEW: u8 = 2;
    /// `GROWN + level`, level 0 (under 5%) to 4 (doubled or more).
    pub const GROWN: u8 = 3;
    /// `SHRUNK + level`, level 0 (under 5%) to 4 (emptied).
    pub const SHRUNK: u8 = 8;
}

/// A page of the biggest changes under a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChangesRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub snapshot: String,
    pub folder: u32,
    /// Leave folders out and list only files.
    pub files_only: bool,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChangeRow {
    /// The item in the scan; `None` when it was removed.
    pub node: Option<u32>,
    /// Its folder in the scan; `None` when removed.
    pub parent: Option<u32>,
    pub name: String,
    pub folder: bool,
    /// The containing folder relative to the requested one ("" when
    /// directly in it).
    pub subfolder: String,
    pub kind: ChangeKind,
    #[ts(type = "number | null")]
    pub before: Option<u64>,
    /// As in [`ItemChange::before_below`].
    #[ts(type = "number | null")]
    pub before_below: Option<u64>,
    #[ts(type = "number | null")]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChangesPage {
    #[ts(type = "number")]
    pub generation: u64,
    pub folder: u32,
    pub offset: u32,
    /// Changed items in all.
    pub total: u32,
    pub rows: Vec<ChangeRow>,
}

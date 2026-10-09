//! Typed, versioned commands and events exchanged between the Rust backend and
//! the webview interface.
//!
//! Every type here derives [`ts_rs::TS`]; `cargo test` regenerates the
//! TypeScript bindings in `ui/src/lib/protocol/`. CI fails if the committed
//! bindings differ from the generated ones.
//!
//! Byte counts and identifiers are `u64` in Rust and `number` in TypeScript.
//! JSON numbers are exact up to 2^53 (8 PiB), which covers any real volume.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub mod wire;

/// Incremented whenever a command or event shape changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 3;

/// Name of the event that carries [`ScanStatus`] updates.
pub const SCAN_STATUS_EVENT: &str = "scan-status";

/// Static information about the running backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AppInfo {
    pub protocol_version: u32,
    pub app_version: String,
    pub os: String,
    pub arch: String,
    /// Scans bypass permission checks because the app was started as
    /// administrator or root. The app never elevates itself.
    pub privileged_access: bool,
}

/// Which size gives treemap area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Metric {
    Allocated,
    Logical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum ScanPhase {
    Scanning,
    Complete,
    CompleteWithOmissions,
    Cancelled,
    Failed { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum OmissionReason {
    Symlink,
    MountPoint,
    SpecialFile,
    UnknownReparse,
    PermissionDenied,
    Vanished,
    Disconnected,
    Unavailable,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct OmissionSample {
    pub path: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct OmissionGroup {
    pub reason: OmissionReason,
    /// Skipped by policy (links, mounts, special files) rather than an error.
    pub policy: bool,
    #[ts(type = "number")]
    pub count: u64,
    pub samples: Vec<OmissionSample>,
}

/// A coalesced snapshot of one scan, emitted as [`SCAN_STATUS_EVENT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ScanStatus {
    #[ts(type = "number")]
    pub generation: u64,
    /// The scan root as displayed to the user.
    pub root: String,
    pub phase: ScanPhase,
    #[ts(type = "number")]
    pub elapsed_ms: u64,
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub dirs: u64,
    #[ts(type = "number")]
    pub logical: u64,
    #[ts(type = "number")]
    pub allocated: u64,
    #[ts(type = "number")]
    pub unknown_allocation_files: u64,
    #[ts(type = "number")]
    pub hardlink_aliases: u64,
    #[ts(type = "number")]
    pub pending_dirs: u64,
    /// Changes whenever the scanned tree changes.
    #[ts(type = "number")]
    pub revision: u64,
    pub omissions: Vec<OmissionGroup>,
}

/// Returned when a scan starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ScanStarted {
    #[ts(type = "number")]
    pub generation: u64,
    pub root: String,
}

/// Asks for the treemap of `view` at the given size. The binary reply is
/// described in [`wire`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LayoutRequest {
    #[ts(type = "number")]
    pub generation: u64,
    /// Echoed in the reply so the interface can drop stale replies.
    pub request: u32,
    pub view: u32,
    pub metric: Metric,
    /// CSS pixels.
    pub width: f32,
    pub height: f32,
    /// Draw only files matching this search (see [`SearchSummary::search`]).
    pub search: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum FolderState {
    /// Still being listed.
    Pending,
    Listed,
    /// Listing failed; contents are unknown.
    Failed,
    /// Not listed because the scan was cancelled.
    NotScanned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Crumb {
    pub node: u32,
    pub name: String,
}

/// Everything the details panel shows about one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct NodeDetails {
    #[ts(type = "number")]
    pub generation: u64,
    pub node: u32,
    pub name: String,
    pub path: String,
    /// `None` for files.
    pub folder: Option<FolderState>,
    #[ts(type = "number")]
    pub logical: u64,
    /// Known allocated bytes. For a file, `None` means unknown.
    #[ts(type = "number | null")]
    pub allocated: Option<u64>,
    #[ts(type = "number")]
    pub unknown_allocation_files: u64,
    #[ts(type = "number")]
    pub files: u64,
    pub cloud: bool,
    pub hardlinked: bool,
    /// For a hard-link alias, the path that owns the allocation.
    pub alias_of: Option<String>,
    /// From the scan root down to and including this node.
    pub ancestors: Vec<Crumb>,
}

/// Totals for the files matching a filename search, across the whole scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchSummary {
    #[ts(type = "number")]
    pub generation: u64,
    /// Identifies this query within the scan; newer queries get larger
    /// numbers. Layout and result requests name it.
    pub search: u32,
    pub query: String,
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub logical: u64,
    /// Known, owned allocation of matching files.
    #[ts(type = "number")]
    pub allocated: u64,
    #[ts(type = "number")]
    pub unknown_allocation_files: u64,
}

/// Asks for a page of matching files, largest first by `metric`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchResultsRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub search: u32,
    pub metric: Metric,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchRow {
    pub node: u32,
    pub name: String,
    /// The containing folder.
    pub parent: u32,
    /// The containing folder's path relative to the scan root; empty at the
    /// root.
    pub folder: String,
    #[ts(type = "number")]
    pub logical: u64,
    /// `None` when unknown. Hard-link aliases report 0 (see `alias`).
    #[ts(type = "number | null")]
    pub allocated: Option<u64>,
    /// Another name of a hard-linked file whose allocation is counted at its
    /// first name.
    pub alias: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SearchPage {
    #[ts(type = "number")]
    pub generation: u64,
    pub search: u32,
    pub offset: u32,
    /// Matching files in all.
    pub total: u32,
    pub rows: Vec<SearchRow>,
}

/// Asks for the items merged into an "other small items" region: the
/// region's folder and item count, as drawn with this metric and search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SmallItemsRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub folder: u32,
    pub count: u32,
    pub metric: Metric,
    pub search: Option<u32>,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SmallItem {
    pub node: u32,
    pub name: String,
    pub folder: bool,
    /// Size in the requested metric (matching bytes when filtered).
    #[ts(type = "number")]
    pub weight: u64,
}

/// Items of a small-items region, largest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SmallItemsPage {
    #[ts(type = "number")]
    pub generation: u64,
    pub folder: u32,
    pub offset: u32,
    pub total: u32,
    pub items: Vec<SmallItem>,
    /// Other children of the folder with no size in this metric, which the
    /// map never draws.
    pub empty: u32,
}

/// Capacity of the volume holding the scan root, for the volume overview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct VolumeInfo {
    #[ts(type = "number")]
    pub generation: u64,
    /// The scan root is the volume's top folder, so the scan covers the
    /// whole volume. The overview is only offered then.
    pub is_root: bool,
    #[ts(type = "number")]
    pub capacity: u64,
    #[ts(type = "number")]
    pub free: u64,
    pub filesystem: Option<String>,
    /// Filesystem structures that could be measured (the NTFS master file
    /// table, when elevated).
    #[ts(type = "number | null")]
    pub metadata: Option<u64>,
}

/// Asks what an action on a set of selected items would touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SelectionRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub nodes: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SelectedItem {
    pub node: u32,
    pub name: String,
    pub path: String,
    pub folder: bool,
    #[ts(type = "number")]
    pub logical: u64,
    /// Known allocated bytes. For a file, `None` means unknown.
    #[ts(type = "number | null")]
    pub allocated: Option<u64>,
    /// Files at or under this item.
    #[ts(type = "number")]
    pub files: u64,
}

/// The items a selection stands for: entries inside another selected folder
/// are counted once, with that folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SelectionSummary {
    #[ts(type = "number")]
    pub generation: u64,
    /// Selected items not inside another selected folder.
    pub items: u32,
    pub folders: u32,
    /// Files in the items, counting each folder's contents.
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub logical: u64,
    #[ts(type = "number")]
    pub allocated: u64,
    #[ts(type = "number")]
    pub unknown_allocation_files: u64,
    /// Entries left out because they're no longer in the scan.
    pub missing: u32,
    /// The largest items, at most [`SELECTION_SAMPLES`].
    pub samples: Vec<SelectedItem>,
}

/// Most items listed in a [`SelectionSummary`].
pub const SELECTION_SAMPLES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DeleteMode {
    /// Move to the Recycle Bin (Windows) or Trash (macOS, Linux).
    Recycle,
    /// Delete without keeping a copy.
    Permanent,
}

/// Deletes selected files and folders (folders with everything in them).
/// Refused while the scan is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DeleteRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub nodes: Vec<u32>,
    pub mode: DeleteMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DeleteFailure {
    pub node: u32,
    pub path: String,
    pub message: String,
}

/// What a delete did. Deleted items are removed from the scan, which moves
/// to a new revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DeleteReport {
    #[ts(type = "number")]
    pub generation: u64,
    pub mode: DeleteMode,
    /// Items now gone from disk and from the scan.
    pub deleted: Vec<u32>,
    /// Of those, items that were already gone before the delete.
    pub already_gone: u32,
    pub failed: Vec<DeleteFailure>,
    /// Totals of the deleted items, as scanned.
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub logical: u64,
    #[ts(type = "number")]
    pub allocated: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_uses_camel_case() {
        let info = AppInfo {
            protocol_version: PROTOCOL_VERSION,
            app_version: "0.1.0".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
            privileged_access: false,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["appVersion"], "0.1.0");
    }

    #[test]
    fn scan_phase_is_tagged() {
        let json = serde_json::to_value(ScanPhase::Failed {
            message: "x".into(),
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"kind": "failed", "message": "x"}));
        let json = serde_json::to_value(ScanPhase::CompleteWithOmissions).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "completeWithOmissions"}));
    }

    #[test]
    fn layout_request_reads_camel_case() {
        let req: LayoutRequest = serde_json::from_value(serde_json::json!({
            "generation": 3, "request": 9, "view": 0, "metric": "logical",
            "width": 640.5, "height": 480, "search": null
        }))
        .unwrap();
        assert_eq!(req.metric, Metric::Logical);
        assert_eq!(req.request, 9);
    }
}

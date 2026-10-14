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

pub mod compare;
pub mod duplicates;
pub mod wire;

pub use compare::*;
pub use duplicates::*;

/// Incremented whenever a command or event shape changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 4;

/// Name of the event that carries [`ScanStatus`] updates.
pub const SCAN_STATUS_EVENT: &str = "scan-status";

/// Name of the event that carries [`FilterUpdated`].
pub const FILTER_UPDATED_EVENT: &str = "filter-updated";

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

/// The filter caught up with a scan status after requests had been
/// answered from it as of an earlier one, and the scan hasn't reported
/// since: what's filtered (totals, map, results) is worth asking again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FilterUpdated {
    #[ts(type = "number")]
    pub generation: u64,
    pub search: u32,
    /// The scan revision the filter now reflects.
    #[ts(type = "number")]
    pub revision: u64,
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

/// The error a query command fails with when a newer request in the same
/// [`Lane`] replaced it before it finished. The interface drops it silently:
/// the newer request's reply is the one wanted.
pub const SUPERSEDED: &str = "superseded by a newer request";

/// Where a query's reply goes, so a newer request makes older ones moot.
/// Each consumer in the interface (the map's layouts, its tooltips, one
/// details panel, one list) has its own `key`, and numbers its requests
/// with `seq` whenever its earlier replies stop mattering. A query with a
/// higher `seq` in the same lane supersedes the lower ones still waiting or
/// running: they fail with [`SUPERSEDED`]. Requests with the same `seq`,
/// such as the pages of one list, never supersede each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Lane {
    pub key: String,
    #[ts(type = "number")]
    pub seq: u64,
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
    /// Listed, but some entries couldn't be read (the listing failed part
    /// way, or entries vanished or couldn't be measured).
    Partial,
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
    /// A file's last-modified time in seconds since the Unix epoch; `None`
    /// for folders, when the filesystem didn't say, and for times before
    /// 1970 or after 2106, which the scan doesn't keep exactly.
    #[ts(type = "number | null")]
    pub modified: Option<u64>,
    /// For a hard-link alias, the path that owns the allocation.
    pub alias_of: Option<String>,
    /// From the scan root down to and including this node.
    pub ancestors: Vec<Crumb>,
    /// What a filter matches at or under this node, when the request named
    /// the current search.
    pub matching: Option<Matching>,
}

/// Files matching a filter at or under a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Matching {
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub logical: u64,
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
    /// The scan revision (see [`ScanStatus::revision`]) the totals were last
    /// fully caught up with. The filter keeps up with a scan in the
    /// background, so while scanning this can trail the latest status.
    #[ts(type = "number")]
    pub revision: u64,
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
    /// The scan revision the items were ranked at, so pages fetched at
    /// different times can be told apart.
    #[ts(type = "number")]
    pub revision: u64,
}

/// Asks for a page of the files anywhere under a folder, largest first by
/// `metric`; with a search, matching files only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FolderFilesRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub folder: u32,
    pub metric: Metric,
    pub search: Option<u32>,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FolderFile {
    pub node: u32,
    pub name: String,
    /// The containing folder.
    pub parent: u32,
    /// The subfolder holding the file, relative to the requested folder;
    /// empty when the file is directly in it.
    pub subfolder: String,
    /// Size in the requested metric.
    #[ts(type = "number")]
    pub weight: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FolderFilesPage {
    #[ts(type = "number")]
    pub generation: u64,
    pub folder: u32,
    pub offset: u32,
    /// Files with a size under the folder, in all.
    pub total: u32,
    pub files: Vec<FolderFile>,
    /// Other files under the folder with no size in this metric.
    pub empty: u32,
    /// The scan revision the files were ranked at.
    #[ts(type = "number")]
    pub revision: u64,
}

/// What the filter's completion is suggesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SuggestKind {
    /// Folder names, for `dir:`.
    Folder,
    /// File extensions, for `*.`.
    Extension,
}

/// Asks for folder names or extensions in the scan that start with or
/// contain `typed`, ignoring case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SuggestRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub kind: SuggestKind,
    pub typed: String,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Suggestion {
    /// A folder name, or an extension without its dot.
    pub name: String,
    /// Folders with this name, or files with this extension.
    pub count: u32,
    /// Logical bytes in them; a folder name counts nested folders of the
    /// same name once.
    #[ts(type = "number")]
    pub logical: u64,
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
    /// Some files in the items have other hard-link names, so deleting
    /// them may free less disk space than they're allocated (none while
    /// another name remains).
    pub hardlinked: bool,
    /// Entries left out because they're no longer in the scan.
    pub missing: u32,
    /// The largest items, at most [`SELECTION_SAMPLES`].
    pub samples: Vec<SelectedItem>,
}

/// Most items listed in a [`SelectionSummary`].
pub const SELECTION_SAMPLES: usize = 8;

/// Moves selected files and folders (folders with everything in them) to
/// the Recycle Bin (Windows) or Trash (macOS, Linux). Nothing is ever
/// deleted permanently. Refused unless deleting was allowed in this run of
/// the app, and while the scan is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DeleteRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub nodes: Vec<u32>,
}

/// What happened to one item a delete was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DeleteOutcome {
    /// Moved to the Recycle Bin or Trash.
    Recycled,
    /// Nothing was there any more when its turn came; nothing was moved.
    AlreadyMissing,
    /// Not the item the scan found (replaced, modified, or now a link), so
    /// it was left alone. A rescan shows it as it is now.
    Changed,
    /// Couldn't be moved; still where it was.
    Failed,
    /// Not confirmed in the Recycle Bin or Trash: some or all of it may
    /// have been deleted without a copy.
    Uncertain,
    /// A duplicate selected itself, kept because it may be the last copy:
    /// it changed since compared, or no copy that stays was confirmed. The
    /// message says which.
    Kept,
}

/// An item a delete didn't simply recycle, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DeleteItem {
    pub node: u32,
    pub path: String,
    pub outcome: DeleteOutcome,
    /// Empty for [`DeleteOutcome::AlreadyMissing`].
    pub message: String,
}

/// What a delete did. Items no longer where they were are removed from
/// the scan, which moves to a new revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DeleteReport {
    #[ts(type = "number")]
    pub generation: u64,
    /// Items moved to the Recycle Bin or Trash.
    pub recycled: Vec<u32>,
    /// Items taken out of the scan: those recycled, already missing, or
    /// uncertain with nothing left at their path.
    pub removed: Vec<u32>,
    /// Every item not recycled, with what happened to it, in the order
    /// tried.
    pub others: Vec<DeleteItem>,
    /// Totals of the recycled items, as scanned.
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub logical: u64,
    #[ts(type = "number")]
    pub allocated: u64,
}

/// Name of the event that carries [`ExportProgress`] updates.
pub const EXPORT_PROGRESS_EVENT: &str = "export-progress";

/// What a CSV export lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ExportScope {
    /// The folder's own files and folders.
    Contents,
    /// Every file and folder at any depth under it.
    Everything,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ExportFormat {
    /// One row per file and folder.
    Csv,
    /// A readable summary: totals and the largest folders and files.
    Report,
}

/// Saves what's under a folder to a file the user chooses in the native
/// save dialog. With `search`, only what that filter matches is exported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ExportRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub folder: u32,
    /// Ignored for a report, which always covers everything under the folder.
    pub scope: ExportScope,
    pub format: ExportFormat,
    pub metric: Metric,
    pub search: Option<u32>,
    /// The name the save dialog suggests.
    pub file_name: String,
    /// When the export was asked for, in the user's local time, for the
    /// report's header.
    pub exported_at: String,
}

/// Sent while an export is written, once the file has been chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ExportProgress {
    #[ts(type = "number")]
    pub generation: u64,
    /// Rows written so far; for a report, items examined.
    #[ts(type = "number")]
    pub rows: u64,
}

/// A finished export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ExportReport {
    #[ts(type = "number")]
    pub generation: u64,
    /// Where the file was saved.
    pub path: String,
    pub format: ExportFormat,
    /// CSV rows written, not counting the header; for a report, the folders
    /// and files it lists.
    #[ts(type = "number")]
    pub rows: u64,
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

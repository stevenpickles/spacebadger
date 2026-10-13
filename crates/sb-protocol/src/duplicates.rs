//! Finding duplicate files: the one request that reads file contents. It
//! runs only when asked, in the background, and never reads cloud files.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Name of the event that carries [`DuplicatesStatus`] updates.
pub const DUPLICATES_STATUS_EVENT: &str = "duplicates-status";

/// Most groups a [`DuplicatesList`] holds; the rest have less space at stake.
pub const DUPLICATE_GROUPS_LISTED: u32 = 5_000;

/// Starts looking for duplicates among the files under `folder` (matching
/// the search, if given) of at least `min_size` bytes, replacing any search
/// for duplicates already running. Refused while the scan runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DuplicatesRequest {
    #[ts(type = "number")]
    pub generation: u64,
    pub folder: u32,
    pub search: Option<u32>,
    #[ts(type = "number")]
    pub min_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DuplicatesPhase {
    Running,
    Complete,
    /// Stopped early; the groups found so far are kept.
    Cancelled,
}

/// Candidate files that weren't read, by reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DuplicateSkips {
    /// Cloud files, which reading could download.
    pub cloud: u32,
    /// Gone since the scan.
    pub vanished: u32,
    /// Changed since the scan (size, or no longer a plain file).
    pub changed: u32,
    /// Stored behind a reparse point (deduplicated, tiered, virtualized),
    /// so reading it raw wouldn't give its contents.
    pub special: u32,
    /// Open in another program that doesn't share it.
    pub in_use: u32,
    pub denied: u32,
    /// Couldn't be read for another reason.
    pub unreadable: u32,
}

/// How a search for duplicates is going, and what it has found so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DuplicatesStatus {
    #[ts(type = "number")]
    pub generation: u64,
    /// Numbers each search for duplicates in a scan.
    pub job: u32,
    pub folder: u32,
    /// The folder's path.
    pub path: String,
    /// The filter the files had to match, if any.
    pub query: Option<String>,
    #[ts(type = "number")]
    pub min_size: u64,
    pub phase: DuplicatesPhase,
    #[ts(type = "number")]
    pub elapsed_ms: u64,
    /// Files sharing their size with another, which are read.
    pub candidates: u32,
    /// Bytes in those files.
    #[ts(type = "number")]
    pub total_bytes: u64,
    /// Of those, bytes dealt with: read, ruled out after a sample, or
    /// skipped. Reaches `total_bytes` when complete.
    #[ts(type = "number")]
    pub done_bytes: u64,
    /// Bytes actually read.
    #[ts(type = "number")]
    pub read_bytes: u64,
    /// Groups of identical files with two or more still in the scan.
    pub groups: u32,
    /// Files in those groups.
    pub files: u32,
    /// Space the copies beyond the first in each group take.
    #[ts(type = "number")]
    pub reclaimable: u64,
    pub skipped: DuplicateSkips,
    /// Bumped whenever the groups change, found or deleted.
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DuplicateFile {
    pub node: u32,
    /// The containing folder.
    pub parent: u32,
    pub path: String,
    /// It has other hard-link names, so deleting this one alone may free
    /// nothing.
    pub hardlinked: bool,
}

/// Files with the same contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DuplicateGroup {
    /// Each file's size.
    #[ts(type = "number")]
    pub size: u64,
    /// Shortest path first.
    pub files: Vec<DuplicateFile>,
}

/// The groups found so far, most reclaimable space first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DuplicatesList {
    #[ts(type = "number")]
    pub generation: u64,
    pub job: u32,
    /// Matches [`DuplicatesStatus::version`].
    pub version: u32,
    /// Groups in all; at most [`DUPLICATE_GROUPS_LISTED`] are listed.
    pub total: u32,
    pub groups: Vec<DuplicateGroup>,
}

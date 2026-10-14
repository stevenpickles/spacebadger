// Typed wrappers around backend commands. Types come from sb-protocol via ts-rs.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { AppInfo } from "./protocol/AppInfo";
import type { ChangeMarksRequest } from "./protocol/ChangeMarksRequest";
import type { ChangesPage } from "./protocol/ChangesPage";
import type { ChangesRequest } from "./protocol/ChangesRequest";
import type { CompareSummary } from "./protocol/CompareSummary";
import type { DeleteReport } from "./protocol/DeleteReport";
import type { DeleteRequest } from "./protocol/DeleteRequest";
import type { DuplicateExtras } from "./protocol/DuplicateExtras";
import type { DuplicateFilesPage } from "./protocol/DuplicateFilesPage";
import type { DuplicateFilesRequest } from "./protocol/DuplicateFilesRequest";
import type { DuplicatesCheck } from "./protocol/DuplicatesCheck";
import type { DuplicatesCheckRequest } from "./protocol/DuplicatesCheckRequest";
import type { DuplicatesList } from "./protocol/DuplicatesList";
import type { DuplicatesRecheck } from "./protocol/DuplicatesRecheck";
import type { DuplicatesRequest } from "./protocol/DuplicatesRequest";
import type { DuplicatesStatus } from "./protocol/DuplicatesStatus";
import type { ExportProgress } from "./protocol/ExportProgress";
import type { ExportReport } from "./protocol/ExportReport";
import type { ExportRequest } from "./protocol/ExportRequest";
import type { FolderFilesPage } from "./protocol/FolderFilesPage";
import type { FolderFilesRequest } from "./protocol/FolderFilesRequest";
import type { ItemChange } from "./protocol/ItemChange";
import type { LayoutRequest } from "./protocol/LayoutRequest";
import type { NodeDetails } from "./protocol/NodeDetails";
import type { ScanStarted } from "./protocol/ScanStarted";
import type { ScanStatus } from "./protocol/ScanStatus";
import type { SearchPage } from "./protocol/SearchPage";
import type { SearchResultsRequest } from "./protocol/SearchResultsRequest";
import type { SearchSummary } from "./protocol/SearchSummary";
import type { SelectionRequest } from "./protocol/SelectionRequest";
import type { SelectionSummary } from "./protocol/SelectionSummary";
import type { SmallItemsPage } from "./protocol/SmallItemsPage";
import type { SmallItemsRequest } from "./protocol/SmallItemsRequest";
import type { SnapshotInfo } from "./protocol/SnapshotInfo";
import type { SuggestRequest } from "./protocol/SuggestRequest";
import type { Suggestion } from "./protocol/Suggestion";
import type { VolumeInfo } from "./protocol/VolumeInfo";

export const EXPECTED_PROTOCOL_VERSION = 4;

/** Must match `sb_protocol::SCAN_STATUS_EVENT`. */
const SCAN_STATUS_EVENT = "scan-status";

/** Must match `sb_protocol::EXPORT_PROGRESS_EVENT`. */
const EXPORT_PROGRESS_EVENT = "export-progress";

export function appInfo(): Promise<AppInfo> {
  return invoke<AppInfo>("app_info");
}

/** Opens the native folder picker and starts scanning; `null` if dismissed. */
export function scanChoose(): Promise<ScanStarted | null> {
  return invoke<ScanStarted | null>("scan_choose");
}

export function scanRefresh(): Promise<ScanStarted | null> {
  return invoke<ScanStarted | null>("scan_refresh");
}

export function scanCancel(generation: number): Promise<void> {
  return invoke("scan_cancel", { generation });
}

export function scanStatus(): Promise<ScanStatus | null> {
  return invoke<ScanStatus | null>("scan_status");
}

/** Binary layout; decode with `decodeLayout`. */
export function requestLayout(request: LayoutRequest): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>("layout", { request });
}

/** With `search`, the details also say what that filter matches there. */
export function nodeDetails(generation: number, node: number, search?: number | null): Promise<NodeDetails> {
  return invoke<NodeDetails>("node_details", { generation, node, search: search ?? null });
}

/**
 * Filters by file name; `null` when the query is empty (filter cleared).
 * Rejects if a newer search replaced this one before it finished.
 */
export function searchSet(generation: number, query: string): Promise<SearchSummary | null> {
  return invoke<SearchSummary | null>("search_set", { generation, query });
}

export function searchSummary(generation: number, search: number): Promise<SearchSummary> {
  return invoke<SearchSummary>("search_summary", { generation, search });
}

export function searchResults(request: SearchResultsRequest): Promise<SearchPage> {
  return invoke<SearchPage>("search_results", { request });
}

export function smallItems(request: SmallItemsRequest): Promise<SmallItemsPage> {
  return invoke<SmallItemsPage>("small_items", { request });
}

/** The largest files anywhere under a folder. */
export function folderFiles(request: FolderFilesRequest): Promise<FolderFilesPage> {
  return invoke<FolderFilesPage>("folder_files", { request });
}

/** Capacity of the scanned volume; `null` where unavailable. */
export function volumeInfo(generation: number): Promise<VolumeInfo | null> {
  return invoke<VolumeInfo | null>("volume_info", { generation });
}

/** Shows the item selected in Explorer, Finder, or the desktop's file manager. */
export function reveal(generation: number, node: number): Promise<void> {
  return invoke("reveal", { generation, node });
}

/**
 * Asks where to save with the native dialog, then writes the export there;
 * `null` if the dialog was dismissed. Progress arrives as events once the
 * file is chosen.
 */
export function exportSave(request: ExportRequest): Promise<ExportReport | null> {
  return invoke<ExportReport | null>("export_save", { request });
}

/** Shows the file last exported selected in the system file manager. */
export function revealExport(): Promise<void> {
  return invoke("reveal_export");
}

export function onExportProgress(handler: (progress: ExportProgress) => void): Promise<UnlistenFn> {
  return listen<ExportProgress>(EXPORT_PROGRESS_EVENT, (event) => handler(event.payload));
}

/** What a set of selected items amounts to. */
export function selectionSummary(request: SelectionRequest): Promise<SelectionSummary> {
  return invoke<SelectionSummary>("selection_summary", { request });
}

/** Turns deleting on or off for this run of the app. It always starts off. */
export function setDeleteAllowed(allowed: boolean): Promise<boolean> {
  return invoke<boolean>("set_delete_allowed", { allowed });
}

/**
 * Asks, then reopens SpaceBadger with administrator rights scanning the same
 * folder; this window closes. Resolves `false` if the user said no. Windows
 * only.
 */
export function restartElevated(): Promise<boolean> {
  return invoke<boolean>("restart_elevated");
}

/**
 * Moves items (folders with their contents) to the Recycle Bin or Trash and
 * removes them from the scan; a status update with a new revision follows.
 * Rejects unless deleting is turned on, and while scanning.
 */
export function deleteItems(request: DeleteRequest): Promise<DeleteReport> {
  return invoke<DeleteReport>("delete_items", { request });
}

/** Where deleted items go, for labels. */
export function trashName(os: string): string {
  return os === "windows" ? "Recycle Bin" : "Trash";
}

/** What the system file manager is called, for menu labels. */
export function fileManagerName(os: string): string {
  if (os === "windows") return "Explorer";
  if (os === "macos") return "Finder";
  return "file manager";
}

export function onScanStatus(handler: (status: ScanStatus) => void): Promise<UnlistenFn> {
  return listen<ScanStatus>(SCAN_STATUS_EVENT, (event) => handler(event.payload));
}

/** A failure from a command or the system, as a sentence for messages. */
export function errorText(e: unknown): string {
  const text = e instanceof Error ? e.message : String(e);
  return text.replace(/^Error:\s*/, "").trim() || "Unknown error";
}

/** Folder names or extensions in the scan, for the filter's completion. */
export function filterSuggest(request: SuggestRequest): Promise<Suggestion[]> {
  return invoke<Suggestion[]>("filter_suggest", { request });
}

// ---- comparing with an earlier scan ------------------------------------

/** Earlier scans of the current root to compare with, newest first. */
export function compareSnapshots(generation: number): Promise<SnapshotInfo[]> {
  return invoke<SnapshotInfo[]>("compare_snapshots", { generation });
}

/** Compares with an earlier scan; `null` turns comparing off. */
export function compareSet(generation: number, snapshot: string | null): Promise<CompareSummary | null> {
  return invoke<CompareSummary | null>("compare_set", { generation, snapshot });
}

export function compareItem(generation: number, snapshot: string, node: number): Promise<ItemChange> {
  return invoke<ItemChange>("compare_item", { generation, snapshot, node });
}

/** One byte per node; see `sb_protocol::change_mark` and `changeColor`. */
export function changeMarks(request: ChangeMarksRequest): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>("change_marks", { request });
}

/** The biggest changes under a folder, including removed items. */
export function compareChanges(request: ChangesRequest): Promise<ChangesPage> {
  return invoke<ChangesPage>("compare_changes", { request });
}

/** Must match `sb_protocol::DUPLICATES_STATUS_EVENT`. */
const DUPLICATES_STATUS_EVENT = "duplicates-status";

/**
 * Starts looking for files with the same contents under a folder, replacing
 * any search already running. This reads the files (never cloud files);
 * progress arrives through `onDuplicatesStatus`. Rejects while scanning.
 */
export function duplicatesStart(request: DuplicatesRequest): Promise<DuplicatesStatus> {
  return invoke<DuplicatesStatus>("duplicates_start", { request });
}

export function duplicatesStatus(generation: number, job: number): Promise<DuplicatesStatus> {
  return invoke<DuplicatesStatus>("duplicates_status", { generation, job });
}

/** The groups of duplicates found so far, most reclaimable space first, each
 * with its first files; the rest come from `duplicateFiles`. */
export function duplicatesList(generation: number, job: number): Promise<DuplicatesList> {
  return invoke<DuplicatesList>("duplicates_list", { generation, job });
}

/** More of a group's files, shortest path first; at most `DUPLICATE_FILES_PAGE`. */
export function duplicateFiles(request: DuplicateFilesRequest): Promise<DuplicateFilesPage> {
  return invoke<DuplicateFilesPage>("duplicate_files", { request });
}

/** Every file but the first of each listed group, by node, to select them. */
export function duplicatesExtras(generation: number, job: number): Promise<DuplicateExtras> {
  return invoke<DuplicateExtras>("duplicates_extras", { generation, job });
}

/** Whether deleting the items would leave a group of duplicates with no copy. */
export function duplicatesCheck(request: DuplicatesCheckRequest): Promise<DuplicatesCheck> {
  return invoke<DuplicatesCheck>("duplicates_check", { request });
}

/** Stops reading; the groups found so far stay. */
export function duplicatesCancel(generation: number, job: number): Promise<void> {
  return invoke("duplicates_cancel", { generation, job });
}

export function onDuplicatesStatus(handler: (status: DuplicatesStatus) => void): Promise<UnlistenFn> {
  return listen<DuplicatesStatus>(DUPLICATES_STATUS_EVENT, (event) => handler(event.payload));
}

/** Must match `sb_protocol::DUPLICATES_RECHECK_EVENT`. */
const DUPLICATES_RECHECK_EVENT = "duplicates-recheck";

/** Progress of a delete reading the copies of duplicates that would stay. */
export function onDuplicatesRecheck(handler: (progress: DuplicatesRecheck) => void): Promise<UnlistenFn> {
  return listen<DuplicatesRecheck>(DUPLICATES_RECHECK_EVENT, (event) => handler(event.payload));
}

/** Stops that reading; files whose copy wasn't confirmed yet are kept. */
export function duplicatesRecheckCancel(generation: number): Promise<void> {
  return invoke("duplicates_recheck_cancel", { generation });
}

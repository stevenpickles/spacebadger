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
import type { FilterUpdated } from "./protocol/FilterUpdated";
import type { FolderFilesPage } from "./protocol/FolderFilesPage";
import type { FolderFilesRequest } from "./protocol/FolderFilesRequest";
import type { GitRootsPage } from "./protocol/GitRootsPage";
import type { GitRootsRequest } from "./protocol/GitRootsRequest";
import type { ItemChange } from "./protocol/ItemChange";
import type { Lane } from "./protocol/Lane";
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
import { enableTrace, timed } from "./metrics";

export const EXPECTED_PROTOCOL_VERSION = 6;

/** Must match `sb_protocol::SCAN_STATUS_EVENT`. */
const SCAN_STATUS_EVENT = "scan-status";

/** Must match `sb_protocol::FILTER_UPDATED_EVENT`. */
const FILTER_UPDATED_EVENT = "filter-updated";

/** Must match `sb_protocol::EXPORT_PROGRESS_EVENT`. */
const EXPORT_PROGRESS_EVENT = "export-progress";

/** Must match `sb_protocol::SUPERSEDED`. */
const SUPERSEDED = "superseded by a newer request";

/**
 * Whether a query failed only because a newer request in its lane replaced
 * it. That's not a failure to show: the newer request's reply is the one
 * wanted, so callers drop it silently (`Latest` does).
 */
export function isSuperseded(e: unknown): boolean {
  return errorText(e) === SUPERSEDED;
}

// The query commands below take an optional `lane` (see `Lane` and
// `Latest`): a newer request in the same lane supersedes older ones still
// waiting or running in the backend, which then reject with `SUPERSEDED`.
// Without one, a request is never superseded.

// Request timings are kept only when the backend keeps its own.
invoke<boolean>("trace_enabled").then(
  (on) => enableTrace(on, (reset) => invoke<string | null>("trace_report", { reset })),
  () => {},
);

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
export function requestLayout(request: LayoutRequest, lane?: Lane): Promise<ArrayBuffer> {
  return timed("layout", invoke<ArrayBuffer>("layout", { request, lane: lane ?? null }));
}

/** With `search`, the details also say what that filter matches there. */
export function nodeDetails(
  generation: number,
  node: number,
  search?: number | null,
  lane?: Lane,
): Promise<NodeDetails> {
  return timed(
    "node_details",
    invoke<NodeDetails>("node_details", { generation, node, search: search ?? null, lane: lane ?? null }),
  );
}

/**
 * Filters by file name; `null` when the query is empty (filter cleared).
 * Rejects if a newer search replaced this one before it finished.
 */
export function searchSet(generation: number, query: string): Promise<SearchSummary | null> {
  return timed("search_set", invoke<SearchSummary | null>("search_set", { generation, query }));
}

export function searchSummary(generation: number, search: number, lane?: Lane): Promise<SearchSummary> {
  return timed("search_summary", invoke<SearchSummary>("search_summary", { generation, search, lane: lane ?? null }));
}

export function searchResults(request: SearchResultsRequest, lane?: Lane): Promise<SearchPage> {
  return timed("search_results", invoke<SearchPage>("search_results", { request, lane: lane ?? null }));
}

export function smallItems(request: SmallItemsRequest, lane?: Lane): Promise<SmallItemsPage> {
  return timed("small_items", invoke<SmallItemsPage>("small_items", { request, lane: lane ?? null }));
}

/** The largest files anywhere under a folder. */
export function folderFiles(request: FolderFilesRequest, lane?: Lane): Promise<FolderFilesPage> {
  return timed("folder_files", invoke<FolderFilesPage>("folder_files", { request, lane: lane ?? null }));
}

/** Git repositories, linked worktrees, and unresolved candidates found in
 * the scan, largest first, with matching sizes when given a search. */
export function gitRoots(request: GitRootsRequest, lane?: Lane): Promise<GitRootsPage> {
  return timed("git_roots", invoke<GitRootsPage>("git_roots", { request, lane: lane ?? null }));
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
export function selectionSummary(request: SelectionRequest, lane?: Lane): Promise<SelectionSummary> {
  return timed("selection_summary", invoke<SelectionSummary>("selection_summary", { request, lane: lane ?? null }));
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

/** The filter caught up after a scan stalled; see `FilterUpdated`. */
export function onFilterUpdated(handler: (updated: FilterUpdated) => void): Promise<UnlistenFn> {
  return listen<FilterUpdated>(FILTER_UPDATED_EVENT, (event) => handler(event.payload));
}

/** A failure from a command or the system, as a sentence for messages. */
export function errorText(e: unknown): string {
  const text = e instanceof Error ? e.message : String(e);
  return text.replace(/^Error:\s*/, "").trim() || "Unknown error";
}

/** Folder names or extensions in the scan, for the filter's completion. */
export function filterSuggest(request: SuggestRequest, lane?: Lane): Promise<Suggestion[]> {
  return timed("filter_suggest", invoke<Suggestion[]>("filter_suggest", { request, lane: lane ?? null }));
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

export function compareItem(generation: number, snapshot: string, node: number, lane?: Lane): Promise<ItemChange> {
  return timed("compare_item", invoke<ItemChange>("compare_item", { generation, snapshot, node, lane: lane ?? null }));
}

/** One byte per node; see `sb_protocol::change_mark` and `changeColor`. */
export function changeMarks(request: ChangeMarksRequest, lane?: Lane): Promise<ArrayBuffer> {
  return timed("change_marks", invoke<ArrayBuffer>("change_marks", { request, lane: lane ?? null }));
}

/** The biggest changes under a folder, including removed items. */
export function compareChanges(request: ChangesRequest, lane?: Lane): Promise<ChangesPage> {
  return timed("compare_changes", invoke<ChangesPage>("compare_changes", { request, lane: lane ?? null }));
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

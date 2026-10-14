//! Desktop bridge: exposes typed commands from `sb-protocol` to the webview.
//!
//! Commands that touch scan data are `async` so they run off the main
//! (UI) thread. Every scan-specific request names its generation; requests
//! for an older generation are refused so stale replies never mix scans.

mod atomic_file;
mod delete;
mod duplicates;
#[cfg(windows)]
mod elevate_windows;
mod identity;
mod metrics;
#[cfg(windows)]
mod recycle_windows;
mod session;
mod snapshots;

use sb_protocol::{
    AppInfo, DUPLICATES_RECHECK_EVENT, DUPLICATES_STATUS_EVENT, DeleteReport, DeleteRequest,
    DuplicateExtras, DuplicateFilesPage, DuplicateFilesRequest, DuplicatesCheck,
    DuplicatesCheckRequest, DuplicatesList, DuplicatesRequest, DuplicatesStatus,
    EXPORT_PROGRESS_EVENT, ExportFormat, ExportProgress, ExportReport, ExportRequest,
    FolderFilesPage, FolderFilesRequest, LayoutRequest, NodeDetails, PROTOCOL_VERSION,
    SCAN_STATUS_EVENT, ScanStarted, ScanStatus, SearchPage, SearchResultsRequest, SearchSummary,
    SelectionRequest, SelectionSummary, SmallItemsPage, SmallItemsRequest, SuggestRequest,
    Suggestion, VolumeInfo,
};
use session::Session;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Arc<Session>>>,
    /// Deleting was turned on in this run of the app. Never saved, so every
    /// start begins with it off.
    delete_allowed: AtomicBool,
    /// The file last exported, which "Show in Explorer" can reveal.
    last_export: Mutex<Option<PathBuf>>,
    /// Where snapshots of completed scans are saved, for comparing later
    /// scans with; `None` if the app has no data folder.
    snapshots: Option<Arc<snapshots::Store>>,
}

impl AppState {
    fn current(&self) -> Option<Arc<Session>> {
        self.session.lock().unwrap().clone()
    }

    /// The current session if it belongs to `generation`.
    fn session(&self, generation: u64) -> Result<Arc<Session>, String> {
        self.current()
            .filter(|s| s.generation() == generation)
            .ok_or_else(|| format!("scan {generation} is no longer current"))
    }

    /// Replaces the current scan; dropping the old session cancels it.
    fn start(&self, app: &AppHandle, root: PathBuf) -> ScanStarted {
        // `spacebadger .` names the folder it was started in; snapshots are
        // kept by absolute path, so another folder's never match.
        let root = std::path::absolute(&root).unwrap_or(root);
        // Rescanning the same folder goes on comparing with the same
        // earlier scan, held in memory in case its file is pruned.
        let carried = self
            .current()
            .filter(|s| s.root == root)
            .and_then(|s| s.carry());
        let emitter = app.clone();
        let session = Arc::new(Session::start_with(
            root,
            self.snapshots.clone(),
            carried,
            move |status| {
                let _ = emitter.emit(SCAN_STATUS_EVENT, status);
            },
        ));
        let started = session.started();
        *self.session.lock().unwrap() = Some(session);
        started
    }
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        protocol_version: PROTOCOL_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        privileged_access: sb_scan::native::privileged_access(),
    }
}

/// Lets the user choose a folder or drive with the native dialog and starts
/// scanning it. Returns `None` if the dialog was dismissed.
#[tauri::command]
async fn scan_choose(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<ScanStarted>, String> {
    let picked = app
        .dialog()
        .file()
        .set_parent(&window)
        .set_title("Choose a folder or drive to scan")
        .blocking_pick_folder();
    let Some(picked) = picked else {
        return Ok(None);
    };
    let root = picked.into_path().map_err(|e| e.to_string())?;
    Ok(Some(state.start(&app, root)))
}

/// Scans the current root again as a new generation.
#[tauri::command]
async fn scan_refresh(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ScanStarted>, String> {
    let Some(root) = state.current().map(|s| s.root.clone()) else {
        return Ok(None);
    };
    Ok(Some(state.start(&app, root)))
}

#[tauri::command]
async fn scan_cancel(state: State<'_, AppState>, generation: u64) -> Result<(), String> {
    state.session(generation)?.scan.cancel();
    Ok(())
}

/// The latest status of the current scan, for an interface that (re)loads
/// after events were sent.
#[tauri::command]
async fn scan_status(state: State<'_, AppState>) -> Result<Option<ScanStatus>, String> {
    Ok(state.current().map(|s| s.status()))
}

/// The treemap for one view, as the binary format in `sb_protocol::wire`.
#[tauri::command]
async fn layout(state: State<'_, AppState>, request: LayoutRequest) -> Result<Response, String> {
    let session = state.session(request.generation)?;
    Ok(Response::new(metrics::run("layout", || {
        session.layout(&request)
    })?))
}

/// Details of a scanned item; with `search`, also what that filter matches
/// at or under it.
#[tauri::command]
async fn node_details(
    state: State<'_, AppState>,
    generation: u64,
    node: u32,
    search: Option<u32>,
) -> Result<NodeDetails, String> {
    let session = state.session(generation)?;
    metrics::run("node_details", || session.details(node, search))
}

/// Filters by file name; an empty query clears the filter and returns `None`.
/// Large trees take a moment, so this runs on a blocking thread; a newer
/// call makes an older one return an error.
#[tauri::command]
async fn search_set(
    state: State<'_, AppState>,
    generation: u64,
    query: String,
) -> Result<Option<SearchSummary>, String> {
    let session = state.session(generation)?;
    metrics::blocking("search_set", move || session.search_set(&query)).await?
}

#[tauri::command]
async fn search_summary(
    state: State<'_, AppState>,
    generation: u64,
    search: u32,
) -> Result<SearchSummary, String> {
    let session = state.session(generation)?;
    metrics::run("search_summary", || session.search_summary(search))
}

#[tauri::command]
async fn search_results(
    state: State<'_, AppState>,
    request: SearchResultsRequest,
) -> Result<SearchPage, String> {
    let session = state.session(request.generation)?;
    metrics::run("search_results", || session.search_results(&request))
}

/// The largest files anywhere under a folder. Ranking a large folder takes a
/// moment, so this runs on a blocking thread.
#[tauri::command]
async fn folder_files(
    state: State<'_, AppState>,
    request: FolderFilesRequest,
) -> Result<FolderFilesPage, String> {
    let session = state.session(request.generation)?;
    metrics::blocking("folder_files", move || session.folder_files(&request)).await?
}

/// Folder names or extensions from the scan for the filter's completion.
/// The first call after a change indexes the whole tree, so this runs on a
/// blocking thread.
#[tauri::command]
async fn filter_suggest(
    state: State<'_, AppState>,
    request: SuggestRequest,
) -> Result<Vec<Suggestion>, String> {
    let session = state.session(request.generation)?;
    metrics::blocking("filter_suggest", move || session.suggest(&request)).await?
}

/// The files and folders merged into an "other small items" region.
#[tauri::command]
async fn small_items(
    state: State<'_, AppState>,
    request: SmallItemsRequest,
) -> Result<SmallItemsPage, String> {
    let session = state.session(request.generation)?;
    metrics::run("small_items", || session.small_items(&request))
}

/// Capacity and free space of the scanned volume; `None` where unavailable.
#[tauri::command]
async fn volume_info(
    state: State<'_, AppState>,
    generation: u64,
) -> Result<Option<VolumeInfo>, String> {
    let session = state.session(generation)?;
    tauri::async_runtime::spawn_blocking(move || session.volume())
        .await
        .map_err(|e| e.to_string())
}

/// What a set of selected items amounts to, for the details panel and the
/// delete confirmation.
#[tauri::command]
async fn selection_summary(
    state: State<'_, AppState>,
    request: SelectionRequest,
) -> Result<SelectionSummary, String> {
    let session = state.session(request.generation)?;
    metrics::run("selection_summary", || session.selection(&request.nodes))
}

/// Starts looking for duplicate files under a folder, the one command that
/// reads file contents (never of cloud files). Progress and the final
/// status arrive as events. Planning walks the folder, so this runs on a
/// blocking thread.
#[tauri::command]
async fn duplicates_start(
    app: AppHandle,
    state: State<'_, AppState>,
    request: DuplicatesRequest,
) -> Result<DuplicatesStatus, String> {
    let session = state.session(request.generation)?;
    tauri::async_runtime::spawn_blocking(move || {
        session.duplicates_start(&request, move |status| {
            let _ = app.emit(DUPLICATES_STATUS_EVENT, status);
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn duplicates_status(
    state: State<'_, AppState>,
    generation: u64,
    job: u32,
) -> Result<DuplicatesStatus, String> {
    state.session(generation)?.duplicates_status(job)
}

/// The groups of duplicates found so far, with the first few files of each.
/// Builds their paths, so this runs on a blocking thread.
#[tauri::command]
async fn duplicates_list(
    state: State<'_, AppState>,
    generation: u64,
    job: u32,
) -> Result<DuplicatesList, String> {
    let session = state.session(generation)?;
    tauri::async_runtime::spawn_blocking(move || session.duplicates_list(job))
        .await
        .map_err(|e| e.to_string())?
}

/// More of a group's files. The first page asked for orders the group by
/// path, so this runs on a blocking thread.
#[tauri::command]
async fn duplicate_files(
    state: State<'_, AppState>,
    request: DuplicateFilesRequest,
) -> Result<DuplicateFilesPage, String> {
    let session = state.session(request.generation)?;
    tauri::async_runtime::spawn_blocking(move || session.duplicate_files(&request))
        .await
        .map_err(|e| e.to_string())?
}

/// Every file but the first of each listed group, by node, for selecting
/// them. Orders the groups by path, so this runs on a blocking thread.
#[tauri::command]
async fn duplicates_extras(
    state: State<'_, AppState>,
    generation: u64,
    job: u32,
) -> Result<DuplicateExtras, String> {
    let session = state.session(generation)?;
    tauri::async_runtime::spawn_blocking(move || session.duplicates_extras(job))
        .await
        .map_err(|e| e.to_string())?
}

/// Whether deleting the given items would leave a group of duplicates with
/// no copy. Looks at the files that would stay on disk, so this runs on a
/// blocking thread.
#[tauri::command]
async fn duplicates_check(
    state: State<'_, AppState>,
    request: DuplicatesCheckRequest,
) -> Result<DuplicatesCheck, String> {
    let session = state.session(request.generation)?;
    tauri::async_runtime::spawn_blocking(move || {
        session.duplicates_check(request.job, &request.nodes)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn duplicates_cancel(
    state: State<'_, AppState>,
    generation: u64,
    job: u32,
) -> Result<(), String> {
    state.session(generation)?.duplicates_cancel(job)
}

/// Earlier scans of the current root that it can be compared with, newest
/// first. Reads each snapshot's header, so this runs on a blocking thread.
#[tauri::command]
async fn compare_snapshots(
    state: State<'_, AppState>,
    generation: u64,
) -> Result<Vec<sb_protocol::SnapshotInfo>, String> {
    let session = state.session(generation)?;
    tauri::async_runtime::spawn_blocking(move || session.snapshots())
        .await
        .map_err(|e| e.to_string())
}

/// Compares the scan with an earlier one, or with `None` stops comparing.
/// Reading a large snapshot and matching it takes a moment, so this runs on
/// a blocking thread.
#[tauri::command]
async fn compare_set(
    state: State<'_, AppState>,
    generation: u64,
    snapshot: Option<String>,
) -> Result<Option<sb_protocol::CompareSummary>, String> {
    let session = state.session(generation)?;
    tauri::async_runtime::spawn_blocking(move || session.compare_set(snapshot.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}

/// How one item changed since the earlier scan. After a delete the scan
/// is matched again first, so this runs on a blocking thread.
#[tauri::command]
async fn compare_item(
    state: State<'_, AppState>,
    generation: u64,
    snapshot: String,
    node: u32,
) -> Result<sb_protocol::ItemChange, String> {
    let session = state.session(generation)?;
    metrics::blocking("compare_item", move || {
        session.compare_item(&snapshot, node)
    })
    .await?
}

/// One byte per node of a drawn layout, for the "Change" colors. Runs on a
/// blocking thread, like `compare_item`.
#[tauri::command]
async fn change_marks(
    state: State<'_, AppState>,
    request: sb_protocol::ChangeMarksRequest,
) -> Result<Response, String> {
    let session = state.session(request.generation)?;
    let marks = metrics::blocking("change_marks", move || session.change_marks(&request)).await??;
    Ok(Response::new(marks))
}

/// The biggest changes under a folder since the earlier scan. The first
/// page walks the folder, so this runs on a blocking thread.
#[tauri::command]
async fn compare_changes(
    state: State<'_, AppState>,
    request: sb_protocol::ChangesRequest,
) -> Result<sb_protocol::ChangesPage, String> {
    let session = state.session(request.generation)?;
    metrics::blocking("compare_changes", move || session.changes(&request)).await?
}

/// Whether request timings are kept (`SPACEBADGER_TRACE` is set), so the
/// interface keeps its own too.
#[tauri::command]
fn trace_enabled() -> bool {
    metrics::enabled()
}

/// The request timings so far as a text table, then forgotten if `reset`;
/// `None` when they aren't kept. For diagnosing slow interactions.
#[tauri::command]
fn trace_report(reset: bool) -> Option<String> {
    let report = metrics::report();
    if reset {
        metrics::reset();
    }
    report
}

/// Asks first, then starts SpaceBadger again with administrator rights,
/// scanning the same folder, and closes this copy. Returns `false` if the
/// user said no, here or at the Windows elevation prompt.
#[tauri::command]
async fn restart_elevated(app: AppHandle, state: State<'_, AppState>) -> Result<bool, String> {
    if !cfg!(windows) {
        return Err("Restarting as administrator is only available on Windows.".to_owned());
    }
    let session = state.current();
    if session.as_ref().is_some_and(|s| s.is_deleting()) {
        return Err("Wait for the move to the Recycle Bin to finish first.".to_owned());
    }
    let root = session.map(|s| s.root.clone());
    let again = match &root {
        Some(r) => format!(" and scan {} again", r.display()),
        None => String::new(),
    };
    let ok = app
        .dialog()
        .message(format!(
            "SpaceBadger will close and reopen with administrator rights{again}. \
             Windows asks for permission first."
        ))
        .title("Restart as administrator?")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Restart".to_owned(),
            "Cancel".to_owned(),
        ))
        .blocking_show();
    if !ok {
        return Ok(false);
    }
    #[cfg(windows)]
    if !elevate_windows::relaunch_elevated(root.as_deref())? {
        return Ok(false);
    }
    app.exit(0);
    Ok(true)
}

/// Turns deleting on or off for this run of the app; returns the new state.
#[tauri::command]
fn set_delete_allowed(state: State<'_, AppState>, allowed: bool) -> bool {
    state.delete_allowed.store(allowed, Ordering::SeqCst);
    allowed
}

/// Moves selected items (folders with their contents) to the Recycle Bin or
/// Trash and removes them from the scan. Refused unless deleting is turned
/// on. Runs on a blocking thread: large folders take a while, and copies of
/// duplicates that would stay are read first, with progress as events.
#[tauri::command]
async fn delete_items(
    app: AppHandle,
    state: State<'_, AppState>,
    request: DeleteRequest,
) -> Result<DeleteReport, String> {
    if !state.delete_allowed.load(Ordering::SeqCst) {
        return Err("Deleting is turned off. Turn on “Allow deleting” first.".into());
    }
    let session = state.session(request.generation)?;
    tauri::async_runtime::spawn_blocking(move || {
        session.delete(&request.nodes, &|progress| {
            let _ = app.emit(DUPLICATES_RECHECK_EVENT, progress);
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stops a delete reading copies of duplicates that would stay; files whose
/// copy wasn't confirmed are kept, the rest still move.
#[tauri::command]
async fn duplicates_recheck_cancel(
    state: State<'_, AppState>,
    generation: u64,
) -> Result<(), String> {
    state.session(generation)?.duplicates_recheck_cancel();
    Ok(())
}

/// Shows a scanned file or folder selected in the system file manager
/// (Explorer, Finder, or the desktop's file manager).
#[tauri::command]
async fn reveal(state: State<'_, AppState>, generation: u64, node: u32) -> Result<(), String> {
    let path = state.session(generation)?.path(node)?;
    tauri::async_runtime::spawn_blocking(move || reveal_path(&path))
        .await
        .map_err(|e| e.to_string())?
}

/// Asks where to save, then exports what's under a folder there as CSV or
/// a report. Returns `None` if the save dialog was dismissed. Writing runs
/// on a blocking thread and reports its progress as events; it stops if a
/// new scan replaces this one meanwhile.
#[tauri::command]
async fn export_save(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    request: ExportRequest,
) -> Result<Option<ExportReport>, String> {
    let generation = request.generation;
    // Not held across the dialog, so a scan started meanwhile isn't kept alive.
    state.session(generation)?;
    let (kind, extension) = match request.format {
        ExportFormat::Csv => ("CSV", "csv"),
        ExportFormat::Report => ("Text", "txt"),
    };
    let picked = app
        .dialog()
        .file()
        .set_parent(&window)
        .set_title("Export")
        .set_file_name(Session::export_file_name(
            &request.file_name,
            request.format,
        ))
        .add_filter(kind, &[extension])
        .blocking_save_file();
    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    let session = state
        .session(generation)
        .map_err(|_| "A new scan started, so nothing was exported.".to_owned())?;
    let emitter = app.clone();
    let target = path.clone();
    let report = tauri::async_runtime::spawn_blocking(move || {
        let current = || {
            emitter
                .state::<AppState>()
                .current()
                .is_some_and(|s| s.generation() == generation)
        };
        session.export(&request, &target, current, |rows| {
            let _ = emitter.emit(EXPORT_PROGRESS_EVENT, ExportProgress { generation, rows });
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    *state.last_export.lock().unwrap() = Some(path);
    Ok(Some(report))
}

/// Shows the file last exported selected in the system file manager.
#[tauri::command]
async fn reveal_export(state: State<'_, AppState>) -> Result<(), String> {
    let path = state
        .last_export
        .lock()
        .unwrap()
        .clone()
        .ok_or("Nothing has been exported yet.")?;
    tauri::async_runtime::spawn_blocking(move || reveal_path(&path))
        .await
        .map_err(|e| e.to_string())?
}

fn reveal_path(path: &Path) -> Result<(), String> {
    let shown = session::display_path(path);
    // The item may have been moved or deleted since it was scanned.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(format!(
                "{shown} no longer exists. Refresh to update the map."
            ));
        }
        Err(e) => return Err(format!("Couldn't reach {shown}: {e}")),
    };
    // A folder opens in the file manager, so its contents show. Anything
    // else is shown selected in its folder and never opened: opening a file
    // would run it or open it in an app. That includes links and junctions
    // (`is_dir` is false for them), which would be followed, and macOS
    // packages such as apps, which are folders that Finder launches.
    // Both use the platform API with the path as an argument, never a shell.
    if meta.is_dir() && !may_be_package(path) {
        return tauri_plugin_opener::open_path(path, None::<&str>)
            .map_err(|e| format!("Couldn't open {shown} in the file manager: {e}"));
    }
    tauri_plugin_opener::reveal_item_in_dir(path)
        .map_err(|e| format!("Couldn't show {shown} in the file manager: {e}"))
}

/// On macOS, a folder with an extension (`Safari.app`, `Photos Library.photoslibrary`)
/// may be a package that opening would launch, so it's shown selected instead.
fn may_be_package(path: &Path) -> bool {
    cfg!(target_os = "macos") && path.extension().is_some()
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let state = AppState {
                snapshots: app
                    .path()
                    .app_local_data_dir()
                    .ok()
                    .map(|dir| Arc::new(snapshots::Store::new(dir.join("snapshots")))),
                ..AppState::default()
            };
            // `spacebadger <folder>` starts scanning that folder right away.
            if let Some(root) = std::env::args_os().nth(1) {
                state.start(app.handle(), PathBuf::from(root));
            }
            app.manage(state);
            // "SpaceBadger v0.1.0" for a release; builds between releases
            // carry "+dev" and the commit (see build.rs).
            if let Some(window) = app.get_webview_window("main") {
                window.set_title(&format!("SpaceBadger {}", env!("SPACEBADGER_BUILD")))?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            scan_choose,
            scan_refresh,
            scan_cancel,
            scan_status,
            layout,
            node_details,
            reveal,
            export_save,
            reveal_export,
            search_set,
            search_summary,
            search_results,
            small_items,
            folder_files,
            filter_suggest,
            volume_info,
            selection_summary,
            duplicates_start,
            duplicates_status,
            duplicates_list,
            duplicate_files,
            duplicates_extras,
            duplicates_check,
            duplicates_cancel,
            duplicates_recheck_cancel,
            set_delete_allowed,
            restart_elevated,
            delete_items,
            compare_snapshots,
            compare_set,
            compare_item,
            change_marks,
            compare_changes,
            trace_enabled,
            trace_report
        ])
        .build(tauri::generate_context!())
        .expect("error while running SpaceBadger")
        .run(|_, event| {
            if let tauri::RunEvent::Exit = event {
                metrics::write_report();
            }
        });
}

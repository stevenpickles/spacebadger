//! Desktop bridge: exposes typed commands from `sb-protocol` to the webview.
//!
//! Commands that touch scan data are `async` so they run off the main
//! (UI) thread. Every scan-specific request names its generation; requests
//! for an older generation are refused so stale replies never mix scans.

mod delete;
mod duplicates;
#[cfg(windows)]
mod elevate_windows;
#[cfg(windows)]
mod recycle_windows;
mod session;

use sb_protocol::{
    AppInfo, DUPLICATES_STATUS_EVENT, DeleteReport, DeleteRequest, DuplicatesList,
    DuplicatesRequest, DuplicatesStatus, FolderFilesPage, FolderFilesRequest, LayoutRequest,
    NodeDetails, PROTOCOL_VERSION, SCAN_STATUS_EVENT, ScanStarted, ScanStatus, SearchPage,
    SearchResultsRequest, SearchSummary, SelectionRequest, SelectionSummary, SmallItemsPage,
    SmallItemsRequest, SuggestRequest, Suggestion, VolumeInfo,
};
use session::Session;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Arc<Session>>>,
    /// Deleting was turned on in this run of the app. Never saved, so every
    /// start begins with it off.
    delete_allowed: AtomicBool,
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
        let emitter = app.clone();
        let session = Arc::new(Session::start(root, move |status| {
            let _ = emitter.emit(SCAN_STATUS_EVENT, status);
        }));
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
    state: State<'_, AppState>,
) -> Result<Option<ScanStarted>, String> {
    let picked = app
        .dialog()
        .file()
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
    Ok(Response::new(session.layout(&request)?))
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
    state.session(generation)?.details(node, search)
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
    tauri::async_runtime::spawn_blocking(move || session.search_set(&query))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn search_summary(
    state: State<'_, AppState>,
    generation: u64,
    search: u32,
) -> Result<SearchSummary, String> {
    state.session(generation)?.search_summary(search)
}

#[tauri::command]
async fn search_results(
    state: State<'_, AppState>,
    request: SearchResultsRequest,
) -> Result<SearchPage, String> {
    state.session(request.generation)?.search_results(&request)
}

/// The largest files anywhere under a folder. Ranking a large folder takes a
/// moment, so this runs on a blocking thread.
#[tauri::command]
async fn folder_files(
    state: State<'_, AppState>,
    request: FolderFilesRequest,
) -> Result<FolderFilesPage, String> {
    let session = state.session(request.generation)?;
    tauri::async_runtime::spawn_blocking(move || session.folder_files(&request))
        .await
        .map_err(|e| e.to_string())?
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
    tauri::async_runtime::spawn_blocking(move || session.suggest(&request))
        .await
        .map_err(|e| e.to_string())?
}

/// The files and folders merged into an "other small items" region.
#[tauri::command]
async fn small_items(
    state: State<'_, AppState>,
    request: SmallItemsRequest,
) -> Result<SmallItemsPage, String> {
    state.session(request.generation)?.small_items(&request)
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
    state.session(request.generation)?.selection(&request.nodes)
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

/// The groups of duplicates found so far. Builds every listed path, so this
/// runs on a blocking thread.
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

#[tauri::command]
async fn duplicates_cancel(
    state: State<'_, AppState>,
    generation: u64,
    job: u32,
) -> Result<(), String> {
    state.session(generation)?.duplicates_cancel(job)
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
/// on. Runs on a blocking thread: large folders take a while.
#[tauri::command]
async fn delete_items(
    state: State<'_, AppState>,
    request: DeleteRequest,
) -> Result<DeleteReport, String> {
    if !state.delete_allowed.load(Ordering::SeqCst) {
        return Err("Deleting is turned off. Turn on “Allow deleting” first.".into());
    }
    let session = state.session(request.generation)?;
    tauri::async_runtime::spawn_blocking(move || session.delete(&request.nodes))
        .await
        .map_err(|e| e.to_string())?
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
            let state = AppState::default();
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
            duplicates_cancel,
            set_delete_allowed,
            restart_elevated,
            delete_items
        ])
        .run(tauri::generate_context!())
        .expect("error while running SpaceBadger");
}

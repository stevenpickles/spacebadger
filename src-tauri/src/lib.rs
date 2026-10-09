//! Desktop bridge: exposes typed commands from `sb-protocol` to the webview.
//!
//! Commands that touch scan data are `async` so they run off the main
//! (UI) thread. Every scan-specific request names its generation; requests
//! for an older generation are refused so stale replies never mix scans.

mod session;

use sb_protocol::{
    AppInfo, LayoutRequest, NodeDetails, PROTOCOL_VERSION, SCAN_STATUS_EVENT, ScanStarted,
    ScanStatus,
};
use session::Session;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Arc<Session>>>,
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

#[tauri::command]
async fn node_details(
    state: State<'_, AppState>,
    generation: u64,
    node: u32,
) -> Result<NodeDetails, String> {
    state.session(generation)?.details(node)
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
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(format!(
                "{shown} no longer exists. Refresh to update the map."
            ));
        }
        Err(e) => return Err(format!("Couldn't reach {shown}: {e}")),
    }
    // Uses the platform API with the path as an argument, never a shell.
    tauri_plugin_opener::reveal_item_in_dir(path)
        .map_err(|e| format!("Couldn't show {shown} in the file manager: {e}"))
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
            reveal
        ])
        .run(tauri::generate_context!())
        .expect("error while running SpaceBadger");
}

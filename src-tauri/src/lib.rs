//! Desktop bridge: exposes typed commands from `sb-protocol` to the webview.

use sb_protocol::{AppInfo, PROTOCOL_VERSION};

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        protocol_version: PROTOCOL_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    }
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_info])
        .run(tauri::generate_context!())
        .expect("error while running SpaceBadger");
}

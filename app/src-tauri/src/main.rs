#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use lifeboat_core::ipc::{client, Request, Response};

/// Ask the service for its status and return a one-line description.
#[tauri::command]
fn service_status() -> Result<String, String> {
    match client::request(&Request::Status).map_err(|e| e.to_string())? {
        Response::Status { version, frozen } => Ok(format!(
            "Service running (v{version}){}",
            if frozen { ", backups frozen" } else { "" }
        )),
        Response::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![service_status])
        .run(tauri::generate_context!())
        .expect("error while running Lifeboat");
}

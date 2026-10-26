#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod admin;
mod folders;
mod pipe;
mod target;

use lifeboat_core::disk::{DiskInfo, PrepareMode};
use lifeboat_core::ipc::{FileEntry, SnapshotInfo, VaultItem, PIPE_NAME};
use pipe::{ApiError, StatusInfo};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;

/// How often the app asks the service whether backups got frozen.
const POLL_EVERY: Duration = Duration::from_secs(30);

/// Run a blocking pipe call off the UI thread.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::other(e.to_string()))?
}

fn folders_file(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|d| d.join("folders.json"))
        .map_err(|e| e.to_string())
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Should the "backups paused" notification fire? Once per freeze, including a freeze already
/// present when the app starts.
fn should_notify_frozen(previous: Option<bool>, now: bool) -> bool {
    now && previous != Some(true)
}

#[derive(Serialize)]
struct HomeState {
    status: StatusInfo,
    folders: Vec<String>,
}

#[tauri::command]
async fn home_state() -> Result<HomeState, ApiError> {
    blocking(|| {
        let status = pipe::status(PIPE_NAME)?;
        // No repository yet simply means no folders yet.
        let folders = pipe::folders(PIPE_NAME).unwrap_or_default();
        Ok(HomeState { status, folders })
    })
    .await
}

/// Back up now, then tell the user. Used by the button and the tray.
async fn run_backup(app: AppHandle) -> Result<(), ApiError> {
    let r = blocking(|| pipe::backup(PIPE_NAME)).await;
    match &r {
        Ok(_) => {
            notify(&app, "Backup finished", "Your files are safe.");
            let _ = app.emit("backup-done", ());
        }
        Err(e) => notify(&app, "Backup did not work", &e.message),
    }
    r.map(|_| ())
}

#[tauri::command]
async fn backup_now(app: AppHandle) -> Result<(), ApiError> {
    run_backup(app).await
}

// --- drive statistics (read-only) ---
#[tauri::command]
async fn drive_stats() -> Result<pipe::DriveStatsInfo, ApiError> {
    blocking(|| pipe::drive_stats(PIPE_NAME)).await
}
// --- end drive statistics ---

#[tauri::command]
async fn list_snapshots() -> Result<Vec<SnapshotInfo>, ApiError> {
    blocking(|| pipe::snapshots(PIPE_NAME)).await
}

/// Restore `paths` (empty = the whole version). Returns the folder the files landed in.
#[tauri::command]
async fn restore(
    snapshot: String,
    target: String,
    password: String,
    paths: Vec<String>,
) -> Result<String, ApiError> {
    if password.is_empty() {
        return Err(ApiError::other("Please type your Lifeboat password."));
    }
    target::check_restore_target(&target).map_err(ApiError::other)?;
    blocking(move || pipe::restore(PIPE_NAME, &snapshot, &target, &password, paths)).await
}

/// Queue backups for deletion. They are removed after the service's delay (7 days).
#[tauri::command]
async fn delete_snapshots(password: String, ids: Vec<String>) -> Result<(), ApiError> {
    if password.is_empty() {
        return Err(ApiError::other(
            "Please type your Lifeboat password or recovery key.",
        ));
    }
    if ids.is_empty() {
        return Err(ApiError::other("Please choose a backup to delete."));
    }
    blocking(move || pipe::delete_snapshots(PIPE_NAME, &password, ids)).await
}

/// Queue vault items for removal. They go after the service's delay (7 days).
#[tauri::command]
async fn vault_remove(password: String, paths: Vec<String>) -> Result<(), ApiError> {
    if password.is_empty() {
        return Err(ApiError::other(
            "Please type your Lifeboat password or recovery key.",
        ));
    }
    if paths.is_empty() {
        return Err(ApiError::other("Please choose something to remove."));
    }
    blocking(move || pipe::vault_remove(PIPE_NAME, &password, paths)).await
}

/// How big the Lifeboat partition is, and how far it can grow.
#[tauri::command]
async fn lifeboat_size_info() -> Result<pipe::SizeInfo, ApiError> {
    blocking(move || pipe::lifeboat_size_info(PIPE_NAME)).await
}

/// Grow the Lifeboat partition. Needs the password.
#[tauri::command]
async fn resize_lifeboat(password: String, new_size: u64) -> Result<u64, ApiError> {
    if password.is_empty() {
        return Err(ApiError::other("Please type your Lifeboat password."));
    }
    blocking(move || pipe::resize_lifeboat(PIPE_NAME, &password, new_size)).await
}

/// Delete Lifeboat for good. Needs the password and the word DELETE.
#[tauri::command]
async fn remove_lifeboat(password: String, confirm: String) -> Result<String, ApiError> {
    if password.is_empty() {
        return Err(ApiError::other(
            "Please type your Lifeboat password or recovery key.",
        ));
    }
    blocking(move || pipe::remove_lifeboat(PIPE_NAME, &password, &confirm)).await
}

/// Cancel every queued delete.
#[tauri::command]
async fn cancel_delete(password: String) -> Result<(), ApiError> {
    if password.is_empty() {
        return Err(ApiError::other(
            "Please type your Lifeboat password or recovery key.",
        ));
    }
    blocking(move || pipe::cancel_delete(PIPE_NAME, &password)).await
}

#[tauri::command]
async fn list_files(snapshot: String, path: Option<String>) -> Result<Vec<FileEntry>, ApiError> {
    blocking(move || pipe::list_files(PIPE_NAME, &snapshot, path)).await
}

#[tauri::command]
async fn vault_add(password: String, paths: Vec<String>) -> Result<u32, ApiError> {
    if password.is_empty() {
        return Err(ApiError::other("Please type your Lifeboat password."));
    }
    if paths.is_empty() {
        return Err(ApiError::other("Please choose what to put in the vault."));
    }
    blocking(move || pipe::vault_add(PIPE_NAME, &password, paths)).await
}

#[tauri::command]
async fn vault_list() -> Result<Vec<VaultItem>, ApiError> {
    blocking(|| pipe::vault_list(PIPE_NAME)).await
}

#[tauri::command]
async fn vault_extract(
    password: String,
    paths: Vec<String>,
    target: String,
) -> Result<String, ApiError> {
    if password.is_empty() {
        return Err(ApiError::other("Please type your Lifeboat password."));
    }
    target::check_restore_target(&target).map_err(ApiError::other)?;
    blocking(move || pipe::vault_extract(PIPE_NAME, &password, paths, &target)).await
}

/// Show a folder in Explorer. The path is one argument, never a shell string.
#[tauri::command]
fn open_folder(path: String) -> Result<(), ApiError> {
    if !std::path::Path::new(&path).is_dir() {
        return Err(ApiError::other("That folder could not be found."));
    }
    std::process::Command::new("explorer.exe")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|_| ApiError::other("Could not open the folder."))
}

#[tauri::command]
fn is_admin() -> bool {
    admin::is_admin()
}

/// Start a new elevated copy of Lifeboat and quit this one. If the user says No in the
/// Windows prompt, this stays open and returns a friendly message.
#[tauri::command]
fn relaunch_as_admin(app: AppHandle) -> Result<(), ApiError> {
    admin::relaunch_as_admin().map_err(ApiError::other)?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn list_disks() -> Result<Vec<DiskInfo>, ApiError> {
    blocking(|| pipe::disks(PIPE_NAME)).await
}

/// `internal_drive_letter` and `internal_size_gb` are set only for an internal disk.
#[tauri::command]
async fn prepare_disk(
    disk: u32,
    internal_drive_letter: Option<String>,
    internal_size_gb: Option<u64>,
    password: String,
) -> Result<String, ApiError> {
    if password.len() < 8 {
        return Err(ApiError::other(
            "Please choose a password with at least 8 characters.",
        ));
    }
    let mode = match (internal_drive_letter, internal_size_gb) {
        (None, None) => PrepareMode::External,
        (Some(l), Some(gb)) if gb > 0 => PrepareMode::InternalShrink {
            drive_letter: l
                .chars()
                .next()
                .ok_or_else(|| ApiError::other("Missing drive letter."))?
                .to_ascii_uppercase(),
            size_bytes: gb.saturating_mul(1024 * 1024 * 1024),
        },
        _ => return Err(ApiError::other("Please say how much space to set aside.")),
    };
    blocking(move || pipe::prepare(PIPE_NAME, disk, mode, &password)).await
}

fn same_folder(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[tauri::command]
async fn add_folder(folder: String) -> Result<Vec<String>, ApiError> {
    let folder = folder.trim().to_string();
    blocking(move || {
        let mut list = pipe::folders(PIPE_NAME)?;
        if !folder.is_empty() && !list.iter().any(|f| same_folder(f, &folder)) {
            list.push(folder);
            pipe::set_folders(PIPE_NAME, list.clone(), None)?;
        }
        Ok(list)
    })
    .await
}

/// Removing a folder needs the Lifeboat password.
#[tauri::command]
async fn remove_folder(folder: String, password: String) -> Result<Vec<String>, ApiError> {
    if password.is_empty() {
        return Err(ApiError::other("Please type your Lifeboat password."));
    }
    blocking(move || {
        let mut list = pipe::folders(PIPE_NAME)?;
        list.retain(|f| !same_folder(f, &folder));
        pipe::set_folders(PIPE_NAME, list.clone(), Some(password))?;
        Ok(list)
    })
    .await
}

/// Second setup step: create the repository. Returns the recovery key, to be shown once.
#[tauri::command]
async fn init_repo(
    password: String,
    folders: Vec<String>,
    fresh: Option<bool>,
) -> Result<String, ApiError> {
    if password.len() < 8 {
        return Err(ApiError::other(
            "Please choose a password with at least 8 characters.",
        ));
    }
    blocking(move || pipe::init_repo(PIPE_NAME, &password, folders, fresh.unwrap_or(false))).await
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Lifeboat", true, None::<&str>)?;
    let backup = MenuItem::with_id(app, "backup", "Back up now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &backup, &quit])?;
    let mut builder = TrayIconBuilder::new()
        .tooltip("Lifeboat")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "backup" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = run_backup(app).await;
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// Ask the service for its status every 30 seconds; notify when backups get frozen.
fn spawn_poller(app: AppHandle) {
    std::thread::spawn(move || {
        let mut previous: Option<bool> = None;
        let mut old_file = folders_file(&app).ok();
        loop {
            if let Some(file) = &old_file {
                let r = folders::migrate(
                    file,
                    || pipe::folders(PIPE_NAME).ok(),
                    |l| pipe::set_folders(PIPE_NAME, l, None).is_ok(),
                );
                if r != folders::Migration::Retry {
                    old_file = None;
                }
            }
            match pipe::status(PIPE_NAME) {
                Ok(s) => {
                    if should_notify_frozen(previous, s.frozen) {
                        notify(
                            &app,
                            "Lifeboat paused your backups",
                            "Possible ransomware detected. Open Lifeboat to see what to do.",
                        );
                    }
                    previous = Some(s.frozen);
                    let _ = app.emit("status-changed", s);
                }
                Err(_) => {
                    let _ = app.emit("service-down", ());
                }
            }
            std::thread::sleep(POLL_EVERY);
        }
    });
}

fn main() {
    // After "Restart as administrator" the old copy is still closing: wait for it first.
    if let Some(pid) = admin::relaunched_from(&std::env::args().collect::<Vec<_>>()) {
        admin::wait_for_exit(pid, Duration::from_secs(5));
    }
    tauri::Builder::default()
        // A second launch only brings the first window forward.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app)
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            setup_tray(app.handle())?;
            spawn_poller(app.handle().clone());
            Ok(())
        })
        // Closing the window keeps Lifeboat running in the tray.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            home_state,
            backup_now,
            list_snapshots,
            drive_stats,
            restore,
            delete_snapshots,
            cancel_delete,
            vault_remove,
            lifeboat_size_info,
            resize_lifeboat,
            remove_lifeboat,
            list_disks,
            prepare_disk,
            add_folder,
            remove_folder,
            init_repo,
            list_files,
            vault_add,
            vault_list,
            vault_extract,
            open_folder,
            is_admin,
            relaunch_as_admin
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lifeboat");
}

#[cfg(test)]
mod tests {
    use super::should_notify_frozen;

    #[test]
    fn frozen_notification_fires_once_per_freeze() {
        assert!(should_notify_frozen(None, true));
        assert!(should_notify_frozen(Some(false), true));
        assert!(!should_notify_frozen(Some(true), true));
        assert!(!should_notify_frozen(Some(false), false));
        assert!(!should_notify_frozen(None, false));
    }
}

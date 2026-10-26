//! Typed calls to the Lifeboat service over its named pipe.
//!
//! Every function takes the pipe name so tests can talk to a fake server.

use lifeboat_core::config::{PendingDelete, PendingVaultRemove};
use lifeboat_core::disk::{DiskInfo, PrepareMode};
use lifeboat_core::ipc::{client, FileEntry, Request, Response, SnapshotInfo, VaultItem};
use serde::Serialize;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The background service is not running (the pipe does not exist).
    NotRunning,
    /// The service answered with an error.
    Service,
    /// Anything else (no permission, broken reply, ...).
    Other,
}

/// Error shown to the user. `message` is already plain words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ApiError {
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Other,
            message: message.into(),
        }
    }
}

impl From<io::Error> for ApiError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Self {
                kind: ErrorKind::NotRunning,
                message: "Lifeboat's background service isn't running.".into(),
            },
            io::ErrorKind::PermissionDenied => {
                Self::other("Windows did not let Lifeboat talk to its background service.")
            }
            _ => Self::other(format!("Could not talk to the background service: {e}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusInfo {
    pub version: String,
    pub frozen: bool,
    pub frozen_since: Option<String>,
    /// Unix seconds of the last successful backup cycle.
    pub last_run: Option<u64>,
    /// Backups queued for deletion (id and due time).
    pub pending_deletes: Vec<PendingDelete>,
    /// Vault items queued for removal (path and due time).
    pub pending_vault_removes: Vec<PendingVaultRemove>,
}

fn call(pipe: &str, req: &Request) -> Result<Response, ApiError> {
    match client::request_on(pipe, req)? {
        Response::Error { message } => Err(ApiError {
            kind: ErrorKind::Service,
            message: friendly(&message),
        }),
        other => Ok(other),
    }
}

/// Turn the service's short refusals into plain words for the user.
pub fn friendly(message: &str) -> String {
    if message == "password wrong" {
        "That password is not right. Please try again.".into()
    } else if message == "needs administrator" {
        "This step needs administrator permission. Press \"Restart as administrator\" and try again.".into()
    } else if let Some(m) = message
        .strip_prefix("locked for ")
        .and_then(|r| r.strip_suffix(" minutes"))
    {
        format!("Too many wrong passwords. Please wait {m} minutes and try again.")
    } else if let Some(r) = message.strip_prefix("cannot check password") {
        format!("Lifeboat could not check the password{r}")
    } else {
        message.to_string()
    }
}

fn unexpected(r: Response) -> ApiError {
    ApiError::other(format!("Unexpected answer from the service: {r:?}"))
}

pub fn status(pipe: &str) -> Result<StatusInfo, ApiError> {
    match call(pipe, &Request::Status)? {
        Response::Status {
            version,
            frozen,
            frozen_since,
            last_run,
            pending_deletes,
            pending_vault_removes,
            ..
        } => Ok(StatusInfo {
            version,
            frozen,
            frozen_since,
            last_run,
            pending_deletes,
            pending_vault_removes,
        }),
        r => Err(unexpected(r)),
    }
}

/// Returns the new snapshot id.
pub fn backup(pipe: &str) -> Result<String, ApiError> {
    match call(pipe, &Request::Backup)? {
        Response::BackupDone { snapshot } => Ok(snapshot),
        r => Err(unexpected(r)),
    }
}

// --- drive statistics (read-only) ---
/// What the Drives card shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveStatsInfo {
    pub disks: Vec<lifeboat_core::drive::DriveDisk>,
    pub lifeboat: Option<lifeboat_core::drive::LifeboatDrive>,
}

pub fn drive_stats(pipe: &str) -> Result<DriveStatsInfo, ApiError> {
    match call(pipe, &Request::DriveStats)? {
        Response::DriveStats { disks, lifeboat } => Ok(DriveStatsInfo { disks, lifeboat }),
        r => Err(unexpected(r)),
    }
}
// --- end drive statistics ---

pub fn snapshots(pipe: &str) -> Result<Vec<SnapshotInfo>, ApiError> {
    match call(pipe, &Request::ListSnapshots)? {
        Response::Snapshots { snapshots } => Ok(snapshots),
        r => Err(unexpected(r)),
    }
}

/// Restore `paths` (empty = the whole version) into `target`. Returns the folder the files landed in.
pub fn restore(
    pipe: &str,
    snapshot: &str,
    target: &str,
    password: &str,
    paths: Vec<String>,
) -> Result<String, ApiError> {
    let req = Request::Restore {
        password: password.to_string(),
        snapshot: snapshot.to_string(),
        target: target.to_string(),
        paths,
    };
    match call(pipe, &req)? {
        Response::Restored { folder } => Ok(folder),
        r => Err(unexpected(r)),
    }
}

/// Queue backups for deletion; the service removes them after its delay (7 days).
pub fn delete_snapshots(pipe: &str, password: &str, ids: Vec<String>) -> Result<(), ApiError> {
    let req = Request::DeleteSnapshots {
        password: password.to_string(),
        ids,
    };
    match call(pipe, &req)? {
        Response::Done => Ok(()),
        r => Err(unexpected(r)),
    }
}

/// Queue vault items for removal; the service removes them after its delay (7 days).
pub fn vault_remove(pipe: &str, password: &str, paths: Vec<String>) -> Result<(), ApiError> {
    let req = Request::VaultRemove {
        password: password.to_string(),
        paths,
    };
    match call(pipe, &req)? {
        Response::Done => Ok(()),
        r => Err(unexpected(r)),
    }
}

/// How big the Lifeboat partition is and how far it can grow (#54).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SizeInfo {
    pub total: u64,
    pub used: Option<u64>,
    pub max_size: u64,
    pub can_grow: bool,
    pub reason: String,
}

pub fn lifeboat_size_info(pipe: &str) -> Result<SizeInfo, ApiError> {
    match call(pipe, &Request::LifeboatSizeInfo)? {
        Response::ResizeInfo {
            total,
            used,
            max_size,
            can_grow,
            reason,
        } => Ok(SizeInfo {
            total,
            used,
            max_size,
            can_grow,
            reason,
        }),
        r => Err(unexpected(r)),
    }
}

/// Grow the Lifeboat partition to `new_size` bytes. Returns the size it now has.
pub fn resize_lifeboat(pipe: &str, password: &str, new_size: u64) -> Result<u64, ApiError> {
    let req = Request::ResizeLifeboat {
        password: password.to_string(),
        new_size,
    };
    match call(pipe, &req)? {
        Response::Resized { new_size } => Ok(new_size),
        r => Err(unexpected(r)),
    }
}

/// Delete the Lifeboat partition for good. `confirm` must be the word DELETE. Returns a note
/// for the user (empty when everything went fine).
pub fn remove_lifeboat(pipe: &str, password: &str, confirm: &str) -> Result<String, ApiError> {
    let req = Request::RemoveLifeboat {
        password: password.to_string(),
        confirm: confirm.to_string(),
    };
    match call(pipe, &req)? {
        Response::Removed { note } => Ok(note),
        r => Err(unexpected(r)),
    }
}

/// Cancel every queued delete.
pub fn cancel_delete(pipe: &str, password: &str) -> Result<(), ApiError> {
    let req = Request::CancelDelete {
        password: password.to_string(),
    };
    match call(pipe, &req)? {
        Response::Done => Ok(()),
        r => Err(unexpected(r)),
    }
}

/// List one folder inside a version (`path` None = the top).
pub fn list_files(
    pipe: &str,
    snapshot: &str,
    path: Option<String>,
) -> Result<Vec<FileEntry>, ApiError> {
    let req = Request::ListFiles {
        snapshot: snapshot.to_string(),
        path,
    };
    match call(pipe, &req)? {
        Response::Files { entries } => Ok(entries),
        r => Err(unexpected(r)),
    }
}

/// Move files into the vault. Returns how many items were moved.
pub fn vault_add(pipe: &str, password: &str, paths: Vec<String>) -> Result<u32, ApiError> {
    let req = Request::VaultAdd {
        password: password.to_string(),
        paths,
    };
    match call(pipe, &req)? {
        Response::VaultAdded { moved, .. } => Ok(moved),
        r => Err(unexpected(r)),
    }
}

pub fn vault_list(pipe: &str) -> Result<Vec<VaultItem>, ApiError> {
    match call(pipe, &Request::VaultList)? {
        Response::Vault { items } => Ok(items),
        r => Err(unexpected(r)),
    }
}

/// Take items out of the vault into `target`. Returns the folder they landed in.
pub fn vault_extract(
    pipe: &str,
    password: &str,
    paths: Vec<String>,
    target: &str,
) -> Result<String, ApiError> {
    let req = Request::VaultExtract {
        password: password.to_string(),
        paths,
        target: target.to_string(),
    };
    match call(pipe, &req)? {
        Response::Restored { folder } => Ok(folder),
        r => Err(unexpected(r)),
    }
}

pub fn disks(pipe: &str) -> Result<Vec<DiskInfo>, ApiError> {
    match call(pipe, &Request::ListDisks)? {
        Response::Disks { disks } => Ok(disks),
        r => Err(unexpected(r)),
    }
}

pub fn prepare(
    pipe: &str,
    disk: u32,
    mode: PrepareMode,
    password: &str,
) -> Result<String, ApiError> {
    let req = Request::PrepareDisk {
        password: password.to_string(),
        disk,
        mode,
    };
    match call(pipe, &req)? {
        Response::Prepared { volume_path, .. } => Ok(volume_path),
        r => Err(unexpected(r)),
    }
}

/// The folders the service backs up.
pub fn folders(pipe: &str) -> Result<Vec<String>, ApiError> {
    match call(pipe, &Request::ListFolders)? {
        Response::Folders { folders } => Ok(folders),
        r => Err(unexpected(r)),
    }
}

/// Replace the folder list. `password` is needed when the change removes a folder.
pub fn set_folders(
    pipe: &str,
    folders: Vec<String>,
    password: Option<String>,
) -> Result<(), ApiError> {
    match call(pipe, &Request::SetFolders { folders, password })? {
        Response::Done => Ok(()),
        r => Err(unexpected(r)),
    }
}

/// First setup: create the repository. Returns the recovery key (shown once, never stored).
pub fn init_repo(
    pipe: &str,
    password: &str,
    folders: Vec<String>,
    fresh: bool,
) -> Result<String, ApiError> {
    let req = Request::InitRepo {
        password: password.to_string(),
        folders,
        fresh,
    };
    match call(pipe, &req)? {
        Response::RepoInitialized { recovery_key } => Ok(recovery_key),
        r => Err(unexpected(r)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    /// Fake service: answers `n` requests with `handler`.
    fn fake_server(
        name: &str,
        n: usize,
        handler: fn(Request) -> Response,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let pipe = format!(r"\\.\pipe\lifeboat-ui-test-{}-{name}", std::process::id());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)
            .unwrap();
        let p = pipe.clone();
        let h = tokio::spawn(async move {
            for _ in 0..n {
                server.connect().await.unwrap();
                let mut conn = server;
                server = ServerOptions::new().create(&p).unwrap();
                let mut line = String::new();
                BufReader::new(&mut conn)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let req: Request = serde_json::from_str(line.trim()).unwrap();
                let mut out = serde_json::to_string(&handler(req)).unwrap();
                out.push('\n');
                conn.write_all(out.as_bytes()).await.unwrap();
                conn.flush().await.unwrap();
            }
        });
        (pipe, h)
    }

    async fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::task::spawn_blocking(f).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_with_frozen_since() {
        let (pipe, h) = fake_server("status", 1, |req| {
            assert_eq!(req, Request::Status);
            Response::Status {
                version: "1.2.3".into(),
                frozen: true,
                frozen_since: Some("2026-10-24T10:00:00Z".into()),
                freeze_reasons: vec![],
                same_physical_disk: false,
                last_run: Some(1_700_000_000),
                last_backup: Some(1_700_000_000),
                last_check: None,
                pending_deletes: vec![PendingDelete {
                    id: "abc".into(),
                    due: 1_700_600_000,
                }],
                pending_vault_removes: vec![PendingVaultRemove {
                    path: "C:/Secret".into(),
                    snapshot: String::new(),
                    due: 1_700_700_000,
                }],
            }
        });
        let s = on_thread(move || status(&pipe)).await.unwrap();
        assert_eq!(s.pending_vault_removes.len(), 1);
        assert_eq!(s.pending_vault_removes[0].due, 1_700_700_000);
        assert_eq!(s.pending_deletes.len(), 1);
        assert_eq!(s.pending_deletes[0].due, 1_700_600_000);
        assert_eq!(s.last_run, Some(1_700_000_000));
        assert!(s.frozen);
        assert_eq!(s.frozen_since.as_deref(), Some("2026-10-24T10:00:00Z"));
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backup_and_snapshots() {
        let (pipe, h) = fake_server("snap", 2, |req| match req {
            Request::Backup => Response::BackupDone {
                snapshot: "abc".into(),
            },
            Request::ListSnapshots => Response::Snapshots {
                snapshots: vec![SnapshotInfo {
                    id: "abc".into(),
                    short_id: "abc".into(),
                    time: "2026-10-24T09:00:00Z".into(),
                    paths: vec![],
                    hostname: String::new(),
                }],
            },
            other => panic!("unexpected {other:?}"),
        });
        let p2 = pipe.clone();
        let id = on_thread(move || backup(&pipe)).await.unwrap();
        assert_eq!(id, "abc");
        let list = on_thread(move || snapshots(&p2)).await.unwrap();
        assert_eq!(list.len(), 1);
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_sends_snapshot_and_target() {
        let (pipe, h) = fake_server("restore", 1, |req| match req {
            Request::Restore {
                snapshot,
                target,
                password,
                paths,
            } => {
                assert_eq!(password, "pw");
                assert_eq!(snapshot, "abc");
                assert_eq!(target, r"D:\Back");
                assert_eq!(paths, vec!["/Docs".to_string()]);
                Response::Restored {
                    folder: r"D:\Back".into(),
                }
            }
            other => panic!("unexpected {other:?}"),
        });
        let folder =
            on_thread(move || restore(&pipe, "abc", r"D:\Back", "pw", vec!["/Docs".into()]))
                .await
                .unwrap();
        assert_eq!(folder, r"D:\Back");
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_snapshots_sends_ids_and_password() {
        let (pipe, h) = fake_server("delete", 1, |req| match req {
            Request::DeleteSnapshots { password, ids } => {
                assert_eq!(password, "pw");
                assert_eq!(ids, vec!["abc".to_string(), "def".to_string()]);
                Response::Done
            }
            other => panic!("unexpected {other:?}"),
        });
        on_thread(move || delete_snapshots(&pipe, "pw", vec!["abc".into(), "def".into()]))
            .await
            .unwrap();
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn vault_remove_sends_paths_and_shows_refusal() {
        let (pipe, h) = fake_server("vaultrm", 2, |req| match req {
            Request::VaultRemove { password, paths } if password == "pw" => {
                assert_eq!(paths, vec![r"C:\Secret".to_string()]);
                Response::Done
            }
            Request::VaultRemove { .. } => Response::Error {
                message: "password wrong".into(),
            },
            other => panic!("unexpected {other:?}"),
        });
        let p2 = pipe.clone();
        on_thread(move || vault_remove(&pipe, "pw", vec![r"C:\Secret".into()]))
            .await
            .unwrap();
        let err = on_thread(move || vault_remove(&p2, "bad", vec![r"C:\Secret".into()]))
            .await
            .unwrap_err();
        assert_eq!(err.message, "That password is not right. Please try again.");
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resize_and_remove_roundtrip() {
        let (pipe, h) = fake_server("resizerm", 4, |req| match req {
            Request::LifeboatSizeInfo => Response::ResizeInfo {
                total: 10,
                used: Some(4),
                max_size: 30,
                can_grow: true,
                reason: String::new(),
            },
            Request::ResizeLifeboat { new_size, .. } => Response::Resized { new_size },
            Request::RemoveLifeboat { confirm, .. } if confirm == "DELETE" => Response::Removed {
                note: "space note".into(),
            },
            Request::RemoveLifeboat { .. } => Response::Error {
                message: "type DELETE to confirm deleting Lifeboat".into(),
            },
            other => panic!("unexpected {other:?}"),
        });
        let (p1, p2, p3) = (pipe.clone(), pipe.clone(), pipe.clone());
        let info = on_thread(move || lifeboat_size_info(&pipe)).await.unwrap();
        assert_eq!(info.max_size, 30);
        assert_eq!(info.used, Some(4));
        let n = on_thread(move || resize_lifeboat(&p1, "pw", 20))
            .await
            .unwrap();
        assert_eq!(n, 20);
        let note = on_thread(move || remove_lifeboat(&p2, "pw", "DELETE"))
            .await
            .unwrap();
        assert_eq!(note, "space note");
        let err = on_thread(move || remove_lifeboat(&p3, "pw", "nope"))
            .await
            .unwrap_err();
        assert!(err.message.contains("DELETE"));
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_delete_sends_password_and_shows_refusal() {
        let (pipe, h) = fake_server("canceldel", 2, |req| match req {
            Request::CancelDelete { password } if password == "pw" => Response::Done,
            Request::CancelDelete { .. } => Response::Error {
                message: "password wrong".into(),
            },
            other => panic!("unexpected {other:?}"),
        });
        let p2 = pipe.clone();
        on_thread(move || cancel_delete(&pipe, "pw")).await.unwrap();
        let err = on_thread(move || cancel_delete(&p2, "bad"))
            .await
            .unwrap_err();
        assert_eq!(err.message, "That password is not right. Please try again.");
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_files_maps_entries() {
        let (pipe, h) = fake_server("files", 1, |req| match req {
            Request::ListFiles { snapshot, path } => {
                assert_eq!(snapshot, "abc");
                assert_eq!(path.as_deref(), Some("/Docs"));
                Response::Files {
                    entries: vec![FileEntry {
                        path: "/Docs/a.txt".into(),
                        name: "a.txt".into(),
                        dir: false,
                        size: 12,
                        mtime: "2026-10-24T09:00:00Z".into(),
                    }],
                }
            }
            other => panic!("unexpected {other:?}"),
        });
        let list = on_thread(move || list_files(&pipe, "abc", Some("/Docs".into())))
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "a.txt");
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn vault_calls_map_requests() {
        let (pipe, h) = fake_server("vault", 3, |req| match req {
            Request::VaultAdd { password, paths } => {
                assert_eq!(password, "pw");
                assert_eq!(paths, vec![r"C:\Secret".to_string()]);
                Response::VaultAdded {
                    snapshot: "s1".into(),
                    moved: 4,
                }
            }
            Request::VaultList => Response::Vault {
                items: vec![VaultItem {
                    path: "Secret".into(),
                    dir: true,
                    size: 99,
                    added: "2026-10-24T09:00:00Z".into(),
                    snapshot: "s1".into(),
                    removes_at: None,
                }],
            },
            Request::VaultExtract {
                password,
                paths,
                target,
            } => {
                assert_eq!(password, "pw");
                assert_eq!(paths, vec!["Secret".to_string()]);
                Response::Restored { folder: target }
            }
            other => panic!("unexpected {other:?}"),
        });
        let (p2, p3) = (pipe.clone(), pipe.clone());
        let moved = on_thread(move || vault_add(&pipe, "pw", vec![r"C:\Secret".into()]))
            .await
            .unwrap();
        assert_eq!(moved, 4);
        let items = on_thread(move || vault_list(&p2)).await.unwrap();
        assert_eq!(items[0].size, 99);
        let folder = on_thread(move || vault_extract(&p3, "pw", vec!["Secret".into()], r"D:\Out"))
            .await
            .unwrap();
        assert_eq!(folder, r"D:\Out");
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn service_error_is_mapped() {
        let (pipe, h) = fake_server("err", 1, |_| Response::Error {
            message: "wrong password".into(),
        });
        let e = on_thread(move || backup(&pipe)).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Service);
        assert_eq!(e.message, "wrong password");
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn folders_and_set_folders() {
        let (pipe, h) = fake_server("folders", 3, |req| match req {
            Request::ListFolders => Response::Folders {
                folders: vec!["C:/A".into()],
            },
            Request::SetFolders { folders, password } => {
                assert_eq!(folders, vec!["C:/B".to_string()]);
                match password.as_deref() {
                    Some("pw") => Response::Done,
                    _ => Response::Error {
                        message: "password wrong".into(),
                    },
                }
            }
            other => panic!("unexpected {other:?}"),
        });
        let (p2, p3) = (pipe.clone(), pipe.clone());
        let l = on_thread(move || folders(&pipe)).await.unwrap();
        assert_eq!(l, vec!["C:/A".to_string()]);
        let e = on_thread(move || set_folders(&p2, vec!["C:/B".into()], None))
            .await
            .unwrap_err();
        assert!(e.message.contains("not right"));
        on_thread(move || set_folders(&p3, vec!["C:/B".into()], Some("pw".into())))
            .await
            .unwrap();
        h.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn init_repo_returns_key() {
        let (pipe, h) = fake_server("init", 1, |req| match req {
            Request::InitRepo {
                password,
                folders,
                fresh,
            } => {
                assert!(!fresh);
                assert_eq!(password, "pw");
                assert_eq!(folders, vec!["C:/A".to_string()]);
                Response::RepoInitialized {
                    recovery_key: "KEY".into(),
                }
            }
            other => panic!("unexpected {other:?}"),
        });
        let k = on_thread(move || init_repo(&pipe, "pw", vec!["C:/A".into()], false))
            .await
            .unwrap();
        assert_eq!(k, "KEY");
        h.await.unwrap();
    }

    #[test]
    fn refusals_become_plain_words() {
        assert!(friendly("password wrong").contains("not right"));
        assert!(friendly("locked for 15 minutes").contains("15 minutes"));
        assert!(friendly("needs administrator").contains("Restart as administrator"));
        assert!(friendly("cannot check password: x").starts_with("Lifeboat could not check"));
        assert_eq!(friendly("other"), "other");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_reply_is_other_error() {
        let (pipe, h) = fake_server("wrong", 1, |_| Response::Pong);
        let e = on_thread(move || status(&pipe)).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Other);
        h.await.unwrap();
    }

    #[test]
    fn missing_pipe_means_service_not_running() {
        let e = status(r"\\.\pipe\lifeboat-ui-test-does-not-exist").unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotRunning);
        assert!(e.message.contains("isn't running"));
    }
}

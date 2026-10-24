//! Typed calls to the Lifeboat service over its named pipe.
//!
//! Every function takes the pipe name so tests can talk to a fake server.

use lifeboat_core::disk::{DiskInfo, PrepareMode};
use lifeboat_core::ipc::{client, Request, Response, SnapshotInfo};
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
        "This step needs administrator permission. Close Lifeboat, then open it again with \"Run as administrator\" and repeat the setup.".into()
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
        } => Ok(StatusInfo {
            version,
            frozen,
            frozen_since,
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

pub fn snapshots(pipe: &str) -> Result<Vec<SnapshotInfo>, ApiError> {
    match call(pipe, &Request::ListSnapshots)? {
        Response::Snapshots { snapshots } => Ok(snapshots),
        r => Err(unexpected(r)),
    }
}

pub fn restore(pipe: &str, snapshot: &str, target: &str, password: &str) -> Result<(), ApiError> {
    let req = Request::Restore {
        password: password.to_string(),
        snapshot: snapshot.to_string(),
        target: target.to_string(),
        path: None,
    };
    match call(pipe, &req)? {
        Response::Restored => Ok(()),
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
            }
        });
        let s = on_thread(move || status(&pipe)).await.unwrap();
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
                ..
            } => {
                assert_eq!(password, "pw");
                assert_eq!(snapshot, "abc");
                assert_eq!(target, r"D:\Back");
                Response::Restored
            }
            other => panic!("unexpected {other:?}"),
        });
        on_thread(move || restore(&pipe, "abc", r"D:\Back", "pw"))
            .await
            .unwrap();
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

    #[test]
    fn refusals_become_plain_words() {
        assert!(friendly("password wrong").contains("not right"));
        assert!(friendly("locked for 15 minutes").contains("15 minutes"));
        assert!(friendly("needs administrator").contains("Run as administrator"));
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

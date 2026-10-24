//! IPC protocol between the UI and the service: one JSON object per line over
//! the named pipe [`PIPE_NAME`].

use crate::config::{CheckResult, PendingDelete};
use crate::disk::{DiskInfo, PrepareMode};
use serde::{Deserialize, Serialize};

/// Name of the named pipe the service listens on.
pub const PIPE_NAME: &str = r"\\.\pipe\lifeboat";

/// A command sent to the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Request {
    Ping,
    Status,
    /// Back up all configured folders now.
    Backup,
    ListSnapshots,
    /// Restore a snapshot (or only `path` from it) into the new folder `target`.
    /// Dangerous: needs the repository `password`.
    Restore {
        password: String,
        snapshot: String,
        target: String,
        path: Option<String>,
    },
    /// List the disks the machine has.
    ListDisks,
    /// Create the hidden Lifeboat partition on `disk`. Dangerous: needs the repository
    /// `password` and an elevated administrator on the other end of the pipe.
    PrepareDisk {
        password: String,
        disk: u32,
        mode: PrepareMode,
    },
    /// First setup only: create the repository with `password`, add a recovery key, plant
    /// canaries in `folders`. Needs an elevated administrator; refused when a repo exists.
    InitRepo {
        password: String,
        folders: Vec<String>,
    },
    /// Queue snapshot deletes; they run after a delay (7 days). Needs the password.
    DeleteSnapshots {
        password: String,
        ids: Vec<String>,
    },
    /// Cancel every queued delete. Needs the password.
    CancelDelete {
        password: String,
    },
    /// Clear the freeze after an alert. Needs the password.
    Unfreeze {
        password: String,
    },
    /// The folders the service backs up.
    ListFolders,
    /// Replace the folder list. Adding folders needs no password; any change that removes a
    /// folder needs `password` (malware must not be able to silently stop backups).
    SetFolders {
        folders: Vec<String>,
        #[serde(default)]
        password: Option<String>,
    },
}

/// A snapshot as shown to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: String,
    pub short_id: String,
    pub time: String,
    pub paths: Vec<String>,
}

/// The service's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply")]
pub enum Response {
    Pong,
    Status {
        /// Service version.
        version: String,
        /// Whether backups are currently frozen.
        frozen: bool,
        /// When the freeze started (RFC 3339), if frozen and known.
        #[serde(default)]
        frozen_since: Option<String>,
        /// Why backups were frozen.
        #[serde(default)]
        freeze_reasons: Vec<String>,
        /// The backup volume shares a physical disk with a backed-up folder.
        #[serde(default)]
        same_physical_disk: bool,
        /// Unix seconds of the last successful backup cycle.
        #[serde(default)]
        last_run: Option<u64>,
        /// Unix seconds of the last backup (same as `last_run`).
        #[serde(default)]
        last_backup: Option<u64>,
        #[serde(default)]
        last_check: Option<CheckResult>,
        #[serde(default)]
        pending_deletes: Vec<PendingDelete>,
    },
    /// A request without data finished.
    Done,
    /// The backed-up folders.
    Folders {
        folders: Vec<String>,
    },
    /// The repository was created. `recovery_key` is shown once and never stored.
    RepoInitialized {
        recovery_key: String,
    },
    BackupDone {
        snapshot: String,
    },
    Snapshots {
        snapshots: Vec<SnapshotInfo>,
    },
    Restored,
    Disks {
        disks: Vec<DiskInfo>,
    },
    Prepared {
        /// Volume GUID path of the new partition (a "Volume{...}" path under the device namespace).
        volume_path: String,
        /// Unique partition GUID; the UI/service saves it as `Config::partition_guid`.
        partition_guid: String,
    },
    Error {
        message: String,
    },
}

/// Blocking client used by the UI.
pub mod client {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{self, BufRead, BufReader, Write};

    /// Send one request to the service on `pipe` and read one reply.
    pub fn request_on(pipe: &str, req: &Request) -> io::Result<Response> {
        let mut f = OpenOptions::new().read(true).write(true).open(pipe)?;
        let mut line = serde_json::to_string(req).map_err(io::Error::other)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.flush()?;
        let mut reader = BufReader::new(f);
        let mut out = String::new();
        reader.read_line(&mut out)?;
        serde_json::from_str(out.trim()).map_err(io::Error::other)
    }

    /// Send one request to the default pipe.
    pub fn request(req: &Request) -> io::Result<Response> {
        request_on(PIPE_NAME, req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        assert_eq!(
            serde_json::to_string(&Request::Ping).unwrap(),
            r#"{"cmd":"Ping"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::Pong).unwrap(),
            r#"{"reply":"Pong"}"#
        );
    }

    #[test]
    fn restore_roundtrip() {
        let r = Request::Restore {
            password: "pw".into(),
            snapshot: "abc".into(),
            target: "D:/out".into(),
            path: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        let r = Response::Snapshots {
            snapshots: vec![SnapshotInfo {
                id: "a".into(),
                short_id: "a".into(),
                time: "t".into(),
                paths: vec![],
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
    }

    #[test]
    fn status_roundtrip() {
        let r = Response::Status {
            version: "0.1.0".into(),
            frozen: true,
            frozen_since: Some("2026-01-01T00:00:00Z".into()),
            freeze_reasons: vec!["canary".into()],
            same_physical_disk: true,
            last_run: Some(1),
            last_backup: Some(1),
            last_check: None,
            pending_deletes: vec![PendingDelete {
                id: "x".into(),
                due: 2,
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
    }

    #[test]
    fn status_without_frozen_since_still_parses() {
        let r: Response =
            serde_json::from_str(r#"{"reply":"Status","version":"1","frozen":true}"#).unwrap();
        assert!(matches!(
            r,
            Response::Status {
                frozen: true,
                frozen_since: None,
                ..
            }
        ));
    }

    #[test]
    fn m5_requests_roundtrip() {
        for r in [
            Request::InitRepo {
                password: "p".into(),
                folders: vec!["C:/Docs".into()],
            },
            Request::DeleteSnapshots {
                password: "p".into(),
                ids: vec!["a".into()],
            },
            Request::CancelDelete {
                password: "p".into(),
            },
            Request::Unfreeze {
                password: "p".into(),
            },
            Request::ListFolders,
            Request::SetFolders {
                folders: vec!["C:/Docs".into()],
                password: Some("p".into()),
            },
        ] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
        let r = Response::RepoInitialized {
            recovery_key: "K".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
        let r = Response::Folders {
            folders: vec!["C:/Docs".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
        assert_eq!(
            serde_json::to_string(&Response::Done).unwrap(),
            r#"{"reply":"Done"}"#
        );
    }

    #[test]
    fn set_folders_password_is_optional() {
        let r: Request =
            serde_json::from_str(r#"{"cmd":"SetFolders","folders":["C:/A"]}"#).unwrap();
        assert_eq!(
            r,
            Request::SetFolders {
                folders: vec!["C:/A".into()],
                password: None
            }
        );
    }

    #[test]
    fn old_status_without_new_fields_parses() {
        let r: Response =
            serde_json::from_str(r#"{"reply":"Status","version":"1","frozen":false}"#).unwrap();
        assert!(matches!(
            r,
            Response::Status {
                same_physical_disk: false,
                ..
            }
        ));
    }
}

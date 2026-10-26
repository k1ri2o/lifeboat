//! IPC protocol between the UI and the service: one JSON object per line over
//! the named pipe [`PIPE_NAME`].

use crate::config::{CheckResult, PendingDelete, PendingVaultRemove};
use crate::disk::{DiskInfo, PrepareMode};
use crate::drive::{DriveDisk, LifeboatDrive};
use serde::{Deserialize, Serialize};

/// Name of the named pipe the service listens on.
pub const PIPE_NAME: &str = r"\\.\pipe\lifeboat";

/// A command sent to the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Request {
    Ping,
    Status,
    // --- drive statistics (read-only) ---
    /// Every disk, and what is known about the Lifeboat partition. No password: reveals nothing
    /// secret.
    DriveStats,
    // --- end drive statistics ---
    /// Back up all configured folders now.
    Backup,
    /// Normal backup snapshots (vault snapshots are left out; see [`Request::VaultList`]).
    ListSnapshots,
    /// One folder level of a snapshot. `path` is an [`FileEntry::path`] from an earlier
    /// reply; `None` lists the snapshot's backed-up top folders. No password: names only.
    ListFiles {
        snapshot: String,
        #[serde(default)]
        path: Option<String>,
    },
    /// Restore `paths` (each a [`FileEntry::path`]; empty = whole snapshot) from `snapshot`
    /// into the folder `target` (new or empty). Each item lands directly in `target` under its
    /// own name, not under its full original path. Dangerous: needs the repository `password`.
    Restore {
        password: String,
        snapshot: String,
        target: String,
        #[serde(default)]
        paths: Vec<String>,
    },
    /// Vault mode: move `paths` (files or folders) into the safe. The service backs them up
    /// as a snapshot tagged `vault`, checks every file is in it with the right size, and only
    /// then deletes the originals. Refused while frozen. Needs the `password`.
    VaultAdd {
        password: String,
        paths: Vec<String>,
    },
    /// What is kept only in the vault.
    VaultList,
    /// Copy vault items (`paths` are [`VaultItem::path`]s) out into the folder `target`
    /// (new or empty). The vault copy stays. Needs the `password`.
    VaultExtract {
        password: String,
        paths: Vec<String>,
        target: String,
    },
    /// Queue the removal of vault items (`paths` are [`VaultItem::path`]s). Like
    /// `DeleteSnapshots` it runs after a delay (7 days) and `CancelDelete` stops it. Needs the
    /// `password`.
    VaultRemove {
        password: String,
        paths: Vec<String>,
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
        /// Move an old repository with another password aside and start a new one.
        #[serde(default)]
        fresh: bool,
    },
    /// Queue snapshot deletes; they run after a delay (7 days). Needs the password.
    DeleteSnapshots {
        password: String,
        ids: Vec<String>,
    },
    /// Cancel every queued delete and vault removal. Needs the password.
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

    // --- Resize and delete the Lifeboat partition (#54, #55) ---
    /// Read-only: how big Lifeboat is, how full, and how far it can grow.
    LifeboatSizeInfo,
    /// Grow the Lifeboat partition to `new_size` bytes. Needs the password; refused while frozen.
    ResizeLifeboat {
        password: String,
        new_size: u64,
    },
    /// Grow Lifeboat to `new_size` when the free space is in front of it: build a bigger copy
    /// in space taken from the drive in front, check it, switch, remove the old one (#62).
    /// Needs the password; refused while frozen.
    MoveGrowLifeboat {
        password: String,
        new_size: u64,
    },
    /// Delete the Lifeboat partition and all backups on it, forever. `confirm` must be the
    /// word `DELETE`. Needs the password or recovery key; refused while frozen.
    RemoveLifeboat {
        password: String,
        confirm: String,
    },
}

/// A snapshot as shown to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: String,
    pub short_id: String,
    pub time: String,
    pub paths: Vec<String>,
    /// Name of the PC that made this backup (empty when unknown). Backups from another PC
    /// show up after a drive is opened on a new computer.
    #[serde(default)]
    pub hostname: String,
}

/// One entry of a snapshot folder listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path inside the snapshot, passed back in `ListFiles`/`Restore`.
    pub path: String,
    /// Display name (for a top folder: its Windows path, e.g. `C:\Users\Dad\Documents`).
    pub name: String,
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    /// Last change time (RFC 3339).
    #[serde(default)]
    pub mtime: String,
}

/// A file or folder kept only in the vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultItem {
    /// Original Windows path; also the id passed to `VaultExtract`.
    pub path: String,
    pub dir: bool,
    /// Total bytes.
    pub size: u64,
    /// When it was moved in (RFC 3339).
    pub added: String,
    /// The vault snapshot that holds it.
    pub snapshot: String,
    /// Unix seconds when a queued removal runs; `None` when none is queued.
    #[serde(default)]
    pub removes_at: Option<u64>,
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
        #[serde(default)]
        pending_vault_removes: Vec<PendingVaultRemove>,
    },
    // --- drive statistics (read-only) ---
    DriveStats {
        disks: Vec<DriveDisk>,
        /// `None` until a Lifeboat disk is prepared or when it is not found.
        lifeboat: Option<LifeboatDrive>,
    },
    // --- end drive statistics ---
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
    /// Restore or vault extract finished; `folder` is where the files are now.
    Restored {
        folder: String,
    },
    Files {
        entries: Vec<FileEntry>,
    },
    /// Vault add finished: `moved` items are now only in the vault.
    VaultAdded {
        snapshot: String,
        moved: u32,
    },
    Vault {
        items: Vec<VaultItem>,
    },
    Disks {
        disks: Vec<DiskInfo>,
    },
    Prepared {
        /// Volume GUID path of the new partition (a "Volume{...}" path under the device namespace).
        volume_path: String,
        /// Unique partition GUID; the UI/service saves it as `Config::partition_guid`.
        partition_guid: String,
    },
    /// Answer to `LifeboatSizeInfo` (#54).
    ResizeInfo {
        /// Current partition size in bytes.
        total: u64,
        /// Approximate bytes in use, when known.
        #[serde(default)]
        used: Option<u64>,
        /// Largest size Lifeboat can be given.
        max_size: u64,
        can_grow: bool,
        /// Plain-English reason when it can't grow.
        #[serde(default)]
        reason: String,
        /// Growing by moving is possible (#62). Old services omit it.
        #[serde(default)]
        can_move: bool,
        /// Largest total size reachable by moving.
        #[serde(default)]
        move_max: u64,
    },
    /// `MoveGrowLifeboat` finished; Lifeboat is now `new_size` bytes. `note` says what could
    /// not be finished (the backups are safe either way).
    Moved {
        new_size: u64,
        #[serde(default)]
        note: String,
    },
    /// `ResizeLifeboat` finished; Lifeboat is now `new_size` bytes.
    Resized {
        new_size: u64,
    },
    /// `RemoveLifeboat` finished. `note` is empty, or says what could not be finished (for
    /// example the freed space could not be given back to the drive in front).
    Removed {
        #[serde(default)]
        note: String,
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
            paths: vec![],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        let r = Response::Snapshots {
            snapshots: vec![SnapshotInfo {
                id: "a".into(),
                short_id: "a".into(),
                time: "t".into(),
                paths: vec![],
                hostname: "OLD-PC".into(),
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
    }

    #[test]
    fn drive_stats_roundtrip() {
        assert_eq!(
            serde_json::to_string(&Request::DriveStats).unwrap(),
            r#"{"cmd":"DriveStats"}"#
        );
        let r = Response::DriveStats {
            disks: vec![],
            lifeboat: Some(LifeboatDrive::default()),
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
            pending_vault_removes: vec![PendingVaultRemove {
                path: "C:/x".into(),
                snapshot: "s".into(),
                due: 3,
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
                fresh: false,
            },
            Request::DeleteSnapshots {
                password: "p".into(),
                ids: vec!["a".into()],
            },
            Request::CancelDelete {
                password: "p".into(),
            },
            Request::VaultRemove {
                password: "p".into(),
                paths: vec!["C:/x".into()],
            },
            Request::Unfreeze {
                password: "p".into(),
            },
            Request::ListFolders,
            Request::SetFolders {
                folders: vec!["C:/Docs".into()],
                password: Some("p".into()),
            },
            Request::LifeboatSizeInfo,
            Request::ResizeLifeboat {
                password: "p".into(),
                new_size: 5 << 30,
            },
            Request::MoveGrowLifeboat {
                password: "p".into(),
                new_size: 50 << 30,
            },
            Request::RemoveLifeboat {
                password: "p".into(),
                confirm: "DELETE".into(),
            },
        ] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
        for r in [
            Response::ResizeInfo {
                total: 10,
                used: Some(4),
                max_size: 20,
                can_grow: true,
                reason: String::new(),
                can_move: true,
                move_max: 40,
            },
            Response::Moved {
                new_size: 30,
                note: String::new(),
            },
            Response::Resized { new_size: 20 },
            Response::Removed {
                note: "careful".into(),
            },
        ] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
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
    fn old_resize_info_still_parses() {
        let j = r#"{"reply":"ResizeInfo","total":1,"max_size":2,"can_grow":false}"#;
        match serde_json::from_str::<Response>(j) {
            Ok(Response::ResizeInfo {
                can_move, move_max, ..
            }) => {
                assert!(!can_move);
                assert_eq!(move_max, 0);
            }
            other => panic!("{other:?}"),
        }
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

//! Configuration types.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// User configuration stored by the service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Folders that are backed up.
    pub folders: Vec<PathBuf>,
    /// Backup repository folder (the hidden partition comes in M3).
    #[serde(default)]
    pub repo: PathBuf,
    /// Repository password (temporary: real key handling comes later).
    #[serde(default)]
    pub password: String,
    /// Unique GUID of the Lifeboat partition (returned by PrepareDisk); how the partition is found.
    #[serde(default)]
    pub partition_guid: String,
    /// Unix seconds of the last backup cycle start (0 = never). Changes are collected since then.
    #[serde(default)]
    pub last_run: u64,
    /// Result of the last integrity check.
    #[serde(default)]
    pub last_check: Option<CheckResult>,
    /// Snapshot deletes waiting for their due time.
    #[serde(default)]
    pub pending_deletes: Vec<PendingDelete>,
    /// Vault items waiting to be removed from the vault.
    #[serde(default)]
    pub pending_vault_removes: Vec<PendingVaultRemove>,
    /// A move-and-grow in progress (#62), written before the disk is touched so a crash can
    /// be rolled back or finished on the next start.
    #[serde(default)]
    pub pending_move: Option<PendingMove>,
}

/// How far a move-and-grow got. The config switch is the point of no return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveStage {
    /// The new partition may exist, but the config still points at the old one: roll back.
    Building,
    /// The config points at the new partition; only the old one is left to remove.
    Switched,
}

/// Journal of a move-and-grow: the old Lifeboat partition is replaced by a bigger one made in
/// space taken from the drive in front of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMove {
    pub stage: MoveStage,
    /// GUID of the partition being replaced.
    pub old_guid: String,
    /// GUID of the new, bigger partition (known once it exists; empty before).
    #[serde(default)]
    pub new_guid: String,
    /// GUID of the drive that gave up the space.
    pub host_guid: String,
    /// The size asked for in total.
    pub target: u64,
}

/// Result of an integrity check (`restic check`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// Unix seconds when the check finished.
    pub time: u64,
    pub ok: bool,
    /// Short human text (error output when not ok).
    pub detail: String,
}

/// A snapshot delete queued with a delay, so a stolen password cannot erase history at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDelete {
    pub id: String,
    /// Unix seconds when the delete may run.
    pub due: u64,
}

/// A vault item removal queued with the same delay as a snapshot delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingVaultRemove {
    /// The item's original Windows path ([`crate::ipc::VaultItem::path`]).
    pub path: String,
    /// The vault snapshot that held it when it was queued (the run looks the path up again,
    /// because an earlier removal may have rewritten the snapshot).
    pub snapshot: String,
    /// Unix seconds when the removal may run.
    pub due: u64,
}

/// Delay between asking for a delete and it running.
pub const DELETE_DELAY_SECS: u64 = 7 * 24 * 3600;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let c = Config {
            folders: vec![PathBuf::from("Documents")],
            repo: PathBuf::from("repo"),
            password: "pw".into(),
            partition_guid: "{abc}".into(),
            last_run: 5,
            last_check: Some(CheckResult {
                time: 9,
                ok: true,
                detail: String::new(),
            }),
            pending_deletes: vec![PendingDelete {
                id: "a".into(),
                due: 7,
            }],
            pending_vault_removes: vec![PendingVaultRemove {
                path: "C:/x".into(),
                snapshot: "s".into(),
                due: 8,
            }],
            pending_move: Some(PendingMove {
                stage: MoveStage::Building,
                old_guid: "{old}".into(),
                new_guid: "{new}".into(),
                host_guid: "{host}".into(),
                target: 99,
            }),
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&s).unwrap(), c);
    }

    #[test]
    fn old_config_without_new_fields_loads() {
        let c: Config = serde_json::from_str(r#"{"folders":[]}"#).unwrap();
        assert_eq!(c.last_run, 0);
        assert!(c.pending_deletes.is_empty());
        assert!(c.pending_vault_removes.is_empty());
        assert!(c.pending_move.is_none());
    }
}

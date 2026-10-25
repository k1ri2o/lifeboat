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
    }
}

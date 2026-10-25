//! Saved service config: `C:\ProgramData\Lifeboat\config.json`.
//!
//! The repo password is stored encrypted with DPAPI (machine scope) and never logged. The
//! folder is locked to SYSTEM and the service account. The `LIFEBOAT_*` environment variables
//! stay as a test override (see [`env_override`]).

use crate::dpapi;
use lifeboat_core::config::{CheckResult, Config, PendingDelete, PendingVaultRemove};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Shape of the file on disk: like [`Config`] but the password is a DPAPI blob in hex.
#[derive(Serialize, Deserialize, Default)]
struct OnDisk {
    #[serde(default)]
    folders: Vec<PathBuf>,
    #[serde(default)]
    repo: PathBuf,
    #[serde(default)]
    password_dpapi: String,
    #[serde(default)]
    partition_guid: String,
    #[serde(default)]
    last_run: u64,
    #[serde(default)]
    last_check: Option<CheckResult>,
    #[serde(default)]
    pending_deletes: Vec<PendingDelete>,
    #[serde(default)]
    pending_vault_removes: Vec<PendingVaultRemove>,
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

pub struct Store {
    path: PathBuf,
    /// Lock the folder down with icacls on save (off in tests).
    secure: bool,
}

impl Store {
    pub fn production() -> Self {
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        Store {
            path: PathBuf::from(base).join("Lifeboat").join("config.json"),
            secure: true,
        }
    }

    /// Store at a custom path without ACL changes (tests).
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Store {
            path,
            secure: false,
        }
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// Create the data folder (and lock it to the service) so state files can be written
    /// before the first config save. On a fresh install it does not exist yet.
    pub fn ensure_dir(&self) -> io::Result<()> {
        let dir = self.dir();
        std::fs::create_dir_all(dir)?;
        if self.secure {
            secure_dir(dir)?;
        }
        Ok(())
    }

    /// Load; a missing file is the empty config. A password that cannot be decrypted is an error.
    pub fn load(&self) -> io::Result<Config> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e),
        };
        let d: OnDisk = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let password = if d.password_dpapi.is_empty() {
            String::new()
        } else {
            let blob = from_hex(&d.password_dpapi)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad password blob"))?;
            String::from_utf8(dpapi::unprotect(&blob)?)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        };
        Ok(Config {
            folders: d.folders,
            repo: d.repo,
            password,
            partition_guid: d.partition_guid,
            last_run: d.last_run,
            last_check: d.last_check,
            pending_deletes: d.pending_deletes,
            pending_vault_removes: d.pending_vault_removes,
        })
    }

    /// Atomic save (temp file + rename).
    pub fn save(&self, cfg: &Config) -> io::Result<()> {
        let dir = self.dir();
        std::fs::create_dir_all(dir)?;
        if self.secure {
            secure_dir(dir)?;
        }
        let d = OnDisk {
            folders: cfg.folders.clone(),
            repo: cfg.repo.clone(),
            password_dpapi: if cfg.password.is_empty() {
                String::new()
            } else {
                to_hex(&dpapi::protect(cfg.password.as_bytes())?)
            },
            partition_guid: cfg.partition_guid.clone(),
            last_run: cfg.last_run,
            last_check: cfg.last_check.clone(),
            pending_deletes: cfg.pending_deletes.clone(),
            pending_vault_removes: cfg.pending_vault_removes.clone(),
        };
        let json = serde_json::to_vec_pretty(&d)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &self.path)
    }
}

/// SYSTEM + service account only (same ACL as the hidden volume root).
fn secure_dir(dir: &Path) -> io::Result<()> {
    let root = dir.to_string_lossy().to_string();
    for args in lifeboat_disk::lock_down_args(&root, lifeboat_disk::acl::SERVICE_ACCOUNT) {
        let out = std::process::Command::new("icacls").args(&args).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "icacls failed: {}",
                String::from_utf8_lossy(&out.stdout)
            )));
        }
    }
    Ok(())
}

/// Config from the `LIFEBOAT_*` environment variables, when `LIFEBOAT_REPO` is set (test override).
pub fn env_override() -> Option<Config> {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    if var("LIFEBOAT_REPO").is_empty() {
        return None;
    }
    Some(Config {
        folders: var("LIFEBOAT_FOLDERS")
            .split(';')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        repo: PathBuf::from(var("LIFEBOAT_REPO")),
        password: var("LIFEBOAT_PASSWORD"),
        partition_guid: var("LIFEBOAT_PARTITION_GUID"),
        ..Config::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        assert_eq!(
            from_hex(&to_hex(&[0, 1, 254, 255])).unwrap(),
            [0, 1, 254, 255]
        );
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }

    #[test]
    fn save_load_roundtrip_password_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.json");
        let s = Store::at(path.clone());
        assert_eq!(s.load().unwrap(), Config::default());
        let c = Config {
            folders: vec![PathBuf::from("C:/Docs")],
            repo: PathBuf::from("E:/repo"),
            password: "correct horse battery".into(),
            partition_guid: "{abc}".into(),
            last_run: 42,
            last_check: Some(CheckResult {
                time: 7,
                ok: true,
                detail: String::new(),
            }),
            pending_deletes: vec![PendingDelete {
                id: "x".into(),
                due: 9,
            }],
            pending_vault_removes: vec![PendingVaultRemove {
                path: "C:/Secret".into(),
                snapshot: "v1".into(),
                due: 10,
            }],
        };
        s.save(&c).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("correct horse"), "{raw}");
        assert_eq!(s.load().unwrap(), c);
    }

    #[test]
    fn corrupt_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, b"{not json").unwrap();
        assert!(Store::at(p).load().is_err());
    }
}

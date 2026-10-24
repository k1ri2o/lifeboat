//! Persistent freeze state. Once frozen it stays frozen until explicitly cleared.

use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FreezeState {
    pub frozen: bool,
    /// Unix seconds when the freeze began (0 when not frozen).
    pub since: u64,
    pub reasons: Vec<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl FreezeState {
    /// Freeze now. A state that is already frozen keeps its original time and reasons.
    pub fn freeze(&mut self, reasons: Vec<String>) {
        if self.frozen {
            return;
        }
        self.frozen = true;
        self.since = now();
        self.reasons = reasons;
    }

    /// The only way out of a freeze: an explicit user action.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Load from `path`; a missing file means not frozen. A corrupt file is an error
    /// so the caller can fail safe (treat as frozen) instead of silently unfreezing.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomic save: write a temp file next to the target, flush to disk, then rename over it.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_sticky() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("freeze.json");
        assert!(!FreezeState::load(&p).unwrap().frozen);
        let mut s = FreezeState::default();
        s.freeze(vec!["canary".into()]);
        s.save(&p).unwrap();
        s.freeze(vec!["other".into()]);
        let mut loaded = FreezeState::load(&p).unwrap();
        assert_eq!(loaded, s);
        assert_eq!(loaded.reasons, vec!["canary".to_string()]);
        assert!(loaded.since > 0);
        assert!(!dir.path().join("freeze.json.tmp").exists());
        loaded.clear();
        loaded.save(&p).unwrap();
        assert!(!FreezeState::load(&p).unwrap().frozen);
    }

    #[test]
    fn corrupt_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.json");
        std::fs::write(&p, b"{not json").unwrap();
        assert!(FreezeState::load(&p).is_err());
    }
}

//! Hidden bait files. Nothing legitimate touches them, so any change is an attack.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// Hidden file names written into each watched folder.
const CANARY_NAMES: &[&str] = &[
    ".lifeboat-canary-a.docx.txt",
    ".lifeboat-canary-b.xlsx.txt",
    ".lifeboat-canary-c.pdf.txt",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryFile {
    pub path: PathBuf,
    pub len: u64,
    /// FNV-1a 64-bit hash of the content, hex.
    pub hash: String,
}

/// Record of the canaries created in one folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CanaryManifest {
    pub files: Vec<CanaryFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanaryHitKind {
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryHit {
    pub path: PathBuf,
    pub kind: CanaryHitKind,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hash_hex(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a(bytes))
}

fn content(i: usize) -> Vec<u8> {
    format!(
        "Lifeboat bait file {i}. Do not edit or delete. \
         This file only exists to detect ransomware.\n"
    )
    .repeat(20)
    .into_bytes()
}

fn set_hidden(path: &Path, hidden: bool) {
    if cfg!(windows) {
        // Hidden + system, so Explorer shows them only with protected OS files enabled.
        let (h, s) = if hidden { ("+h", "+s") } else { ("-h", "-s") };
        let _ = std::process::Command::new("attrib")
            .arg(h)
            .arg(s)
            .arg(path)
            .output();
    }
}

/// Write the bait files into `folder` (replacing stale ones) and return their record.
pub fn create_canaries(folder: &Path) -> io::Result<CanaryManifest> {
    let mut files = Vec::new();
    for (i, name) in CANARY_NAMES.iter().enumerate() {
        let path = folder.join(name);
        let data = content(i);
        if path.exists() {
            // A hidden file cannot be reopened for write on Windows; unhide first.
            set_hidden(&path, false);
        }
        std::fs::write(&path, &data)?;
        set_hidden(&path, true);
        files.push(CanaryFile {
            path,
            len: data.len() as u64,
            hash: hash_hex(&data),
        });
    }
    Ok(CanaryManifest { files })
}

/// Report every canary that changed or is gone.
pub fn check_canaries(manifest: &CanaryManifest) -> Vec<CanaryHit> {
    manifest
        .files
        .iter()
        .filter_map(|c| match std::fs::read(&c.path) {
            Err(_) => Some(CanaryHit {
                path: c.path.clone(),
                kind: CanaryHitKind::Deleted,
            }),
            Ok(d) if d.len() as u64 != c.len || hash_hex(&d) != c.hash => Some(CanaryHit {
                path: c.path.clone(),
                kind: CanaryHitKind::Modified,
            }),
            Ok(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_edit_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let m = create_canaries(dir.path()).unwrap();
        assert_eq!(m.files.len(), CANARY_NAMES.len());
        assert!(check_canaries(&m).is_empty());
        set_hidden(&m.files[0].path, false);
        std::fs::write(&m.files[0].path, b"encrypted garbage").unwrap();
        set_hidden(&m.files[1].path, false);
        std::fs::remove_file(&m.files[1].path).unwrap();
        let hits = check_canaries(&m);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].kind, CanaryHitKind::Modified);
        assert_eq!(hits[1].kind, CanaryHitKind::Deleted);
    }

    #[test]
    fn recreate_over_existing() {
        let dir = tempfile::tempdir().unwrap();
        create_canaries(dir.path()).unwrap();
        let m = create_canaries(dir.path()).unwrap();
        assert!(check_canaries(&m).is_empty());
    }
}

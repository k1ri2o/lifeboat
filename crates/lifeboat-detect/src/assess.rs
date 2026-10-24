//! Combine all signals of one backup run into a verdict.

use crate::baseline::Baseline;
use crate::canary::CanaryHit;
use crate::entropy::entropy;
use crate::magic::{is_naturally_high_entropy, mismatches};
use crate::ransom::{has_ransom_extension, is_ransom_note};
use std::path::PathBuf;

/// Share of a folder's existing files changed in one run above which it is abnormal.
pub const MAX_CHANGED_SHARE: f64 = 0.30;
/// Folders smaller than this never trigger the share rule (3 of 5 files is normal).
pub const MIN_FILES_FOR_SHARE: usize = 10;
/// Absolute count of existing files changed inside the window that is abnormal.
pub const MAX_CHANGED_IN_WINDOW: usize = 200;
/// Length of "a short window" for the absolute count rule, in seconds.
pub const WINDOW_SECS: u64 = 300;
/// With a trained baseline, a change count must also exceed this multiple of the usual rate.
pub const BASELINE_FACTOR: f64 = 5.0;
/// Entropy (bits/byte) at or above which content looks encrypted.
pub const HIGH_ENTROPY: f64 = 7.5;
/// A file counts as "was low entropy before" below this.
pub const LOW_ENTROPY_BEFORE: f64 = 6.5;
/// Files turned into high-entropy junk: weak signal from this many...
pub const ENCRYPTED_WEAK_MIN: usize = 2;
/// ...strong signal from this many.
pub const ENCRYPTED_STRONG_MIN: usize = 5;
/// This many encrypted-looking files is an attack on its own.
pub const ENCRYPTED_ATTACK_MIN: usize = 20;
/// Files whose content no longer matches their type: weak from 2, strong from this.
pub const MISMATCH_STRONG_MIN: usize = 5;
/// Files with ransom extensions: weak from 1, strong from this.
pub const RANSOM_EXT_STRONG_MIN: usize = 3;
/// Deleted share of a folder that is a mass deletion (with at least `MASS_DELETE_MIN` files).
pub const MASS_DELETE_SHARE: f64 = 0.30;
pub const MASS_DELETE_MIN: usize = 10;
/// Deletion this large is an attack on its own: half the folder (min 20 files) or 200 files.
pub const MASS_DELETE_ATTACK_SHARE: f64 = 0.50;
pub const MASS_DELETE_ATTACK_MIN: usize = 20;
pub const MASS_DELETE_ATTACK_COUNT: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    Renamed { from: PathBuf },
}

/// One changed file with the facts the detector needs (computed by the caller or [`FileChange::observe`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FileChange {
    /// Current path (the new name for a rename).
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub entropy_before: Option<f64>,
    pub entropy_after: Option<f64>,
    /// Content no longer matches the type its (original) name claims.
    pub magic_mismatch: bool,
}

impl FileChange {
    /// Build from the file's content before and after the change.
    pub fn observe(
        path: impl Into<PathBuf>,
        kind: ChangeKind,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) -> Self {
        let path = path.into();
        let name_for_type = match &kind {
            ChangeKind::Renamed { from } => from.clone(),
            _ => path.clone(),
        };
        FileChange {
            magic_mismatch: after
                .map(|a| mismatches(&name_for_type, a))
                .unwrap_or(false),
            entropy_before: before.map(entropy),
            entropy_after: after.map(entropy),
            path,
            kind,
        }
    }

    fn original_path(&self) -> &PathBuf {
        match &self.kind {
            ChangeKind::Renamed { from } => from,
            _ => &self.path,
        }
    }
}

/// Everything seen in one watched folder during one run.
#[derive(Debug, Clone, Default)]
pub struct ChangeSet {
    /// Number of files in the folder before this run.
    pub total_files: usize,
    pub changes: Vec<FileChange>,
    /// How many seconds the changes were spread over.
    pub window_secs: u64,
    pub canary_hits: Vec<CanaryHit>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Ok,
    Suspicious { reasons: Vec<String> },
    Attack { reasons: Vec<String> },
}

impl Verdict {
    pub fn is_attack(&self) -> bool {
        matches!(self, Verdict::Attack { .. })
    }
}

/// Judge one run. Created files never count towards "changed": building a project or
/// importing photos adds many files legitimately.
pub fn assess(baseline: &Baseline, cs: &ChangeSet) -> Verdict {
    let mut attack: Vec<String> = Vec::new();
    let mut strong: Vec<String> = Vec::new();
    let mut weak: Vec<String> = Vec::new();

    if !cs.canary_hits.is_empty() {
        attack.push(format!(
            "{} canary file(s) changed or deleted",
            cs.canary_hits.len()
        ));
    }

    let existing = |c: &&FileChange| !matches!(c.kind, ChangeKind::Created);
    let changed = cs.changes.iter().filter(existing).count();
    let deleted = cs
        .changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Deleted)
        .count();

    // Encrypted-looking: now high entropy, and either was low before or no longer matches its type.
    let encrypted = cs
        .changes
        .iter()
        .filter(|c| matches!(c.kind, ChangeKind::Modified | ChangeKind::Renamed { .. }))
        .filter(|c| {
            let high = c.entropy_after.map(|e| e >= HIGH_ENTROPY).unwrap_or(false);
            let was_low = c
                .entropy_before
                .map(|e| e < LOW_ENTROPY_BEFORE)
                .unwrap_or(false);
            high && (was_low || c.magic_mismatch)
                && (!is_naturally_high_entropy(c.original_path()) || c.magic_mismatch)
        })
        .count();
    if encrypted >= ENCRYPTED_ATTACK_MIN {
        attack.push(format!("{encrypted} files turned into random-looking data"));
    } else if encrypted >= ENCRYPTED_STRONG_MIN {
        strong.push(format!("{encrypted} files turned into random-looking data"));
    } else if encrypted >= ENCRYPTED_WEAK_MIN {
        weak.push(format!("{encrypted} files turned into random-looking data"));
    }

    let mism = cs
        .changes
        .iter()
        .filter(|c| existing(c) && c.magic_mismatch)
        .count();
    if mism >= MISMATCH_STRONG_MIN {
        strong.push(format!("{mism} files no longer match their file type"));
    } else if mism >= 2 {
        weak.push(format!("{mism} files no longer match their file type"));
    }

    let ransom_ext = cs
        .changes
        .iter()
        .filter(|c| {
            matches!(c.kind, ChangeKind::Created | ChangeKind::Renamed { .. })
                && has_ransom_extension(&c.path)
        })
        .count();
    if ransom_ext >= RANSOM_EXT_STRONG_MIN {
        strong.push(format!("{ransom_ext} files with ransomware extensions"));
    } else if ransom_ext >= 1 {
        weak.push(format!("{ransom_ext} file(s) with a ransomware extension"));
    }

    let notes = cs
        .changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Created && is_ransom_note(&c.path))
        .count();
    if notes > 0 {
        strong.push(format!("{notes} ransom note file(s) appeared"));
    }

    if cs.total_files > 0 {
        let del_share = deleted as f64 / cs.total_files as f64;
        if deleted >= MASS_DELETE_ATTACK_COUNT
            || (deleted >= MASS_DELETE_ATTACK_MIN && del_share >= MASS_DELETE_ATTACK_SHARE)
        {
            attack.push(format!(
                "mass deletion: {deleted} of {} files",
                cs.total_files
            ));
        } else if deleted >= MASS_DELETE_MIN && del_share >= MASS_DELETE_SHARE {
            strong.push(format!(
                "mass deletion: {deleted} of {} files",
                cs.total_files
            ));
        }
    }

    let share = if cs.total_files > 0 {
        changed as f64 / cs.total_files as f64
    } else {
        0.0
    };
    let abnormal_for_folder =
        !baseline.is_trained() || changed as f64 > BASELINE_FACTOR * baseline.ewma_changed;
    let by_share = cs.total_files >= MIN_FILES_FOR_SHARE && share > MAX_CHANGED_SHARE;
    let by_count = changed > MAX_CHANGED_IN_WINDOW && cs.window_secs <= WINDOW_SECS;
    if (by_share || by_count) && abnormal_for_folder {
        weak.push(format!(
            "{changed} existing files changed ({:.0}% of the folder)",
            share * 100.0
        ));
    }

    if !attack.is_empty() || strong.len() >= 2 || (!strong.is_empty() && weak.len() >= 2) {
        let mut r = attack;
        r.extend(strong);
        r.extend(weak);
        Verdict::Attack { reasons: r }
    } else if !strong.is_empty() || !weak.is_empty() {
        let mut r = strong;
        r.extend(weak);
        Verdict::Suspicious { reasons: r }
    } else {
        Verdict::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canary::{check_canaries, create_canaries};
    use std::fs;
    use std::path::Path;

    /// Tiny deterministic generator so tests need no extra crates.
    fn random_bytes(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn doc_bytes(i: usize) -> Vec<u8> {
        let mut v = b"PK\x03\x04".to_vec();
        v.extend(
            format!("plain document text number {i} with ordinary words. ")
                .repeat(100)
                .into_bytes(),
        );
        v
    }

    fn trained() -> Baseline {
        let mut b = Baseline::new();
        for _ in 0..5 {
            b.update(3, 100);
        }
        b
    }

    fn make_folder(dir: &Path, n: usize) {
        for i in 0..n {
            fs::write(dir.join(format!("doc{i}.docx")), doc_bytes(i)).unwrap();
        }
    }

    #[test]
    fn simulated_attack_is_attack() {
        let dir = tempfile::tempdir().unwrap();
        make_folder(dir.path(), 40);
        let mut changes = Vec::new();
        for i in 0..40 {
            let p = dir.path().join(format!("doc{i}.docx"));
            let before = fs::read(&p).unwrap();
            let after = random_bytes(i as u64, 8000);
            fs::write(&p, &after).unwrap();
            let locked = dir.path().join(format!("doc{i}.docx.locked"));
            fs::rename(&p, &locked).unwrap();
            changes.push(FileChange::observe(
                locked,
                ChangeKind::Renamed { from: p },
                Some(&before),
                Some(&after),
            ));
        }
        changes.push(FileChange::observe(
            dir.path().join("README_DECRYPT.txt"),
            ChangeKind::Created,
            None,
            Some(b"pay us"),
        ));
        let cs = ChangeSet {
            total_files: 40,
            changes,
            window_secs: 60,
            canary_hits: vec![],
        };
        let v = assess(&trained(), &cs);
        assert!(v.is_attack(), "{v:?}");
    }

    #[test]
    fn mass_delete_is_attack_or_suspicious() {
        let del = |n: usize| ChangeSet {
            total_files: 100,
            changes: (0..n)
                .map(|i| {
                    FileChange::observe(format!("f{i}.txt"), ChangeKind::Deleted, Some(b"x"), None)
                })
                .collect(),
            window_secs: 30,
            canary_hits: vec![],
        };
        assert!(assess(&trained(), &del(60)).is_attack());
        assert!(matches!(
            assess(&trained(), &del(35)),
            Verdict::Suspicious { .. }
        ));
        assert_eq!(assess(&trained(), &del(3)), Verdict::Ok);
    }

    #[test]
    fn canary_edit_is_attack_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let m = create_canaries(dir.path()).unwrap();
        let p = m.files[0].path.clone();
        // Windows refuses writes to hidden files through File::create; unhide like a real attacker could.
        let _ = std::process::Command::new("attrib")
            .arg("-h")
            .arg(&p)
            .output();
        fs::write(&p, random_bytes(1, 500)).unwrap();
        let cs = ChangeSet {
            total_files: 1000,
            changes: vec![],
            window_secs: 1,
            canary_hits: check_canaries(&m),
        };
        assert!(assess(&Baseline::new(), &cs).is_attack());
    }

    #[test]
    fn editing_a_few_docs_is_ok() {
        let changes: Vec<_> = (0..3)
            .map(|i| {
                FileChange::observe(
                    format!("doc{i}.docx"),
                    ChangeKind::Modified,
                    Some(&doc_bytes(i)),
                    Some(&doc_bytes(i + 10)),
                )
            })
            .collect();
        let cs = ChangeSet {
            total_files: 100,
            changes,
            window_secs: 60,
            canary_hits: vec![],
        };
        assert_eq!(assess(&trained(), &cs), Verdict::Ok);
    }

    #[test]
    fn adding_photos_and_building_a_project_is_ok() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend(random_bytes(7, 50_000));
        let mut changes: Vec<_> = (0..300)
            .map(|i| {
                FileChange::observe(
                    format!("img{i}.jpg"),
                    ChangeKind::Created,
                    None,
                    Some(&jpeg),
                )
            })
            .collect();
        let src = b"fn main() { println!(\"hello\"); } // object file text ".repeat(50);
        changes.extend((0..500).map(|i| {
            FileChange::observe(
                format!("target/o{i}.o"),
                ChangeKind::Created,
                None,
                Some(&src),
            )
        }));
        let cs = ChangeSet {
            total_files: 100,
            changes,
            window_secs: 120,
            canary_hits: vec![],
        };
        assert_eq!(assess(&Baseline::new(), &cs), Verdict::Ok);
    }

    #[test]
    fn naturally_high_entropy_zips_with_valid_magic_are_ok() {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend(random_bytes(3, 20_000));
        // Zips rewritten by an archiver: were already high entropy, still valid.
        let mut changes: Vec<_> = (0..30)
            .map(|i| {
                FileChange::observe(
                    format!("a{i}.zip"),
                    ChangeKind::Modified,
                    Some(&zip),
                    Some(&zip),
                )
            })
            .collect();
        // Even a zip that previously had no recorded entropy and now is high with valid magic.
        changes.push(FileChange::observe(
            "n.zip",
            ChangeKind::Modified,
            None,
            Some(&zip),
        ));
        let cs = ChangeSet {
            total_files: 1000,
            changes,
            window_secs: 60,
            canary_hits: vec![],
        };
        assert_eq!(assess(&trained(), &cs), Verdict::Ok);
        // Text files rewritten into a zip (e.g. user compresses a doc) is at most a weak single signal.
        assert!(!assess(&trained(), &cs).is_attack());
    }

    #[test]
    fn bulk_change_alone_is_only_suspicious() {
        let changes: Vec<_> = (0..60)
            .map(|i| {
                FileChange::observe(
                    format!("s{i}.txt"),
                    ChangeKind::Modified,
                    Some(b"old text content"),
                    Some(b"new text content"),
                )
            })
            .collect();
        let cs = ChangeSet {
            total_files: 100,
            changes,
            window_secs: 60,
            canary_hits: vec![],
        };
        assert!(matches!(
            assess(&trained(), &cs),
            Verdict::Suspicious { .. }
        ));
    }

    #[test]
    fn busy_folder_baseline_tolerates_bulk_changes() {
        let mut b = Baseline::new();
        for _ in 0..5 {
            b.update(50, 100);
        }
        let changes: Vec<_> = (0..60)
            .map(|i| {
                FileChange::observe(
                    format!("s{i}.txt"),
                    ChangeKind::Modified,
                    Some(b"a"),
                    Some(b"b"),
                )
            })
            .collect();
        let cs = ChangeSet {
            total_files: 100,
            changes,
            window_secs: 60,
            canary_hits: vec![],
        };
        assert_eq!(assess(&b, &cs), Verdict::Ok);
    }
}

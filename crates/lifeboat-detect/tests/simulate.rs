//! Attack simulations: a realistic folder in a tempdir, a scripted "ransomware" or a normal day,
//! and the detector's verdict. The change set is computed by diffing the folder against a
//! snapshot of its content, so these tests need no service, restic or elevation.

use lifeboat_detect::{
    assess, check_canaries, create_canaries, Baseline, CanaryManifest, ChangeKind, ChangeSet,
    FileChange, Verdict,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---- fixtures ------------------------------------------------------------------------------

/// Deterministic pseudo-random bytes (what good encryption output looks like).
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

fn prose(i: usize, n: usize) -> Vec<u8> {
    let words = [
        "quarterly", "report", "budget", "invoice", "holiday", "photos", "meeting", "notes", "the",
        "of", "and", "family", "tax", "return", "draft",
    ];
    let mut s = String::new();
    let mut k = i;
    while s.len() < n {
        k = k.wrapping_mul(31).wrapping_add(7);
        s.push_str(words[k % words.len()]);
        s.push(' ');
    }
    s.truncate(n);
    s.into_bytes()
}

/// Real header plus low-entropy body (a docx is a zip, so it starts with PK).
fn file_bytes(ext: &str, i: usize) -> Vec<u8> {
    let (head, n): (&[u8], usize) = match ext {
        "docx" | "xlsx" => (b"PK\x03\x04\x14\x00\x06\x00", 6000),
        "pdf" => (b"%PDF-1.7\n", 9000),
        "jpg" => (&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46], 12000),
        _ => (b"", 3000),
    };
    let mut v = head.to_vec();
    v.extend(prose(i, n));
    v
}

/// docx/jpg/pdf/txt/xlsx mix, the way a documents folder looks.
fn make_folder(dir: &Path, n: usize) {
    fs::create_dir_all(dir.join("Photos")).unwrap();
    fs::create_dir_all(dir.join("Work")).unwrap();
    let exts = ["docx", "jpg", "pdf", "txt", "xlsx"];
    for i in 0..n {
        let ext = exts[i % exts.len()];
        let sub = match ext {
            "jpg" => "Photos",
            "docx" | "xlsx" => "Work",
            _ => "",
        };
        fs::write(dir.join(sub).join(format!("file{i}.{ext}")), file_bytes(ext, i)).unwrap();
    }
}

type Snapshot = BTreeMap<PathBuf, Vec<u8>>;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn snapshot(dir: &Path) -> Snapshot {
    let mut v = Vec::new();
    walk(dir, &mut v);
    v.into_iter()
        .map(|p| {
            let b = fs::read(&p).unwrap();
            (p, b)
        })
        .collect()
}

/// Diff two snapshots into the change set the service would build for one run.
fn diff(before: &Snapshot, after: &Snapshot, manifest: &CanaryManifest, secs: u64) -> ChangeSet {
    let mut changes = Vec::new();
    let mut renamed_from = Vec::new();
    for (p, b) in after {
        match before.get(p) {
            Some(old) if old == b => {}
            Some(old) => changes.push(FileChange::observe(
                p.clone(),
                ChangeKind::Modified,
                Some(old),
                Some(b),
            )),
            None => {
                // "<old>.<ext>" next to a vanished "<old>" is a rename.
                let from = p.with_extension("");
                match before.get(&from).filter(|_| !after.contains_key(&from)) {
                    Some(old) => {
                        renamed_from.push(from.clone());
                        changes.push(FileChange::observe(
                            p.clone(),
                            ChangeKind::Renamed { from },
                            Some(old),
                            Some(b),
                        ));
                    }
                    None => changes.push(FileChange::observe(
                        p.clone(),
                        ChangeKind::Created,
                        None,
                        Some(b),
                    )),
                }
            }
        }
    }
    for (p, old) in before {
        if !after.contains_key(p) && !renamed_from.contains(p) {
            changes.push(FileChange::observe(
                p.clone(),
                ChangeKind::Deleted,
                Some(old),
                None,
            ));
        }
    }
    ChangeSet {
        total_files: before.len(),
        changes,
        window_secs: secs,
        canary_hits: check_canaries(manifest),
    }
}

/// A trained baseline for a folder where ~3 files change per run.
fn trained() -> Baseline {
    let mut b = Baseline::new();
    for _ in 0..5 {
        b.update(3, 100);
    }
    b
}

/// Folder with canaries planted the way the service plants them (`create_canaries`).
struct Sim {
    dir: tempfile::TempDir,
    manifest: CanaryManifest,
    before: Snapshot,
}

fn setup(n: usize) -> Sim {
    let dir = tempfile::tempdir().unwrap();
    make_folder(dir.path(), n);
    let manifest = create_canaries(dir.path()).unwrap();
    let before = snapshot(dir.path());
    Sim {
        dir,
        manifest,
        before,
    }
}

impl Sim {
    fn real_files(&self) -> Vec<PathBuf> {
        self.before
            .keys()
            .filter(|p| {
                !p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".lifeboat-canary")
            })
            .cloned()
            .collect()
    }

    fn verdict(&self, base: &Baseline, secs: u64) -> Verdict {
        let after = snapshot(self.dir.path());
        assess(base, &diff(&self.before, &after, &self.manifest, secs))
    }
}

/// Encrypt in place and append `.locked`, the classic behaviour.
fn encrypt(p: &Path, seed: u64) {
    let len = fs::metadata(p).unwrap().len() as usize;
    fs::write(p, random_bytes(seed, len)).unwrap();
    let mut locked = p.as_os_str().to_owned();
    locked.push(".locked");
    fs::rename(p, locked).unwrap();
}

/// Clear hidden/system so the simulated malware can write, as real malware (running as the
/// user) would.
fn unhide(manifest: &CanaryManifest) {
    if cfg!(windows) {
        for c in &manifest.files {
            let _ = std::process::Command::new("attrib")
                .arg("-h")
                .arg("-s")
                .arg(&c.path)
                .output();
        }
    }
}

// ---- scenarios -----------------------------------------------------------------------------

#[test]
fn fixture_is_clean_before_anything_happens() {
    let s = setup(60);
    assert_eq!(s.manifest.files.len(), 3);
    assert_eq!(s.verdict(&trained(), 0), Verdict::Ok);
}

#[test]
fn a_ransomware_encrypts_most_files_and_drops_a_note() {
    let s = setup(60);
    let files = s.real_files();
    for (i, p) in files.iter().enumerate().take(files.len() * 9 / 10) {
        encrypt(p, i as u64);
    }
    fs::write(
        s.dir.path().join("README_DECRYPT.txt"),
        b"Your files are encrypted. Pay 2 BTC.",
    )
    .unwrap();
    let v = s.verdict(&trained(), 90);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn a_ransomware_is_an_attack_on_an_untrained_baseline_too() {
    let s = setup(60);
    for (i, p) in s.real_files().iter().enumerate() {
        encrypt(p, i as u64);
    }
    let v = s.verdict(&Baseline::new(), 90);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn a_ransomware_that_overwrites_in_place_without_renaming_is_an_attack() {
    let s = setup(60);
    for (i, p) in s.real_files().iter().enumerate() {
        let len = fs::metadata(p).unwrap().len() as usize;
        fs::write(p, random_bytes(i as u64, len)).unwrap();
    }
    let v = s.verdict(&trained(), 90);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn b_slow_ransomware_that_touches_a_canary_trips_at_once() {
    let s = setup(60);
    unhide(&s.manifest);
    let files = s.real_files();
    encrypt(&files[0], 1);
    encrypt(&files[1], 2);
    fs::write(&s.manifest.files[0].path, random_bytes(9, 600)).unwrap();
    let v = s.verdict(&trained(), 3600);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn b_slow_ransomware_deleting_a_canary_trips_at_once() {
    let s = setup(60);
    unhide(&s.manifest);
    fs::remove_file(&s.manifest.files[2].path).unwrap();
    let v = s.verdict(&trained(), 3600);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn b_slow_ransomware_avoiding_canaries_is_at_least_flagged() {
    let s = setup(60);
    for (i, p) in s.real_files().iter().take(3).enumerate() {
        encrypt(p, i as u64);
    }
    let v = s.verdict(&trained(), 3600);
    assert_ne!(v, Verdict::Ok, "silent");
}

#[test]
fn b_slow_ransomware_avoiding_canaries_should_freeze() {
    let s = setup(60);
    for (i, p) in s.real_files().iter().take(3).enumerate() {
        encrypt(p, i as u64);
    }
    let v = s.verdict(&trained(), 3600);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn c_mass_delete_90_percent_freezes() {
    let s = setup(100);
    let files = s.real_files();
    for p in files.iter().take(files.len() * 9 / 10) {
        fs::remove_file(p).unwrap();
    }
    let v = s.verdict(&trained(), 120);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn c_mass_delete_freezes_on_a_busy_folder_baseline() {
    let s = setup(100);
    let mut busy = Baseline::new();
    for _ in 0..10 {
        busy.update(60, 100);
    }
    for p in s.real_files().iter().take(90) {
        fs::remove_file(p).unwrap();
    }
    let v = s.verdict(&busy, 120);
    assert!(v.is_attack(), "{v:?}");
}

#[test]
fn d_a_normal_day_does_not_freeze() {
    let s = setup(100);
    let files = s.real_files();
    // Edit 3 documents, add 20 photos and a new doc.
    for p in files.iter().filter(|p| p.extension().unwrap() == "docx").take(3) {
        let mut b = fs::read(p).unwrap();
        b.extend(prose(77, 400));
        fs::write(p, b).unwrap();
    }
    for i in 0..20u64 {
        let mut v = file_bytes("jpg", 1000 + i as usize);
        // Photos are compressed, so their tail is high entropy: still not an attack.
        v.extend(random_bytes(i, 20000));
        fs::write(s.dir.path().join("Photos").join(format!("IMG_{i:04}.jpg")), v).unwrap();
    }
    fs::write(s.dir.path().join("Work").join("new.docx"), file_bytes("docx", 9)).unwrap();
    let v = s.verdict(&trained(), 3600);
    assert_eq!(v, Verdict::Ok, "false positive");
}

#[test]
fn d_a_normal_day_on_an_untrained_baseline_does_not_freeze() {
    let s = setup(100);
    for p in s.real_files().iter().take(4) {
        let mut b = fs::read(p).unwrap();
        b.extend(prose(5, 300));
        fs::write(p, b).unwrap();
    }
    let v = s.verdict(&Baseline::new(), 3600);
    assert!(!v.is_attack(), "false positive: {v:?}");
}

#[test]
fn d_big_import_of_compressed_files_does_not_freeze() {
    let s = setup(100);
    for i in 0..150u64 {
        let mut v = b"PK\x03\x04".to_vec();
        v.extend(random_bytes(i, 5000));
        fs::write(s.dir.path().join(format!("import{i}.zip")), v).unwrap();
    }
    let v = s.verdict(&trained(), 60);
    assert!(!v.is_attack(), "{v:?}");
}

#[test]
fn d_renaming_a_document_is_not_an_attack() {
    let s = setup(100);
    let files = s.real_files();
    let p = files.iter().find(|p| p.extension().unwrap() == "txt").unwrap();
    fs::rename(p, p.with_file_name("renamed_notes.txt")).unwrap();
    let v = s.verdict(&trained(), 60);
    assert!(!v.is_attack(), "{v:?}");
}

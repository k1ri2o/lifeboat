//! Service-level simulations (M7): the real `run_cycle` with the real folder scan, and real
//! restic when `vendor/restic/restic.exe` exists. No disk, no elevation, no service install.

use crate::cycle::{
    load_canaries, plant_canaries, run_cycle, scan_folders, Env, Mount, Outcome, Paths,
};
use lifeboat_core::config::{Config, PendingDelete};
use lifeboat_detect::{ChangeSet, FreezeState};
use lifeboat_engine::{Engine, PRE_ALERT_TAG};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn restic() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/restic/restic.exe");
    p.exists().then_some(p)
}

macro_rules! need_restic {
    () => {
        match restic() {
            Some(p) => p,
            None => {
                eprintln!(
                    "SKIPPED: vendor/restic/restic.exe missing; run scripts/fetch-restic.ps1"
                );
                return;
            }
        }
    };
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

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
        "report", "budget", "invoice", "holiday", "notes", "the", "of", "and", "tax",
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

fn file_bytes(ext: &str, i: usize) -> Vec<u8> {
    let head: &[u8] = match ext {
        "docx" => b"PK\x03\x04\x14\x00\x06\x00",
        "pdf" => b"%PDF-1.7\n",
        "jpg" => &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46],
        _ => b"",
    };
    let mut v = head.to_vec();
    v.extend(prose(i, 4000));
    v
}

fn age(p: &Path, secs: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs);
    let f = fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(t).unwrap();
}

/// A documents folder; every file is made an hour old so only later edits count as changes.
fn make_folder(dir: &Path, n: usize) {
    fs::create_dir_all(dir).unwrap();
    let exts = ["docx", "jpg", "pdf", "txt"];
    for i in 0..n {
        let ext = exts[i % exts.len()];
        let p = dir.join(format!("file{i}.{ext}"));
        fs::write(&p, file_bytes(ext, i)).unwrap();
        age(&p, 3600);
    }
}

fn user_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && !p
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".lifeboat-canary")
        })
        .collect();
    v.sort();
    v
}

/// Same shape as the production `RealEnv` (real scan, real restic), but with a call log and
/// failure injection, and no disk.
struct SimEnv {
    cfg: Config,
    canaries: PathBuf,
    restic: Option<PathBuf>,
    backup_error: Option<String>,
    calls: RefCell<Vec<String>>,
}

impl SimEnv {
    fn new(cfg: &Config, paths: &Paths, restic: Option<PathBuf>) -> Self {
        SimEnv {
            cfg: cfg.clone(),
            canaries: paths.canaries.clone(),
            restic,
            backup_error: None,
            calls: RefCell::new(Vec::new()),
        }
    }
    fn engine(&self) -> Engine {
        Engine::new(
            self.restic.clone().expect("restic needed"),
            self.cfg.repo.clone(),
            self.cfg.password.clone(),
            self.cfg.folders.clone(),
        )
    }
    fn called(&self, s: &str) -> bool {
        self.calls.borrow().iter().any(|c| c == s)
    }
}

impl Env for SimEnv {
    fn now(&self) -> u64 {
        now()
    }
    fn mount(&self) -> Result<Mount, String> {
        Ok(Mount { disk: None })
    }
    fn offline(&self, _disk: u32) {}
    fn scan(&self, since: u64) -> Result<ChangeSet, String> {
        Ok(scan_folders(
            &self.cfg.folders,
            since,
            &load_canaries(&self.canaries),
        ))
    }
    fn backup(&self) -> Result<String, String> {
        self.calls.borrow_mut().push("backup".into());
        if let Some(e) = &self.backup_error {
            return Err(e.clone());
        }
        self.engine().backup().map_err(|e| e.to_string())
    }
    fn latest_snapshot(&self) -> Result<Option<String>, String> {
        if self.restic.is_none() {
            return Ok(None);
        }
        let mut v = self.engine().snapshots().map_err(|e| e.to_string())?;
        v.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(v.pop().map(|s| s.id))
    }
    fn tag_pre_alert(&self, id: &str) -> Result<(), String> {
        self.engine()
            .tag_add(id, PRE_ALERT_TAG)
            .map_err(|e| e.to_string())
    }
    fn retention(&self) -> Result<(), String> {
        self.calls.borrow_mut().push("retention".into());
        if self.restic.is_none() {
            return Ok(());
        }
        self.engine().apply_retention().map_err(|e| e.to_string())
    }
    fn check_subset(&self) -> Result<(), String> {
        self.calls.borrow_mut().push("check".into());
        Ok(())
    }
    fn forget(&self, ids: &[String]) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("forget {}", ids.join(",")));
        Ok(())
    }
}

struct World {
    _t: tempfile::TempDir,
    src: PathBuf,
    cfg: Config,
    paths: Paths,
}

fn world(n: usize) -> World {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("Documents");
    make_folder(&src, n);
    let state = t.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let paths = Paths::in_dir(&state);
    plant_canaries(std::slice::from_ref(&src), &paths.canaries).unwrap();
    let cfg = Config {
        folders: vec![src.clone()],
        repo: t.path().join("repo"),
        password: "pw-main".into(),
        ..Default::default()
    };
    World {
        _t: t,
        src,
        cfg,
        paths,
    }
}

/// All files below `root` by file name (restic recreates the original path under the target).
fn by_name(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read(&p).unwrap(),
                );
            }
        }
    }
    out
}

fn unhide(dir: &Path) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        if e.file_name()
            .to_string_lossy()
            .starts_with(".lifeboat-canary")
        {
            let _ = std::process::Command::new("attrib")
                .arg("-h")
                .arg("-s")
                .arg(e.path())
                .output();
        }
    }
}

// ---- real restic ---------------------------------------------------------------------------

#[test]
fn attack_keeps_the_clean_snapshot_and_restores_the_original_bytes() {
    let restic = need_restic!();
    let mut w = world(60);
    Engine::new(
        restic.clone(),
        &w.cfg.repo,
        &w.cfg.password,
        w.cfg.folders.clone(),
    )
    .init()
    .unwrap();

    // Day 1: clean backup through the real cycle.
    let env = SimEnv::new(&w.cfg, &w.paths, Some(restic.clone()));
    let first = run_cycle(&env, &mut w.cfg, &w.paths);
    assert!(matches!(first, Outcome::Backed { .. }), "{first:?}");
    let engine = env.engine();
    let snaps = engine.snapshots().unwrap();
    assert_eq!(snaps.len(), 1);
    let good_id = snaps[0].id.clone();
    let original: BTreeMap<String, Vec<u8>> = user_files(&w.src)
        .iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(p).unwrap(),
            )
        })
        .collect();

    // The attack: encrypt every file, rename to .locked, drop a note and touch a canary.
    w.cfg.last_run = now() - 600;
    unhide(&w.src);
    for (i, p) in user_files(&w.src).iter().enumerate() {
        let len = fs::metadata(p).unwrap().len() as usize;
        fs::write(p, random_bytes(i as u64, len)).unwrap();
        let mut l = p.as_os_str().to_owned();
        l.push(".locked");
        fs::rename(p, l).unwrap();
    }
    fs::write(
        w.src.join("README_DECRYPT.txt"),
        b"Your files are encrypted. Pay 2 BTC.",
    )
    .unwrap();

    let env = SimEnv::new(&w.cfg, &w.paths, Some(restic.clone()));
    let out = run_cycle(&env, &mut w.cfg, &w.paths);
    assert!(matches!(out, Outcome::Attack { .. }), "{out:?}");
    assert!(!env.called("backup"), "no backup may run after an attack");
    assert!(!env.called("retention"));

    // No new snapshot kept; the old one carries pre-alert (tagging rewrites its id).
    let snaps = engine.snapshots().unwrap();
    assert_eq!(snaps.len(), 1, "no new snapshot after an attack: {snaps:?}");
    assert!(
        snaps[0].tags.iter().any(|t| t == PRE_ALERT_TAG),
        "{snaps:?}"
    );
    let _ = good_id;
    assert!(FreezeState::load(&w.paths.freeze).unwrap().frozen);

    // Frozen: the next cycle does nothing, even though the scheduler keeps firing.
    let env = SimEnv::new(&w.cfg, &w.paths, Some(restic.clone()));
    assert_eq!(
        run_cycle(&env, &mut w.cfg, &w.paths),
        Outcome::SkippedFrozen
    );
    assert!(env.calls.borrow().is_empty());

    // The pre-alert snapshot restores the original bytes.
    let out = w._t.path().join("restored");
    engine.restore(&snaps[0].id, &out, None).unwrap();
    let restored = by_name(&out);
    let mut checked = 0;
    for (name, bytes) in &original {
        assert_eq!(
            restored.get(name),
            Some(bytes),
            "{name} differs after restore"
        );
        checked += 1;
    }
    assert_eq!(checked, 60);
}

#[test]
fn recovery_key_opens_the_repo_and_a_wrong_key_does_not() {
    let restic = need_restic!();
    let w = world(8);
    let e = Engine::new(
        restic.clone(),
        &w.cfg.repo,
        "pw-main",
        w.cfg.folders.clone(),
    );
    e.init().unwrap();
    e.backup().unwrap();
    e.add_key("recovery-words-one-two-three").unwrap();

    // The user forgot "pw-main": the recovery key alone opens the repo and lists the snapshot.
    let by_key = Engine::new(
        restic.clone(),
        &w.cfg.repo,
        "recovery-words-one-two-three",
        w.cfg.folders.clone(),
    );
    assert!(by_key
        .verify_password("recovery-words-one-two-three")
        .unwrap());
    assert_eq!(by_key.snapshots().unwrap().len(), 1);
    let out = w._t.path().join("restored-by-key");
    by_key
        .restore(&by_key.snapshots().unwrap()[0].id, &out, None)
        .unwrap();
    assert!(by_name(&out).len() >= 8);

    assert!(e.verify_password("pw-main").unwrap());
    assert!(!e.verify_password("not-the-key").unwrap());
    assert!(!e.verify_password("recovery-words-one-two").unwrap());
    let wrong = Engine::new(restic, &w.cfg.repo, "not-the-key", w.cfg.folders.clone());
    assert!(wrong.snapshots().is_err());
}

// ---- mass delete through the real scan --------------------------------------------------------

#[test]
fn mass_delete_90_percent_freezes_through_the_service_scan() {
    let mut w = world(100);
    w.cfg.last_run = now() - 600;
    // A previous clean run saw the whole folder.
    let before = scan_folders(&[w.src.clone()], 0, &[]).total_files;
    let b = lifeboat_detect::Baseline {
        ewma_changed: 2.0,
        runs: 5,
        file_count: before,
    };
    fs::write(&w.paths.baseline, serde_json::to_vec(&b).unwrap()).unwrap();
    let files = user_files(&w.src);
    for p in files.iter().take(90) {
        fs::remove_file(p).unwrap();
    }
    let env = SimEnv::new(&w.cfg, &w.paths, None);
    let mut env = env;
    env.backup_error = Some("would have backed up".into());
    let out = run_cycle(&env, &mut w.cfg, &w.paths);
    assert!(matches!(out, Outcome::Attack { .. }), "{out:?}");
    assert!(!env.called("backup"));
    // The baseline is not taught by an attack run.
    let saved: lifeboat_detect::Baseline =
        serde_json::from_slice(&fs::read(&w.paths.baseline).unwrap()).unwrap();
    assert_eq!(saved.file_count, before);
}

#[test]
fn mass_delete_that_also_wipes_the_canaries_freezes() {
    let mut w = world(100);
    w.cfg.last_run = now() - 600;
    unhide(&w.src);
    for p in fs::read_dir(&w.src).unwrap().flatten() {
        fs::remove_file(p.path()).unwrap();
    }
    let env = SimEnv::new(&w.cfg, &w.paths, None);
    let out = run_cycle(&env, &mut w.cfg, &w.paths);
    assert!(matches!(out, Outcome::Attack { .. }), "{out:?}");
    assert!(!env.called("backup"));
}

// ---- normal day through the real scan -----------------------------------------------------------

#[test]
fn normal_day_backs_up_and_does_not_freeze() {
    let mut w = world(60);
    w.cfg.last_run = now() - 600;
    for p in user_files(&w.src)
        .iter()
        .filter(|p| p.extension().unwrap() == "docx")
        .take(3)
    {
        let mut b = fs::read(p).unwrap();
        b.extend(prose(3, 300));
        fs::write(p, b).unwrap();
    }
    for i in 0..10u64 {
        let mut v = file_bytes("jpg", 500 + i as usize);
        v.extend(random_bytes(i, 15000));
        fs::write(w.src.join(format!("IMG_{i}.jpg")), v).unwrap();
    }
    let mut env = SimEnv::new(&w.cfg, &w.paths, None);
    env.backup_error = None;
    // No restic: stand in with an error-free fake by using the failure hook only for backup.
    let out = {
        struct NoRestic(SimEnv);
        impl Env for NoRestic {
            fn now(&self) -> u64 {
                self.0.now()
            }
            fn mount(&self) -> Result<Mount, String> {
                self.0.mount()
            }
            fn offline(&self, d: u32) {
                self.0.offline(d)
            }
            fn scan(&self, s: u64) -> Result<ChangeSet, String> {
                self.0.scan(s)
            }
            fn backup(&self) -> Result<String, String> {
                self.0.calls.borrow_mut().push("backup".into());
                Ok("snap".into())
            }
            fn latest_snapshot(&self) -> Result<Option<String>, String> {
                Ok(None)
            }
            fn tag_pre_alert(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn retention(&self) -> Result<(), String> {
                self.0.retention()
            }
            fn check_subset(&self) -> Result<(), String> {
                self.0.check_subset()
            }
            fn forget(&self, ids: &[String]) -> Result<(), String> {
                self.0.forget(ids)
            }
        }
        let n = NoRestic(env);
        let o = run_cycle(&n, &mut w.cfg, &w.paths);
        assert!(n.0.called("backup"));
        o
    };
    assert!(
        matches!(out, Outcome::Backed { .. }),
        "false positive: {out:?}"
    );
    assert!(!FreezeState::load(&w.paths.freeze)
        .map(|f| f.frozen)
        .unwrap_or(false));
}

// ---- full disk -----------------------------------------------------------------------------------

const DISK_FULL: &str = "restic failed (Some(1)): Fatal: unable to save snapshot: \
                         write R:\\repo\\data\\ab\\abcd: There is not enough space on the disk.";

#[test]
fn full_disk_is_an_error_and_never_forgets_or_prunes() {
    let mut w = world(10);
    // A due delayed delete and a due retention must NOT run when the backup could not be saved.
    w.cfg.pending_deletes = vec![PendingDelete {
        id: "oldsnap".into(),
        due: 1,
    }];
    let mut env = SimEnv::new(&w.cfg, &w.paths, None);
    env.backup_error = Some(DISK_FULL.into());
    let out = run_cycle(&env, &mut w.cfg, &w.paths);
    assert!(matches!(out, Outcome::Error(_)), "{out:?}");
    assert!(
        !env.called("retention"),
        "retention (prune) ran after a failed backup"
    );
    assert!(
        !env.calls.borrow().iter().any(|c| c.starts_with("forget")),
        "forget ran"
    );
    assert!(!env.called("check"));
    assert_eq!(
        w.cfg.pending_deletes.len(),
        1,
        "pending delete was consumed"
    );
    assert_eq!(
        w.cfg.last_run, 0,
        "last_run moved although nothing was saved"
    );
}

#[test]
fn full_disk_reports_a_clear_disk_full_error() {
    let mut w = world(10);
    let mut env = SimEnv::new(&w.cfg, &w.paths, None);
    env.backup_error = Some(DISK_FULL.into());
    match run_cycle(&env, &mut w.cfg, &w.paths) {
        Outcome::Error(e) => assert!(e.contains("The backup disk is full."), "{e}"),
        o => panic!("{o:?}"),
    }
}

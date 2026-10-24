//! The backup cycle: freeze check, mount, ransomware assessment, backup, retention, weekly
//! check and delayed deletes. Everything the cycle touches outside itself goes through [`Env`],
//! so the logic is tested with a fake and needs no disk, admin rights or restic.

use lifeboat_core::config::{CheckResult, Config};
use lifeboat_detect::{
    assess, check_canaries, create_canaries, Baseline, CanaryManifest, ChangeKind, ChangeSet,
    FileChange, FreezeState, Verdict,
};
use std::path::{Path, PathBuf};

/// Weekly integrity check interval.
pub const CHECK_INTERVAL_SECS: u64 = 7 * 24 * 3600;

/// Where the cycle keeps its state, next to the config.
#[derive(Debug, Clone)]
pub struct Paths {
    pub freeze: PathBuf,
    pub baseline: PathBuf,
    pub canaries: PathBuf,
}

impl Paths {
    pub fn in_dir(dir: &Path) -> Self {
        Paths {
            freeze: dir.join("freeze.json"),
            baseline: dir.join("baseline.json"),
            canaries: dir.join("canaries.json"),
        }
    }
}

/// Result of mounting the backup volume: the disk to take offline again afterwards.
pub struct Mount {
    pub disk: Option<u32>,
}

/// Everything the cycle needs from the outside world.
pub trait Env {
    fn now(&self) -> u64;
    /// Bring the disk online and confirm the Lifeboat volume.
    fn mount(&self) -> Result<Mount, String>;
    fn offline(&self, disk: u32);
    /// Files changed since `since` in the backed-up folders, canary hits included.
    fn scan(&self, since: u64) -> Result<ChangeSet, String>;
    fn backup(&self) -> Result<String, String>;
    /// Newest existing snapshot id, if any.
    fn latest_snapshot(&self) -> Result<Option<String>, String>;
    fn tag_pre_alert(&self, id: &str) -> Result<(), String>;
    fn retention(&self) -> Result<(), String>;
    fn check_subset(&self) -> Result<(), String>;
    fn forget(&self, ids: &[String]) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Frozen (or the freeze state could not be read): nothing was done.
    SkippedFrozen,
    /// Attack suspected: frozen, no snapshot taken.
    Attack {
        reasons: Vec<String>,
    },
    /// Clean run: snapshot taken. `notes` lists non-fatal problems (retention, check, deletes).
    Backed {
        snapshot: String,
        notes: Vec<String>,
    },
    Error(String),
}

/// Takes the disk offline when dropped, also when the cycle fails half way.
struct OfflineGuard<'a> {
    env: &'a dyn Env,
    disk: Option<u32>,
}

impl Drop for OfflineGuard<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.disk {
            self.env.offline(d);
        }
    }
}

fn load_baseline(path: &Path) -> Baseline {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_baseline(path: &Path, b: &Baseline) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(b).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Forget the saved file count after the user removes a watched folder, so the smaller
/// folder set is not mistaken for a mass delete. The next clean run learns the new count.
pub fn forget_file_count(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut b = load_baseline(path);
    b.file_count = 0;
    save_baseline(path, &b)
}

/// Number of files that count as "changed" for the baseline (created files never do).
fn changed_count(cs: &ChangeSet) -> usize {
    cs.changes
        .iter()
        .filter(|c| c.kind != ChangeKind::Created)
        .count()
}

/// Run one cycle. `cfg` is updated (`last_run`, `last_check`, `pending_deletes`); the caller saves it.
pub fn run_cycle(env: &dyn Env, cfg: &mut Config, paths: &Paths) -> Outcome {
    // A freeze state that cannot be read counts as frozen: fail safe.
    let mut freeze = match FreezeState::load(&paths.freeze) {
        Ok(f) => f,
        Err(_) => return Outcome::SkippedFrozen,
    };
    if freeze.frozen {
        return Outcome::SkippedFrozen;
    }
    // Folders added after setup get bait files too; recorded before the scan, so they are not
    // seen as changed. Best effort: a folder that cannot be written to is retried next cycle.
    let _ = plant_missing_canaries(&cfg.folders, &paths.canaries);
    let mount = match env.mount() {
        Ok(m) => m,
        Err(e) => return Outcome::Error(e),
    };
    let _guard = OfflineGuard {
        env,
        disk: mount.disk,
    };

    let mut baseline = load_baseline(&paths.baseline);
    let cs = match env.scan(cfg.last_run) {
        Ok(c) => c,
        Err(e) => return Outcome::Error(e),
    };
    if let Verdict::Attack { reasons } = assess(&baseline, &cs) {
        freeze.freeze(reasons.clone());
        if let Err(e) = freeze.save(&paths.freeze) {
            return Outcome::Error(format!("cannot save freeze state: {e}"));
        }
        // Mark the newest good snapshot so retention never removes it. No new snapshot.
        if let Ok(Some(id)) = env.latest_snapshot() {
            let _ = env.tag_pre_alert(&id);
        }
        return Outcome::Attack { reasons };
    }

    let snapshot = match env.backup() {
        Ok(s) => s,
        Err(e) if lifeboat_engine::is_disk_full(&e) => {
            return Outcome::Error(lifeboat_engine::DISK_FULL_MESSAGE.into())
        }
        Err(e) => return Outcome::Error(e),
    };
    // Only clean runs teach the baseline.
    baseline.update(changed_count(&cs), cs.total_files);
    let mut notes = Vec::new();
    if let Err(e) = save_baseline(&paths.baseline, &baseline) {
        notes.push(format!("baseline not saved: {e}"));
    }
    let now = env.now();
    cfg.last_run = now;

    if let Err(e) = env.retention() {
        notes.push(format!("retention failed: {e}"));
    }

    // Delayed deletes that are due (we are not frozen here).
    let due: Vec<String> = cfg
        .pending_deletes
        .iter()
        .filter(|d| d.due <= now)
        .map(|d| d.id.clone())
        .collect();
    if !due.is_empty() {
        match env.forget(&due) {
            Ok(()) => cfg.pending_deletes.retain(|d| !due.contains(&d.id)),
            Err(e) => notes.push(format!("delayed delete failed: {e}")),
        }
    }

    let check_due = cfg
        .last_check
        .as_ref()
        .map(|c| now.saturating_sub(c.time) > CHECK_INTERVAL_SECS)
        .unwrap_or(true);
    if check_due {
        let r = env.check_subset();
        cfg.last_check = Some(CheckResult {
            time: now,
            ok: r.is_ok(),
            detail: r.err().unwrap_or_default(),
        });
    }
    Outcome::Backed { snapshot, notes }
}

/// Load the saved canary manifests (missing file or damage => none).
pub fn load_canaries(path: &Path) -> Vec<CanaryManifest> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_canaries(path: &Path, m: &[CanaryManifest]) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(m).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Plant canaries in each folder and append their manifests to the saved ones.
pub fn plant_canaries(folders: &[PathBuf], path: &Path) -> Result<(), String> {
    let mut all = load_canaries(path);
    for f in folders {
        let m = create_canaries(f).map_err(|e| format!("canaries in {}: {e}", f.display()))?;
        all.push(m);
    }
    save_canaries(path, &all).map_err(|e| e.to_string())
}

/// Plant canaries in every existing watched folder that has none recorded yet.
pub fn plant_missing_canaries(folders: &[PathBuf], path: &Path) -> Result<(), String> {
    let known = load_canaries(path);
    let missing: Vec<PathBuf> = folders
        .iter()
        .filter(|f| f.is_dir())
        .filter(|f| {
            !known
                .iter()
                .any(|m| m.files.iter().any(|c| c.path.parent() == Some(f.as_path())))
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    plant_canaries(&missing, path)
}

// ---- real environment ----

const SAMPLE: usize = 64 * 1024;

fn unix(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, out),
            Ok(t) if t.is_file() => out.push(p),
            _ => {}
        }
    }
}

fn read_head(p: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut buf = vec![0u8; SAMPLE];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Scan `folders` for files changed after `since` (unix seconds). The first run (`since == 0`)
/// reports nothing as changed. Deleted and renamed files cannot be seen from timestamps alone;
/// a ransom extension on a changed file is reported as a rename.
pub fn scan_folders(folders: &[PathBuf], since: u64, manifests: &[CanaryManifest]) -> ChangeSet {
    let mut files = Vec::new();
    for f in folders {
        walk(f, &mut files);
    }
    let mut cs = ChangeSet {
        total_files: files.len(),
        ..Default::default()
    };
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    if since > 0 {
        for p in &files {
            let Ok(md) = std::fs::metadata(p) else {
                continue;
            };
            let m = md.modified().map(unix).unwrap_or(0);
            if m <= since {
                continue;
            }
            lo = lo.min(m);
            hi = hi.max(m);
            let created = md.created().map(unix).unwrap_or(0) > since;
            let kind = if lifeboat_detect::ransom::has_ransom_extension(p) {
                ChangeKind::Renamed {
                    from: p.with_extension(""),
                }
            } else if created {
                ChangeKind::Created
            } else {
                ChangeKind::Modified
            };
            let after = read_head(p);
            cs.changes
                .push(FileChange::observe(p.clone(), kind, None, after.as_deref()));
        }
        if hi >= lo {
            cs.window_secs = hi - lo;
        }
    }
    for m in manifests {
        cs.canary_hits.extend(check_canaries(m));
    }
    cs
}

/// Production environment: real disk, restic and folders.
pub struct RealEnv {
    pub cfg: Config,
    pub canaries: PathBuf,
}

impl RealEnv {
    fn engine(&self) -> lifeboat_engine::Engine {
        lifeboat_engine::Engine::new(
            lifeboat_engine::Engine::default_restic(),
            self.cfg.repo.clone(),
            self.cfg.password.clone(),
            self.cfg.folders.clone(),
        )
    }
}

/// Disk number holding the partition with `guid`, and whether it is a boot/system disk.
pub fn disk_of(guid: &str) -> Option<(u32, bool)> {
    let disks = lifeboat_disk::list_disks().ok()?;
    disks.into_iter().find_map(|d| {
        d.partitions
            .iter()
            .any(|p| lifeboat_core::disk::same_guid(&p.guid, guid))
            .then_some((d.number, d.is_boot || d.is_system))
    })
}

impl Env for RealEnv {
    fn now(&self) -> u64 {
        unix(std::time::SystemTime::now())
    }

    fn mount(&self) -> Result<Mount, String> {
        let guid = &self.cfg.partition_guid;
        if guid.is_empty() {
            // Test override without a partition: the repo is a plain folder.
            return Ok(Mount { disk: None });
        }
        let (disk, shared) = disk_of(guid).ok_or("backup disk not found")?;
        let mut offline_after = None;
        if !shared {
            lifeboat_disk::set_disk_online(disk).map_err(|e| e.to_string())?;
            offline_after = Some(disk);
        }
        let m = Mount {
            disk: offline_after,
        };
        // From here the guard in run_cycle owns the offline step; on failure do it here.
        match lifeboat_disk::find_lifeboat_volume(guid) {
            Ok(Some(_)) => Ok(m),
            Ok(None) => {
                if let Some(d) = m.disk {
                    self.offline(d);
                }
                Err("backup volume not found or marker mismatch".into())
            }
            Err(e) => {
                if let Some(d) = m.disk {
                    self.offline(d);
                }
                Err(e.to_string())
            }
        }
    }

    fn offline(&self, disk: u32) {
        let _ = lifeboat_disk::set_disk_offline(disk);
    }

    fn scan(&self, since: u64) -> Result<ChangeSet, String> {
        Ok(scan_folders(
            &self.cfg.folders,
            since,
            &load_canaries(&self.canaries),
        ))
    }

    fn backup(&self) -> Result<String, String> {
        self.engine().backup().map_err(|e| e.to_string())
    }

    fn latest_snapshot(&self) -> Result<Option<String>, String> {
        let mut v = self.engine().snapshots().map_err(|e| e.to_string())?;
        v.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(v.pop().map(|s| s.id))
    }

    fn tag_pre_alert(&self, id: &str) -> Result<(), String> {
        self.engine()
            .tag_add(id, lifeboat_engine::PRE_ALERT_TAG)
            .map_err(|e| e.to_string())
    }

    fn retention(&self) -> Result<(), String> {
        self.engine().apply_retention().map_err(|e| e.to_string())
    }

    fn check_subset(&self) -> Result<(), String> {
        self.engine().check_subset().map_err(|e| e.to_string())
    }

    fn forget(&self, ids: &[String]) -> Result<(), String> {
        self.engine()
            .forget_snapshots(ids)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lifeboat_core::config::PendingDelete;
    use lifeboat_detect::CanaryHit;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        now: u64,
        attack: bool,
        mount_fails: bool,
        backup_fails: bool,
        /// When set, `scan` reports hits from the canaries saved here, like the real scan.
        canaries: Option<PathBuf>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn log(&self, s: &str) {
            self.calls.borrow_mut().push(s.to_string());
        }
        fn called(&self, s: &str) -> bool {
            self.calls.borrow().iter().any(|c| c.starts_with(s))
        }
    }

    impl Env for Fake {
        fn now(&self) -> u64 {
            self.now
        }
        fn mount(&self) -> Result<Mount, String> {
            self.log("mount");
            if self.mount_fails {
                Err("no disk".into())
            } else {
                Ok(Mount { disk: Some(7) })
            }
        }
        fn offline(&self, disk: u32) {
            self.log(&format!("offline {disk}"));
        }
        fn scan(&self, _since: u64) -> Result<ChangeSet, String> {
            self.log("scan");
            let mut cs = ChangeSet {
                total_files: 100,
                ..Default::default()
            };
            if let Some(p) = &self.canaries {
                for m in load_canaries(p) {
                    cs.canary_hits.extend(check_canaries(&m));
                }
            }
            if self.attack {
                cs.canary_hits.push(CanaryHit {
                    path: "C:/x/canary.docx".into(),
                    kind: lifeboat_detect::canary::CanaryHitKind::Modified,
                });
            }
            Ok(cs)
        }
        fn backup(&self) -> Result<String, String> {
            self.log("backup");
            if self.backup_fails {
                Err("restic failed".into())
            } else {
                Ok("snap1".into())
            }
        }
        fn latest_snapshot(&self) -> Result<Option<String>, String> {
            Ok(Some("old1".into()))
        }
        fn tag_pre_alert(&self, id: &str) -> Result<(), String> {
            self.log(&format!("tag {id}"));
            Ok(())
        }
        fn retention(&self) -> Result<(), String> {
            self.log("retention");
            Ok(())
        }
        fn check_subset(&self) -> Result<(), String> {
            self.log("check");
            Ok(())
        }
        fn forget(&self, ids: &[String]) -> Result<(), String> {
            self.log(&format!("forget {}", ids.join(",")));
            Ok(())
        }
    }

    fn setup() -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::in_dir(d.path());
        (d, p)
    }

    #[test]
    fn freeze_load_error_means_frozen() {
        let (_d, p) = setup();
        std::fs::write(&p.freeze, b"{garbage").unwrap();
        let env = Fake::default();
        let mut cfg = Config::default();
        assert_eq!(run_cycle(&env, &mut cfg, &p), Outcome::SkippedFrozen);
        assert!(!env.called("mount") && !env.called("backup"));
    }

    #[test]
    fn frozen_skips_everything_including_retention_and_deletes() {
        let (_d, p) = setup();
        let mut f = FreezeState::default();
        f.freeze(vec!["x".into()]);
        f.save(&p.freeze).unwrap();
        let env = Fake {
            now: 10_000_000,
            ..Default::default()
        };
        let mut cfg = Config {
            pending_deletes: vec![PendingDelete {
                id: "a".into(),
                due: 1,
            }],
            ..Config::default()
        };
        assert_eq!(run_cycle(&env, &mut cfg, &p), Outcome::SkippedFrozen);
        assert!(env.calls.borrow().is_empty());
        assert_eq!(cfg.pending_deletes.len(), 1);
    }

    #[test]
    fn attack_freezes_tags_pre_alert_and_takes_no_snapshot() {
        let (_d, p) = setup();
        let env = Fake {
            attack: true,
            ..Default::default()
        };
        let mut cfg = Config::default();
        let out = run_cycle(&env, &mut cfg, &p);
        assert!(matches!(out, Outcome::Attack { .. }), "{out:?}");
        assert!(env.called("tag old1"));
        assert!(!env.called("backup"));
        assert!(!env.called("retention"));
        let f = FreezeState::load(&p.freeze).unwrap();
        assert!(f.frozen && !f.reasons.is_empty() && f.since > 0);
        assert!(env.called("offline 7"));
        // The next cycle is skipped.
        let env2 = Fake::default();
        assert_eq!(run_cycle(&env2, &mut cfg, &p), Outcome::SkippedFrozen);
    }

    #[test]
    fn forget_file_count_keeps_the_rest_of_the_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("baseline.json");
        forget_file_count(&p).unwrap();
        assert!(!p.exists());
        let mut b = Baseline::default();
        b.update(4, 500);
        save_baseline(&p, &b).unwrap();
        forget_file_count(&p).unwrap();
        let after = load_baseline(&p);
        assert_eq!(after.file_count, 0);
        assert_eq!(after.runs, b.runs);
    }

    #[test]
    fn baseline_not_updated_on_attack_but_updated_on_clean() {
        let (_d, p) = setup();
        let env = Fake {
            attack: true,
            ..Default::default()
        };
        let mut cfg = Config::default();
        run_cycle(&env, &mut cfg, &p);
        assert!(
            !p.baseline.exists(),
            "attack run must not touch the baseline"
        );

        let (_d2, p2) = setup();
        let env = Fake {
            now: 500,
            ..Default::default()
        };
        let mut cfg = Config::default();
        assert!(matches!(
            run_cycle(&env, &mut cfg, &p2),
            Outcome::Backed { .. }
        ));
        assert_eq!(load_baseline(&p2.baseline).runs, 1);
        assert_eq!(cfg.last_run, 500);
    }

    #[test]
    fn clean_run_does_retention_and_weekly_check() {
        let (_d, p) = setup();
        let env = Fake {
            now: 1_000_000,
            ..Default::default()
        };
        let mut cfg = Config::default();
        run_cycle(&env, &mut cfg, &p);
        assert!(env.called("retention") && env.called("check"));
        assert!(cfg.last_check.as_ref().unwrap().ok);
        // Checked just now: not again.
        let env = Fake {
            now: 1_000_100,
            ..Default::default()
        };
        run_cycle(&env, &mut cfg, &p);
        assert!(!env.called("check"));
        // More than 7 days later: again.
        let env = Fake {
            now: 1_000_000 + CHECK_INTERVAL_SECS + 1,
            ..Default::default()
        };
        run_cycle(&env, &mut cfg, &p);
        assert!(env.called("check"));
    }

    #[test]
    fn delayed_delete_waits_until_due() {
        let (_d, p) = setup();
        let mut cfg = Config {
            pending_deletes: vec![
                PendingDelete {
                    id: "due".into(),
                    due: 100,
                },
                PendingDelete {
                    id: "later".into(),
                    due: 5000,
                },
            ],
            ..Config::default()
        };
        let env = Fake {
            now: 50,
            ..Default::default()
        };
        run_cycle(&env, &mut cfg, &p);
        assert!(!env.called("forget"), "not due yet");
        assert_eq!(cfg.pending_deletes.len(), 2);

        let env = Fake {
            now: 200,
            ..Default::default()
        };
        run_cycle(&env, &mut cfg, &p);
        assert!(env.called("forget due"));
        assert_eq!(cfg.pending_deletes.len(), 1);
        assert_eq!(cfg.pending_deletes[0].id, "later");
    }

    #[test]
    fn disk_goes_offline_on_error() {
        let (_d, p) = setup();
        let env = Fake {
            backup_fails: true,
            ..Default::default()
        };
        let mut cfg = Config::default();
        assert!(matches!(run_cycle(&env, &mut cfg, &p), Outcome::Error(_)));
        assert!(env.called("offline 7"));
        // A failed backup does not advance last_run.
        assert_eq!(cfg.last_run, 0);
    }

    #[test]
    fn mount_failure_is_error_without_backup() {
        let (_d, p) = setup();
        let env = Fake {
            mount_fails: true,
            ..Default::default()
        };
        let mut cfg = Config::default();
        assert_eq!(
            run_cycle(&env, &mut cfg, &p),
            Outcome::Error("no disk".into())
        );
        assert!(!env.called("scan"));
    }

    #[test]
    fn scan_counts_changed_files_and_first_run_reports_none() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(d.path().join("b.txt"), b"world").unwrap();
        let first = scan_folders(&[d.path().to_path_buf()], 0, &[]);
        assert_eq!(first.total_files, 2);
        assert!(first.changes.is_empty());
        let later = scan_folders(&[d.path().to_path_buf()], 1, &[]);
        assert_eq!(later.changes.len(), 2);
    }

    #[test]
    fn canaries_planted_and_loaded() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("docs");
        std::fs::create_dir(&folder).unwrap();
        let path = d.path().join("canaries.json");
        plant_canaries(&[folder], &path).unwrap();
        let m = load_canaries(&path);
        assert_eq!(m.len(), 1);
        assert!(!m[0].files.is_empty());
        assert!(check_canaries(&m[0]).is_empty());
    }

    #[test]
    fn folder_added_after_init_gets_canaries_without_freezing() {
        let (_d, p) = setup();
        let folder = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            folders: vec![folder.path().to_path_buf()],
            ..Config::default()
        };
        let env = Fake {
            canaries: Some(p.canaries.clone()),
            ..Default::default()
        };
        // First cycle plants the bait and does not treat it as changed.
        let out = run_cycle(&env, &mut cfg, &p);
        assert!(matches!(out, Outcome::Backed { .. }), "{out:?}");
        let m = load_canaries(&p.canaries);
        assert_eq!(m.len(), 1);
        assert!(!m[0].files.is_empty());
        // A second cycle does not plant again.
        run_cycle(&env, &mut cfg, &p);
        assert_eq!(load_canaries(&p.canaries).len(), 1);
        // Tampering with a canary freezes the next cycle.
        let f = &m[0].files[0].path;
        if cfg!(windows) {
            let _ = std::process::Command::new("attrib")
                .args(["-h", "-s"])
                .arg(f)
                .output();
        }
        std::fs::write(f, b"encrypted").unwrap();
        let out = run_cycle(&env, &mut cfg, &p);
        assert!(matches!(out, Outcome::Attack { .. }), "{out:?}");
    }
}

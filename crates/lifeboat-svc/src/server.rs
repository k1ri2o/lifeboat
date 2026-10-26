//! Named-pipe server speaking the JSON-lines protocol.

use crate::cache::{Caches, MAX_TREE_NODES};
use crate::ctx::Ctx;
use crate::cycle::{self, Env, Outcome, Paths, RealEnv};
use crate::security::{
    gate, pipe_client_is_admin, production_sddl, require_admin, Descriptor, RateLimiter, Refusal,
};
use crate::store::{self, Store};
use lifeboat_core::config::{
    Config, MoveStage, PendingDelete, PendingMove, PendingVaultRemove, DELETE_DELAY_SECS,
};
use lifeboat_core::disk::PrepareMode;
use lifeboat_core::ipc::{FileEntry, Request, Response, SnapshotInfo, VaultItem, PIPE_NAME};
use lifeboat_detect::FreezeState;
use lifeboat_engine::{Engine, Snapshot};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::watch;

/// Creates the hidden partition: returns `(volume_path, partition_guid)`. Replaced in tests.
pub type PrepareFn =
    Box<dyn Fn(u32, PrepareMode) -> Result<(String, String), String> + Send + Sync>;

type VerifyFn =
    Box<dyn Fn(&State, &Config, &RateLimiter, &str) -> Result<(), Response> + Send + Sync>;
type GrowInfoFn = Box<dyn Fn(&str) -> Result<lifeboat_disk::GrowInfo, String> + Send + Sync>;
type GrowFn = Box<dyn Fn(&str, u64) -> Result<u64, String> + Send + Sync>;
type RemoveFn = Box<dyn Fn(&str) -> Result<String, String> + Send + Sync>;
type MovePlanFn = Box<dyn Fn(&str) -> Result<lifeboat_disk::MovePlan, String> + Send + Sync>;
type ShrinkFn = Box<dyn Fn(&str, u64) -> Result<u64, String> + Send + Sync>;
type CreateFn = Box<dyn Fn(&str, u64) -> Result<String, String> + Send + Sync>;
type TwoGuidFn = Box<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;
type OneGuidFn = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
type CheckCopyFn = Box<dyn Fn(&Config, &str, &str) -> Result<(), String> + Send + Sync>;
type RollbackFn = Box<dyn Fn(&str, &str, &str) -> Result<(), String> + Send + Sync>;
type InUseFn = Box<dyn Fn(&State) -> Option<u64> + Send + Sync>;
type ExistsFn = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// The disk and password steps behind resize and delete (#54, #55). The real ones touch
/// Windows disks, so tests replace them.
pub struct DiskOps {
    /// Checks the password (or recovery key) for a destructive request. `Ok` when it fits.
    pub verify: VerifyFn,
    pub grow_info: GrowInfoFn,
    /// Grow to this many bytes; returns the new size.
    pub grow: GrowFn,
    /// Delete the partition; returns a note for the user (may be empty).
    pub remove: RemoveFn,
    // --- Move and grow (#62) ---
    /// Read-only plan: which drive gives space, and how much.
    pub move_plan: MovePlanFn,
    /// Space in use on the open Lifeboat volume, in bytes.
    pub in_use: InUseFn,
    /// Shrink the drive in front of Lifeboat by this much.
    pub move_shrink: ShrinkFn,
    /// Create the new hidden partition of this size; returns its GUID.
    pub move_create: CreateFn,
    /// Copy the repository from the old partition to the new one and verify the copy.
    pub move_copy: TwoGuidFn,
    /// Run the restic check on the copy (config, new GUID, password).
    pub move_check: CheckCopyFn,
    /// Delete the old partition (marker verified) without extending the drive in front.
    pub move_remove_old: OneGuidFn,
    /// Grow the new partition up to this total size; returns its size.
    pub move_grow: GrowFn,
    /// Undo before the switch: (old GUID, host GUID, new GUID or empty).
    pub move_rollback: RollbackFn,
    /// Does a partition with this GUID still exist?
    pub move_exists: ExistsFn,
}

impl DiskOps {
    #[allow(clippy::result_large_err)]
    fn real() -> Self {
        DiskOps {
            verify: Box::new(|st, cfg, limiter, pw| {
                let checked = with_volume(st, cfg, |c, _| match password_gate(c, limiter, pw) {
                    Ok(()) => Response::Done,
                    Err(r) => refused(r),
                });
                match checked {
                    Response::Done => Ok(()),
                    other => Err(other),
                }
            }),
            grow_info: Box::new(|g| lifeboat_disk::grow_info(g).map_err(|e| e.to_string())),
            grow: Box::new(|g, n| lifeboat_disk::grow_lifeboat(g, n).map_err(|e| e.to_string())),
            remove: Box::new(|g| {
                lifeboat_disk::remove_lifeboat(g)
                    .map(|o| o.note)
                    .map_err(|e| e.to_string())
            }),
            move_plan: Box::new(|g| lifeboat_disk::move_grow::plan(g).map_err(|e| e.to_string())),
            in_use: Box::new(|st| {
                let guid = st.snapshot_cfg().partition_guid;
                let total = lifeboat_disk::grow_info(&guid).ok()?.current_size;
                let v = st.ctx.lease.held_path()?;
                free_bytes(&v).map(|f| total.saturating_sub(f))
            }),
            move_shrink: Box::new(|h, x| {
                lifeboat_disk::move_grow::shrink_host(h, x).map_err(|e| e.to_string())
            }),
            move_create: Box::new(|g, x| {
                lifeboat_disk::move_grow::create_new(g, x).map_err(|e| e.to_string())
            }),
            move_copy: Box::new(|a, b| {
                lifeboat_disk::move_grow::copy_repo(a, b).map_err(|e| e.to_string())
            }),
            move_check: Box::new(|cfg, new_guid, password| {
                let vol = match lifeboat_disk::open_volume(new_guid, None) {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        let _ = lifeboat_disk::close_volume(new_guid, None);
                        return Err("The new copy could not be opened.".into());
                    }
                    Err(e) => {
                        let _ = lifeboat_disk::close_volume(new_guid, None);
                        return Err(e.to_string());
                    }
                };
                let eng = Engine::new(
                    Engine::default_restic(),
                    PathBuf::from(&vol).join("repo"),
                    password.to_string(),
                    cfg.folders.clone(),
                );
                let r = eng
                    .check()
                    .map_err(|e| format!("The new copy did not pass its check: {e}"));
                let _ = lifeboat_disk::close_volume(new_guid, None);
                r
            }),
            move_remove_old: Box::new(|g| {
                lifeboat_disk::move_grow::remove_partition(g).map_err(|e| e.to_string())
            }),
            move_grow: Box::new(|g, n| {
                lifeboat_disk::move_grow::grow_new(g, n).map_err(|e| e.to_string())
            }),
            move_rollback: Box::new(|o, h, n| {
                lifeboat_disk::move_grow::rollback(o, h, n).map_err(|e| e.to_string())
            }),
            move_exists: Box::new(|g| {
                lifeboat_disk::list_disks()
                    .map(|ds| {
                        ds.iter()
                            .any(|d| d.partitions.iter().any(|p| p.has_guid(g)))
                    })
                    .unwrap_or(true)
            }),
        }
    }
}

/// Everything the request handlers share.
pub struct State {
    pub cfg: Mutex<Config>,
    /// `None` under the environment override: nothing is persisted.
    pub store: Option<Store>,
    pub paths: Paths,
    /// Only one backup cycle at a time (timer, watcher and the Backup request).
    pub cycle_lock: Mutex<()>,
    pub prepare: PrepareFn,
    /// Volume lease, result caches and repository lock.
    pub ctx: Arc<Ctx>,
    /// Resize / delete steps (replaced in tests).
    pub disk_ops: DiskOps,
}

impl State {
    pub fn new(cfg: Config, store: Option<Store>, paths: Paths) -> Self {
        State {
            cfg: Mutex::new(cfg),
            store,
            paths,
            cycle_lock: Mutex::new(()),
            ctx: Arc::new(Ctx::new()),
            disk_ops: DiskOps::real(),
            prepare: Box::new(|disk, mode| {
                prepare(disk, mode)
                    .map(|v| (v.volume_path, v.partition_guid))
                    .map_err(|e| e.to_string())
            }),
        }
    }

    /// Production state: the environment override when set, else the saved config.
    pub fn load_default() -> io::Result<Self> {
        if let Some(cfg) = store::env_override() {
            let dir = std::env::temp_dir().join("lifeboat-env");
            std::fs::create_dir_all(&dir)?;
            return Ok(State::new(cfg, None, Paths::in_dir(&dir)));
        }
        let store = Store::production();
        store.ensure_dir()?;
        let cfg = store.load()?;
        let paths = Paths::in_dir(store.dir());
        Ok(State::new(cfg, Some(store), paths))
    }

    /// Finish or undo a move-and-grow that a crash or power loss interrupted (#62). Run once
    /// at start, before anything else uses the disk. A journal that cannot be settled stays
    /// for the next start.
    pub fn recover_move(&self) {
        let cfg = self.snapshot_cfg();
        let Some(j) = cfg.pending_move.clone() else {
            return;
        };
        let ops = &self.disk_ops;
        match j.stage {
            MoveStage::Building => {
                // The config still points at the old partition: remove the half-made new one
                // and give the drive in front its space back.
                if (ops.move_rollback)(&j.old_guid, &j.host_guid, &j.new_guid).is_ok() {
                    let _ = self.update(|c| c.pending_move = None);
                }
            }
            MoveStage::Switched => {
                if cfg.partition_guid != j.new_guid {
                    // Not what the journal expects: touch no disk, just drop the note.
                    let _ = self.update(|c| c.pending_move = None);
                    return;
                }
                if (ops.move_exists)(&j.old_guid) && (ops.move_remove_old)(&j.old_guid).is_err() {
                    return;
                }
                // Best effort: backups already run from the new partition.
                let _ = (ops.move_grow)(&j.new_guid, j.target);
                let _ = self.update(|c| c.pending_move = None);
            }
        }
    }

    fn snapshot_cfg(&self) -> Config {
        self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn update(&self, f: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut g = self.cfg.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g);
        if let Some(s) = &self.store {
            s.save(&g).map_err(|e| format!("cannot save config: {e}"))?;
        }
        Ok(())
    }

    /// Run one backup cycle (serialised). Config fields the cycle changes are merged back.
    pub fn run_backup(&self) -> Outcome {
        let _one = self.cycle_lock.lock().unwrap_or_else(|e| e.into_inner());
        let before = self.snapshot_cfg();
        if before.repo.as_os_str().is_empty() || before.password.is_empty() {
            return Outcome::Error("backup repository is not configured".into());
        }
        let env = RealEnv {
            cfg: before.clone(),
            canaries: self.paths.canaries.clone(),
            ctx: self.ctx.clone(),
            held: Mutex::new(None),
        };
        self.cycle_with(&env, before)
    }

    /// The cycle with a given environment (separate for tests).
    pub fn cycle_with(&self, env: &dyn Env, before: Config) -> Outcome {
        let mut after = before.clone();
        let out = cycle::run_cycle(env, &mut after, &self.paths);
        let removed: Vec<String> = before
            .pending_deletes
            .iter()
            .filter(|d| !after.pending_deletes.iter().any(|a| a.id == d.id))
            .map(|d| d.id.clone())
            .collect();
        let removed_vault: Vec<String> = before
            .pending_vault_removes
            .iter()
            .filter(|d| !after.pending_vault_removes.iter().any(|a| a.path == d.path))
            .map(|d| d.path.clone())
            .collect();
        let _ = self.update(|c| {
            c.last_run = after.last_run;
            c.last_check = after.last_check.clone();
            c.pending_deletes.retain(|d| !removed.contains(&d.id));
            c.pending_vault_removes
                .retain(|d| !removed_vault.contains(&d.path));
        });
        out
    }
}

fn err(e: impl std::fmt::Display) -> Response {
    Response::Error {
        message: e.to_string(),
    }
}

fn engine(cfg: &Config) -> Result<Engine, String> {
    if cfg.repo.as_os_str().is_empty() || cfg.password.is_empty() {
        return Err("backup repository is not configured".into());
    }
    Ok(Engine::new(
        Engine::default_restic(),
        cfg.repo.clone(),
        cfg.password.clone(),
        cfg.folders.clone(),
    ))
}

/// Check `password` against the repository (`restic cat config`).
fn verify_password(cfg: &Config, password: &str) -> Result<bool, String> {
    if cfg.repo.as_os_str().is_empty() {
        return Err("backup repository is not configured".into());
    }
    if password.is_empty() {
        return Ok(false);
    }
    Engine::new(
        Engine::default_restic(),
        cfg.repo.clone(),
        password.to_string(),
        cfg.folders.clone(),
    )
    .verify_password(password)
    .map_err(|e| e.to_string())
}

/// Password gate for dangerous requests, using the repo from `cfg`.
fn password_gate(cfg: &Config, ctx: &RateLimiter, password: &str) -> Result<(), Refusal> {
    gate(ctx, Instant::now(), || verify_password(cfg, password))
}

/// True when restic said the repository is not there (exit code 10), not that it failed.
fn repo_missing(message: &str) -> bool {
    message.contains("repository does not exist")
}

fn refused(r: Refusal) -> Response {
    err(r)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unix seconds as RFC 3339 UTC.
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

const KEY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A recovery key: 30 random characters of Crockford base32 in six groups of five.
pub fn generate_recovery_key() -> Result<String, String> {
    let mut b = [0u8; 30];
    getrandom::getrandom(&mut b).map_err(|e| format!("no random numbers: {e}"))?;
    let mut s = String::with_capacity(35);
    for (i, x) in b.iter().enumerate() {
        if i > 0 && i % 5 == 0 {
            s.push('-');
        }
        s.push(KEY_ALPHABET[(x & 31) as usize] as char);
    }
    Ok(s)
}

/// Does a backed-up folder live on the same physical disk as the backup volume?
fn same_physical_disk(cfg: &Config) -> bool {
    if cfg.partition_guid.is_empty() {
        return false;
    }
    let Ok(disks) = lifeboat_disk::list_disks() else {
        return false;
    };
    let Some(d) = disks.iter().find(|d| {
        d.partitions
            .iter()
            .any(|p| lifeboat_core::disk::same_guid(&p.guid, &cfg.partition_guid))
    }) else {
        return false;
    };
    cfg.folders.iter().any(|f| {
        let s = f.to_string_lossy();
        let letter = s.chars().next().filter(|c| c.is_ascii_alphabetic());
        letter.is_some_and(|l| {
            d.partitions
                .iter()
                .any(|p| p.drive_letter.is_some_and(|x| x.eq_ignore_ascii_case(&l)))
        })
    })
}

fn status(st: &State) -> Response {
    let cfg = st.snapshot_cfg();
    // A freeze state that cannot be read counts as frozen.
    let (frozen, since, reasons) = match FreezeState::load(&st.paths.freeze) {
        Ok(f) if f.frozen => (true, Some(rfc3339(f.since)), f.reasons),
        Ok(_) => (false, None, Vec::new()),
        Err(e) => (true, None, vec![format!("freeze state unreadable: {e}")]),
    };
    let last = (cfg.last_run > 0).then_some(cfg.last_run);
    Response::Status {
        version: env!("CARGO_PKG_VERSION").to_string(),
        frozen,
        frozen_since: since,
        freeze_reasons: reasons,
        same_physical_disk: same_physical_disk(&cfg),
        last_run: last,
        last_backup: last,
        last_check: cfg.last_check.clone(),
        pending_deletes: cfg.pending_deletes.clone(),
        pending_vault_removes: cfg.pending_vault_removes.clone(),
    }
}

// --- drive statistics (read-only) ---

/// Repo size on disk, remembered for a few minutes: walking a big repo is slow.
static REPO_SIZE: Mutex<Option<(Instant, u64)>> = Mutex::new(None);
const REPO_SIZE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

fn repo_size_cached(repo: &Path) -> u64 {
    let mut g = REPO_SIZE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, n)) = *g {
        if t.elapsed() < REPO_SIZE_TTL {
            return n;
        }
    }
    let n = dir_size(repo);
    *g = Some((Instant::now(), n));
    n
}

/// Free bytes on the volume at `path`, or `None` when Windows refuses.
fn free_bytes(path: &str) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let mut w: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().collect();
    if !path.ends_with('\\') {
        w.push(b'\\' as u16);
    }
    w.push(0);
    let mut free = 0u64;
    // SAFETY: `w` is NUL-terminated and outlives the call; `free` is a valid out pointer and
    // the two optional out pointers may be null.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

/// All disks plus what is known about the Lifeboat partition. Never opens the backup disk: the
/// live numbers (free space, repo size, counts) are only read while it is already open.
fn drive_stats(st: &State) -> Response {
    let cfg = st.snapshot_cfg();
    let disks = match lifeboat_disk::list_disks() {
        Ok(d) => d,
        Err(e) => return err(format!("cannot read the disks: {e}")),
    };
    drive_stats_from(st, &cfg, &disks, free_bytes)
}

fn drive_stats_from(
    st: &State,
    cfg: &Config,
    disks: &[lifeboat_core::disk::DiskInfo],
    free: impl Fn(&str) -> Option<u64>,
) -> Response {
    use lifeboat_core::drive::{drive_disks, lifeboat_disk, same_disk, LifeboatDrive};
    let guid = cfg.partition_guid.as_str();
    let home = lifeboat_disk(disks, guid);
    let lifeboat = (!guid.is_empty()).then(|| {
        let part = home.and_then(|d| {
            d.partitions
                .iter()
                .find(|p| lifeboat_core::disk::same_guid(&p.guid, guid))
        });
        let mut l = LifeboatDrive {
            disk_number: home.map(|d| d.number),
            partition_size: part.map(|p| p.size),
            same_physical_disk: same_disk(disks, guid, &cfg.folders),
            last_check: cfg.last_check.clone(),
            ..LifeboatDrive::default()
        };
        if let Some(vol) = st.ctx.lease.held_path() {
            l.open = true;
            l.free_bytes = free(&vol);
            l.used_bytes = l
                .partition_size
                .zip(l.free_bytes)
                .map(|(t, f)| t.saturating_sub(f));
            let mut c = cfg.clone();
            c.repo = PathBuf::from(&vol).join("repo");
            l.repo_bytes = Some(repo_size_cached(&c.repo));
            if !c.password.is_empty() {
                if let Ok(e) = engine(&c) {
                    if let Ok(all) = all_snapshots(&e, &c, &st.ctx.caches) {
                        let vault = all.iter().filter(|s| is_vault(s)).count();
                        l.vault_count = Some(vault as u32);
                        l.snapshot_count = Some((all.len() - vault) as u32);
                    }
                }
            }
        } else if let Some((_, n)) = *REPO_SIZE.lock().unwrap_or_else(|e| e.into_inner()) {
            l.repo_bytes = Some(n); // the last size seen
        }
        l
    });
    Response::DriveStats {
        disks: drive_disks(disks, guid),
        lifeboat,
    }
}

// --- end drive statistics ---

fn folder_strings(cfg: &Config) -> Vec<String> {
    cfg.folders
        .iter()
        .map(|f| f.to_string_lossy().to_string())
        .collect()
}

/// The message for a Lifeboat disk that already holds backups made with another password.
/// The app matches on "another password" to offer starting a new backup.
pub const OTHER_PASSWORD: &str = "This Lifeboat disk already holds backups made with another password. Type that password to keep them, or start a new backup (the old one is kept aside).";

/// What `init_repo` does with the repository folder on the Lifeboat volume.
#[derive(Debug, PartialEq, Eq)]
enum Existing {
    /// No repository there yet: create one.
    None,
    /// A repository is there and the password opens it (a retry after a step failed or the
    /// app lost the answer): reuse it.
    Reuse,
}

/// Decide from the password check what to do with the repository folder.
fn existing_repo(check: Result<(), Refusal>) -> Result<Existing, String> {
    match check {
        Ok(()) => Ok(Existing::Reuse),
        Err(Refusal::Unavailable(m)) if repo_missing(&m) => Ok(Existing::None),
        Err(Refusal::WrongPassword) => Err(OTHER_PASSWORD.into()),
        Err(r) => Err(r.to_string()),
    }
}

/// Move an old repository out of the way (never deleted) so a new one can be made.
fn set_aside(repo: &Path) -> Result<(), String> {
    if !repo.exists() {
        return Ok(());
    }
    let to = repo.with_file_name(format!("repo-old-{}", now_unix()));
    std::fs::rename(repo, &to).map_err(|e| format!("cannot move the old backup aside: {e}"))
}

/// Create the repository on the Lifeboat volume, add the recovery key, plant canaries, save.
///
/// Safe to call again: a saved repo path from an earlier, unfinished setup is replaced, and a
/// repository that the same password opens is reused (a new recovery key is made). With
/// `fresh`, an old repository with another password is moved aside first.
fn init_repo(
    st: &State,
    limiter: &RateLimiter,
    password: String,
    folders: Vec<String>,
    fresh: bool,
) -> Response {
    let cfg = st.snapshot_cfg();
    if password.is_empty() {
        return err("password must not be empty");
    }
    if folders.is_empty() {
        return err("no folders to back up");
    }
    let folders: Vec<PathBuf> = folders.into_iter().map(PathBuf::from).collect();
    // Find the volume, bringing the disk online for the duration.
    if cfg.partition_guid.is_empty() {
        return err("no backup disk is prepared yet");
    }
    let _w = st.ctx.write();
    let volume = match st.ctx.volume(&cfg.partition_guid) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let repo = PathBuf::from(&volume.path).join("repo");
    let eng = Engine::new(
        Engine::default_restic(),
        repo.clone(),
        password.clone(),
        folders.clone(),
    );
    // Wrong guesses count against the same lockout as every other password check.
    let check = gate(limiter, Instant::now(), || {
        eng.verify_password(&password).map_err(|e| e.to_string())
    });
    let existing = match existing_repo(check) {
        Ok(e) => e,
        Err(m) if fresh && m == OTHER_PASSWORD => {
            if let Err(e) = set_aside(&repo) {
                return err(e);
            }
            Existing::None
        }
        Err(m) => return err(m),
    };
    let key = match generate_recovery_key() {
        Ok(k) => k,
        Err(e) => return err(e),
    };
    if existing == Existing::None {
        if let Err(e) = eng.init() {
            return err(format!("cannot create repository: {e}"));
        }
    }
    if let Err(e) = eng.add_key(&key) {
        return err(format!("cannot add recovery key: {e}"));
    }
    if let Err(e) = cycle::plant_canaries(&folders, &st.paths.canaries) {
        return err(e);
    }
    drop(volume); // the lease keeps it for a few more seconds
    st.ctx.caches.invalidate_all();
    let saved = st.update(|c| {
        c.repo = repo;
        c.password = password;
        c.folders = folders;
    });
    match saved {
        Ok(()) => Response::RepoInitialized { recovery_key: key },
        Err(e) => err(e),
    }
}

fn set_folders(
    st: &State,
    limiter: &RateLimiter,
    folders: Vec<String>,
    password: Option<String>,
) -> Response {
    let cfg = st.snapshot_cfg();
    let new: Vec<PathBuf> = folders.into_iter().map(PathBuf::from).collect();
    let removes = cfg.folders.iter().any(|f| !new.contains(f));
    if removes {
        // Malware must not be able to silently stop backups: removing needs the password.
        let pw = password.unwrap_or_default();
        let checked = with_volume(st, &cfg, |c, _| match password_gate(c, limiter, &pw) {
            Ok(()) => Response::Done,
            Err(r) => refused(r),
        });
        if !matches!(checked, Response::Done) {
            return checked;
        }
    }
    let added: Vec<PathBuf> = new
        .iter()
        .filter(|f| !cfg.folders.contains(f))
        .cloned()
        .collect();
    if let Err(e) = cycle::plant_canaries(&added, &st.paths.canaries) {
        return err(e);
    }
    if let Err(e) = st.update(|c| c.folders = new) {
        return err(e);
    }
    if removes {
        if let Err(e) = cycle::forget_file_count(&st.paths.baseline) {
            return err(format!("folders saved, baseline not reset: {e}"));
        }
    }
    Response::Done
}

/// Run `f` with the backup repository reachable. In production the partition is hidden at
/// rest; the volume lease unhides it on first use and re-hides it after 30 s idle, so a burst
/// of requests costs one PowerShell run. The volume is reached by its GUID path (no drive
/// letter, so it is not visible in Explorer). Reads share the repository lock with each other
/// and with a running backup; only forget / prune / check exclude them. `f` gets a config
/// whose `repo` is the reachable path and, with a real volume, that volume's path.
/// Without a `partition_guid` (test mode) the repo is a plain folder and nothing is leased.
fn with_volume(
    st: &State,
    cfg: &Config,
    f: impl FnOnce(&Config, Option<&str>) -> Response,
) -> Response {
    if cfg.repo.as_os_str().is_empty() {
        return err("backup repository is not configured");
    }
    let _r = st.ctx.read();
    if cfg.partition_guid.is_empty() {
        return f(cfg, None);
    }
    let volume = match st.ctx.volume(&cfg.partition_guid) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let mut c = cfg.clone();
    c.repo = PathBuf::from(&volume.path).join("repo");
    f(&c, Some(&volume.path))
}

/// A freeze state that cannot be read counts as frozen.
fn is_frozen(st: &State) -> bool {
    FreezeState::load(&st.paths.freeze)
        .map(|f| f.frozen)
        .unwrap_or(true)
}

fn is_vault(s: &Snapshot) -> bool {
    s.tags.iter().any(|t| t == lifeboat_engine::VAULT_TAG)
}

/// Vault snapshots, newest first.
fn all_snapshots(e: &Engine, c: &Config, caches: &Caches) -> Result<Vec<Snapshot>, String> {
    caches.snapshots(&c.repo.to_string_lossy(), || {
        e.snapshots().map_err(|e| e.to_string())
    })
}

fn vault_snapshots(e: &Engine, c: &Config, caches: &Caches) -> Result<Vec<Snapshot>, String> {
    let mut v: Vec<Snapshot> = all_snapshots(e, c, caches)?
        .into_iter()
        .filter(is_vault)
        .collect();
    v.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(v)
}

/// `latest` (the newest normal backup) or an id / unique id prefix.
fn resolve_snapshot(e: &Engine, c: &Config, caches: &Caches, id: &str) -> Result<Snapshot, String> {
    if !lifeboat_engine::valid_snapshot_id(id) {
        return Err("bad snapshot id".into());
    }
    let mut all = all_snapshots(e, c, caches)?;
    if id == "latest" {
        all.retain(|s| !is_vault(s));
        all.sort_by(|a, b| a.time.cmp(&b.time));
        return all.pop().ok_or_else(|| "there are no backups yet".into());
    }
    if let Some(i) = all.iter().position(|s| s.id == id) {
        return Ok(all.swap_remove(i));
    }
    let mut m: Vec<Snapshot> = all.into_iter().filter(|s| s.id.starts_with(id)).collect();
    match m.len() {
        1 => Ok(m.remove(0)),
        0 => Err("backup not found".into()),
        _ => Err("backup id is not unique".into()),
    }
}

/// A snapshot-internal path from the UI: absolute, restic style, no `..`.
fn check_restic_path(p: &str) -> Result<(), String> {
    if !p.starts_with('/') || p.contains('\\') || p.split('/').any(|c| c == "..") {
        return Err(format!("bad path {p:?}"));
    }
    Ok(())
}

/// The snapshot's backed-up top folders as restic paths (each once).
fn top_paths(s: &Snapshot) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    s.paths
        .iter()
        .map(|p| lifeboat_engine::to_restic_path(p))
        .filter(|p| seen.insert(p.to_lowercase()))
        .collect()
}

/// Backups for the Restore list: every normal snapshot in the repository, whichever PC made
/// it (there is no host filter, so a drive opened on a new PC shows the old PC's backups),
/// with the name of the PC that made each one.
fn visible_snapshots(v: Vec<Snapshot>) -> Vec<SnapshotInfo> {
    v.into_iter()
        .filter(|s| !is_vault(s))
        .map(|s| SnapshotInfo {
            id: s.id,
            short_id: s.short_id,
            time: s.time,
            paths: s.paths,
            hostname: s.hostname,
        })
        .collect()
}

fn list_snapshots(c: &Config, caches: &Caches) -> Result<Response, String> {
    let e = engine(c)?;
    let v = all_snapshots(&e, c, caches)?;
    Ok(Response::Snapshots {
        snapshots: visible_snapshots(v),
    })
}

fn list_files(
    c: &Config,
    caches: &Caches,
    snapshot: &str,
    path: Option<&str>,
) -> Result<Response, String> {
    let e = engine(c)?;
    let snap = resolve_snapshot(&e, c, caches, snapshot)?;
    let entries = match path {
        None => {
            let mut v: Vec<FileEntry> = snap
                .paths
                .iter()
                .map(|p| FileEntry {
                    path: lifeboat_engine::to_restic_path(p),
                    name: p.clone(),
                    dir: true,
                    size: 0,
                    mtime: snap.time.clone(),
                })
                .collect();
            v.sort_by_key(|f| f.name.to_lowercase());
            v.dedup_by_key(|f| f.path.to_lowercase());
            v
        }
        Some(p) => {
            check_restic_path(p)?;
            let tree = caches.tree(&c.repo.to_string_lossy(), &snap.id);
            tree.dir(
                p,
                || e.ls_all_capped(&snap.id, MAX_TREE_NODES).ok().flatten(),
                || e.list_dir(&snap.id, p).map_err(|e| e.to_string()),
            )?
            .into_iter()
            .map(|n| FileEntry {
                path: n.path,
                name: n.name,
                dir: n.dir,
                size: n.size,
                mtime: n.mtime,
            })
            .collect()
        }
    };
    Ok(Response::Files { entries })
}

fn restore(
    c: &Config,
    caches: &Caches,
    snapshot: &str,
    target: &str,
    paths: &[String],
) -> Result<Response, String> {
    if target.trim().is_empty() {
        return Err("no folder to restore into".into());
    }
    let e = engine(c)?;
    let snap = resolve_snapshot(&e, c, caches, snapshot)?;
    let items = if paths.is_empty() {
        top_paths(&snap)
    } else {
        for p in paths {
            check_restic_path(p)?;
        }
        paths.to_vec()
    };
    if items.is_empty() {
        return Err("this backup holds no folders".into());
    }
    e.restore_groups(&[(snap.id, items)], Path::new(target))
        .map_err(|e| e.to_string())?;
    Ok(Response::Restored {
        folder: target.to_string(),
    })
}

/// Folders the vault never takes: the system, program files, the Lifeboat volume and repo.
fn protected_dirs(c: &Config, volume: Option<&str>) -> Vec<String> {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut v = vec![
        env("WINDIR", r"C:\Windows"),
        env("ProgramFiles", r"C:\Program Files"),
        env("ProgramFiles(x86)", r"C:\Program Files (x86)"),
        env("ProgramData", r"C:\ProgramData"),
        c.repo.to_string_lossy().into_owned(),
    ];
    v.extend(volume.map(str::to_string));
    v
}

/// Refuse a path the vault must never take (and so never delete): not an absolute drive-letter
/// path, a drive root, or inside / equal to one of `protected`.
pub fn refuse_vault_path(path: &str, protected: &[String]) -> Result<(), String> {
    let b = path.as_bytes();
    let drive = b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    if !drive {
        return Err(format!("{path} is not a full path with a drive letter"));
    }
    if path.split(['\\', '/']).any(|c| c == "..") {
        return Err(format!("{path} contains \"..\""));
    }
    let n = lifeboat_engine::to_restic_path(path).to_lowercase();
    if n.matches('/').count() <= 1 {
        return Err(format!("{path} is a whole drive; choose folders inside it"));
    }
    for p in protected.iter().filter(|p| !p.is_empty()) {
        let q = lifeboat_engine::to_restic_path(p).to_lowercase();
        if n == q || n.starts_with(&format!("{q}/")) {
            return Err(format!("{path} is a protected system or Lifeboat folder"));
        }
    }
    Ok(())
}

fn remove_file_force(p: &Path) -> io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let mut perm = std::fs::metadata(p)?.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            std::fs::set_permissions(p, perm)?;
            std::fs::remove_file(p)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Delete a folder: the files of each level first, then the subfolders, then the folder.
fn delete_tree(p: &Path) -> io::Result<()> {
    let mut dirs = Vec::new();
    let rd = match std::fs::read_dir(p) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in rd {
        let path = entry?.path();
        if std::fs::symlink_metadata(&path)?.is_dir() {
            dirs.push(path);
        } else {
            remove_file_force(&path)?;
        }
    }
    for d in dirs {
        delete_tree(&d)?;
    }
    match std::fs::remove_dir(p) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

fn delete_original(p: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => delete_tree(p),
        Ok(_) => remove_file_force(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Move `paths` into the vault: back up, verify every file by name and size, only then delete.
fn vault_add(
    c: &Config,
    caches: &Caches,
    volume: Option<&str>,
    paths: &[String],
) -> Result<Response, String> {
    if paths.is_empty() {
        return Err("nothing chosen to move into the vault".into());
    }
    let protected = protected_dirs(c, volume);
    for p in paths {
        refuse_vault_path(p, &protected)?;
        if std::fs::symlink_metadata(p).is_err() {
            return Err(format!("{p} does not exist"));
        }
    }
    let e = engine(c)?;
    let locals: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let snapshot = e.backup_paths(&locals, lifeboat_engine::VAULT_TAG);
    caches.invalidate_snapshots();
    let snapshot =
        snapshot.map_err(|e| format!("could not copy into the vault; nothing was deleted: {e}"))?;
    // Compare what is on disk now with what the snapshot holds.
    let mut bad = Vec::new();
    for (p, local) in paths.iter().zip(&locals) {
        let expected = lifeboat_engine::local_files(local)
            .map_err(|e| format!("cannot read {p}; nothing was deleted: {e}"))?;
        let listed = e
            .ls(&snapshot, &lifeboat_engine::to_restic_path(p), true)
            .map_err(|e| format!("cannot check the vault copy; nothing was deleted: {e}"))?;
        bad.extend(lifeboat_engine::vault_mismatches(&expected, &listed));
    }
    if !bad.is_empty() {
        let shown: Vec<&str> = bad.iter().take(5).map(String::as_str).collect();
        return Err(format!(
            "the vault copy does not match ({} file(s), e.g. {}); nothing was deleted",
            bad.len(),
            shown.join(", ")
        ));
    }
    for (p, local) in paths.iter().zip(&locals) {
        delete_original(local).map_err(|e| {
            format!("the vault copy is safe but {p} could not be fully deleted: {e}")
        })?;
    }
    Ok(Response::VaultAdded {
        snapshot,
        moved: locals.len() as u32,
    })
}

/// Size marker for a vault path that a rewrite has already removed.
const GONE: u64 = u64::MAX;

/// Match the paths the app sent to what the vault holds now, returning the vault's own spelling.
/// Characters restic would read as a pattern are refused: the removal uses `--exclude`.
fn vault_remove_targets(
    c: &Config,
    caches: &Caches,
    paths: &[String],
) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err("nothing chosen to remove from the vault".into());
    }
    let Response::Vault { items } = vault_list(c, caches)? else {
        return Err("cannot read the vault".into());
    };
    let mut out = Vec::new();
    for p in paths {
        let want = lifeboat_engine::to_restic_path(p);
        let Some(item) = items
            .iter()
            .find(|i| lifeboat_engine::to_restic_path(&i.path).eq_ignore_ascii_case(&want))
        else {
            return Err(format!("{p} is not in the vault"));
        };
        if lifeboat_engine::has_pattern_chars(&item.path) {
            return Err(format!(
                "{p} has a * ? or [ in its name, which cannot be removed safely from the vault"
            ));
        }
        if !out.contains(&item.path) {
            out.push(item.path.clone());
        }
    }
    Ok(out)
}

fn vault_list(c: &Config, caches: &Caches) -> Result<Response, String> {
    let e = engine(c)?;
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for s in vault_snapshots(&e, c, caches)? {
        for p in &s.paths {
            let rp = lifeboat_engine::to_restic_path(p);
            if !seen.insert(rp.to_lowercase()) {
                continue; // an older copy of a path added again
            }
            let (dir, size) = caches.size_of(&c.repo.to_string_lossy(), &s.id, &rp, || {
                let nodes = e.ls(&s.id, &rp, true).map_err(|e| e.to_string())?;
                if nodes.is_empty() {
                    // Removed from this snapshot by a rewrite; its paths list is stale.
                    return Ok((false, GONE));
                }
                Ok((
                    nodes
                        .iter()
                        .any(|n| n.dir && n.path.eq_ignore_ascii_case(&rp)),
                    nodes.iter().filter(|n| !n.dir).map(|n| n.size).sum(),
                ))
            })?;
            if size == GONE {
                continue;
            }
            let removes_at = c
                .pending_vault_removes
                .iter()
                .find(|d| lifeboat_engine::to_restic_path(&d.path).eq_ignore_ascii_case(&rp))
                .map(|d| d.due);
            items.push(VaultItem {
                path: p.clone(),
                dir,
                size,
                added: s.time.clone(),
                snapshot: s.id.clone(),
                removes_at,
            });
        }
    }
    Ok(Response::Vault { items })
}

fn vault_extract(
    c: &Config,
    caches: &Caches,
    paths: &[String],
    target: &str,
) -> Result<Response, String> {
    if paths.is_empty() {
        return Err("nothing chosen to take out of the vault".into());
    }
    if target.trim().is_empty() {
        return Err("no folder to copy into".into());
    }
    let e = engine(c)?;
    let snaps = vault_snapshots(&e, c, caches)?;
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for p in paths {
        let key = lifeboat_engine::to_restic_path(p).to_lowercase();
        let (snap, item) = snaps
            .iter()
            .find_map(|s| {
                s.paths
                    .iter()
                    .map(|sp| lifeboat_engine::to_restic_path(sp))
                    .find(|sp| sp.to_lowercase() == key)
                    .map(|sp| (s, sp))
            })
            .ok_or_else(|| format!("{p} is not in the vault"))?;
        match groups.iter_mut().find(|g| g.0 == snap.id) {
            Some(g) => g.1.push(item),
            None => groups.push((snap.id.clone(), vec![item])),
        }
    }
    e.restore_groups(&groups, Path::new(target))
        .map_err(|e| e.to_string())?;
    Ok(Response::Restored {
        folder: target.to_string(),
    })
}

/// Compute the reply for one request line. `limiter` counts wrong passwords; `is_admin` tells
/// whether the pipe client is an elevated administrator (asked only when a request needs it).
pub fn handle_line(
    line: &str,
    st: &State,
    limiter: &RateLimiter,
    is_admin: &dyn Fn() -> bool,
) -> Response {
    let started = std::time::Instant::now();
    let r = dispatch(line, st, limiter, is_admin);
    // Request name and time only: never the request body (it may hold a password).
    eprintln!(
        "[req] {} {} ms",
        request_name(line),
        started.elapsed().as_millis()
    );
    r
}

/// The `"cmd"` value of a request line (a variant name), or `?`.
fn request_name(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str().map(str::to_owned)))
        .filter(|n| n.len() < 40 && n.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "?".into())
}

fn dispatch(
    line: &str,
    st: &State,
    limiter: &RateLimiter,
    is_admin: &dyn Fn() -> bool,
) -> Response {
    let req = match serde_json::from_str::<Request>(line) {
        Ok(r) => r,
        Err(e) => {
            return Response::Error {
                message: format!("bad request: {e}"),
            }
        }
    };
    let cfg = st.snapshot_cfg();
    match req {
        Request::Ping => Response::Pong,
        Request::Status => status(st),
        Request::DriveStats => drive_stats(st),
        Request::Backup => match st.run_backup() {
            Outcome::Backed { snapshot, .. } => Response::BackupDone { snapshot },
            Outcome::SkippedFrozen => err("backups are frozen; unfreeze first"),
            Outcome::Attack { reasons } => err(format!(
                "possible ransomware, backups frozen: {}",
                reasons.join("; ")
            )),
            Outcome::Error(e) => err(e),
        },
        Request::ListSnapshots => with_volume(st, &cfg, |c, _| {
            list_snapshots(c, &st.ctx.caches).unwrap_or_else(err)
        }),
        Request::ListFiles { snapshot, path } => with_volume(st, &cfg, |c, _| {
            list_files(c, &st.ctx.caches, &snapshot, path.as_deref()).unwrap_or_else(err)
        }),
        Request::Restore {
            snapshot,
            target,
            paths,
            password,
        } => with_volume(st, &cfg, |c, _| {
            if let Err(r) = password_gate(c, limiter, &password) {
                return refused(r);
            }
            restore(c, &st.ctx.caches, &snapshot, &target, &paths).unwrap_or_else(err)
        }),
        Request::VaultAdd { password, paths } => {
            if is_frozen(st) {
                return err("backups are frozen; unfreeze first");
            }
            with_volume(st, &cfg, |c, volume| {
                if let Err(r) = password_gate(c, limiter, &password) {
                    return refused(r);
                }
                vault_add(c, &st.ctx.caches, volume, &paths).unwrap_or_else(err)
            })
        }
        Request::VaultList => with_volume(st, &cfg, |c, _| {
            vault_list(c, &st.ctx.caches).unwrap_or_else(err)
        }),
        Request::VaultExtract {
            password,
            paths,
            target,
        } => with_volume(st, &cfg, |c, _| {
            if let Err(r) = password_gate(c, limiter, &password) {
                return refused(r);
            }
            vault_extract(c, &st.ctx.caches, &paths, &target).unwrap_or_else(err)
        }),
        Request::VaultRemove { password, paths } => {
            let mut targets = Vec::new();
            let checked = with_volume(st, &cfg, |c, _| {
                if let Err(r) = password_gate(c, limiter, &password) {
                    return refused(r);
                }
                match vault_remove_targets(c, &st.ctx.caches, &paths) {
                    Ok(p) => {
                        targets = p;
                        Response::Done
                    }
                    Err(m) => err(m),
                }
            });
            if !matches!(checked, Response::Done) {
                return checked;
            }
            let paths = targets;
            let due = now_unix() + DELETE_DELAY_SECS;
            let r = st.update(|c| {
                for path in paths {
                    if !c.pending_vault_removes.iter().any(|d| d.path == path) {
                        c.pending_vault_removes.push(PendingVaultRemove {
                            path,
                            snapshot: String::new(),
                            due,
                        });
                    }
                }
            });
            r.map(|_| Response::Done).unwrap_or_else(err)
        }
        Request::ListDisks => match lifeboat_disk::list_disks() {
            Ok(disks) => Response::Disks { disks },
            Err(e) => err(e),
        },
        Request::PrepareDisk {
            disk,
            mode,
            password,
        } => {
            // Admin first (cheap, no password guess is spent).
            if let Err(r) = require_admin(is_admin()) {
                return refused(r);
            }
            // First setup: no repository exists yet, so there is no password to check. Once
            // one exists, preparing another disk needs it.
            if !cfg.repo.as_os_str().is_empty() {
                match password_gate(&cfg, limiter, &password) {
                    Ok(()) => {}
                    // A setup that stopped before the backup was created leaves a repo path
                    // but no repository: treat it as a first setup.
                    Err(Refusal::Unavailable(m)) if repo_missing(&m) => {}
                    Err(r) => return refused(r),
                }
            }
            match (st.prepare)(disk, mode) {
                Ok((volume_path, partition_guid)) => {
                    if let Err(e) = st.update(|c| c.partition_guid = partition_guid.clone()) {
                        return err(e);
                    }
                    Response::Prepared {
                        volume_path,
                        partition_guid,
                    }
                }
                Err(e) => err(e),
            }
        }
        Request::InitRepo {
            password,
            folders,
            fresh,
        } => {
            if let Err(r) = require_admin(is_admin()) {
                return refused(r);
            }
            init_repo(st, limiter, password, folders, fresh)
        }
        Request::DeleteSnapshots { password, ids } => {
            let checked = with_volume(st, &cfg, |c, _| {
                match password_gate(c, limiter, &password) {
                    Ok(()) => Response::Done,
                    Err(r) => refused(r),
                }
            });
            if !matches!(checked, Response::Done) {
                return checked;
            }
            let due = now_unix() + DELETE_DELAY_SECS;
            let r = st.update(|c| {
                for id in ids {
                    if !c.pending_deletes.iter().any(|d| d.id == id) {
                        c.pending_deletes.push(PendingDelete { id, due });
                    }
                }
            });
            r.map(|_| Response::Done).unwrap_or_else(err)
        }
        Request::CancelDelete { password } => {
            let checked = with_volume(st, &cfg, |c, _| {
                match password_gate(c, limiter, &password) {
                    Ok(()) => Response::Done,
                    Err(r) => refused(r),
                }
            });
            if !matches!(checked, Response::Done) {
                return checked;
            }
            st.update(|c| {
                c.pending_deletes.clear();
                c.pending_vault_removes.clear();
            })
            .map(|_| Response::Done)
            .unwrap_or_else(err)
        }
        Request::Unfreeze { password } => {
            let checked = with_volume(st, &cfg, |c, _| {
                match password_gate(c, limiter, &password) {
                    Ok(()) => Response::Done,
                    Err(r) => refused(r),
                }
            });
            if !matches!(checked, Response::Done) {
                return checked;
            }
            let mut f = FreezeState::load(&st.paths.freeze).unwrap_or_default();
            f.clear();
            f.save(&st.paths.freeze)
                .map(|_| Response::Done)
                .unwrap_or_else(err)
        }
        Request::ListFolders => Response::Folders {
            folders: folder_strings(&cfg),
        },
        Request::SetFolders { folders, password } => set_folders(st, limiter, folders, password),
        // --- Resize and delete the Lifeboat partition (#54, #55) ---
        Request::LifeboatSizeInfo => lifeboat_size_info(st, &cfg),
        Request::ResizeLifeboat { password, new_size } => {
            // Admin first, like PrepareDisk: no password guess is spent for a non-admin.
            if let Err(r) = require_admin(is_admin()) {
                return refused(r);
            }
            resize_lifeboat(st, limiter, &cfg, &password, new_size)
        }
        Request::MoveGrowLifeboat { password, new_size } => {
            if let Err(r) = require_admin(is_admin()) {
                return refused(r);
            }
            move_grow_lifeboat(st, limiter, &cfg, &password, new_size)
        }
        Request::RemoveLifeboat { password, confirm } => {
            if let Err(r) = require_admin(is_admin()) {
                return refused(r);
            }
            remove_lifeboat(st, limiter, &cfg, &password, &confirm)
        }
    }
}

/// The word the user must type to delete Lifeboat.
pub const REMOVE_CONFIRM_WORD: &str = "DELETE";

#[allow(clippy::result_large_err)]
fn lifeboat_guid(cfg: &Config) -> Result<&str, Response> {
    if cfg.partition_guid.trim().is_empty() {
        return Err(err(
            "Lifeboat does not use its own partition here, so there is nothing to resize or delete",
        ));
    }
    Ok(&cfg.partition_guid)
}

/// Read-only: size, fill level and how far Lifeboat can grow.
fn lifeboat_size_info(st: &State, cfg: &Config) -> Response {
    lifeboat_size_info_with(st, cfg, free_bytes)
}

/// Space in use is measured the same way as the Backup drive card
/// (size minus free space), and only while Lifeboat is open.
fn lifeboat_size_info_with(
    st: &State,
    cfg: &Config,
    free: impl Fn(&str) -> Option<u64>,
) -> Response {
    let guid = match lifeboat_guid(cfg) {
        Ok(g) => g,
        Err(r) => return r,
    };
    match (st.disk_ops.grow_info)(guid) {
        Ok(i) => Response::ResizeInfo {
            total: i.current_size,
            used: st
                .ctx
                .lease
                .held_path()
                .and_then(|v| free(&v))
                .map(|f| i.current_size.saturating_sub(f)),
            max_size: i.max_size,
            can_grow: i.can_grow,
            reason: i.reason,
            can_move: i.can_move,
            move_max: i.move_max,
        },
        Err(e) => err(e),
    }
}

/// Make Lifeboat bigger by moving it (#62): take space from the drive in front of it, copy
/// the backups into a new, bigger hidden partition, check the copy, switch over, and only
/// then remove the old partition. Password-gated, refused while frozen.
fn move_grow_lifeboat(
    st: &State,
    limiter: &RateLimiter,
    cfg: &Config,
    password: &str,
    new_size: u64,
) -> Response {
    if is_frozen(st) {
        return err("backups are frozen; unfreeze first");
    }
    let guid = match lifeboat_guid(cfg) {
        Ok(g) => g,
        Err(r) => return r,
    };
    if cfg.pending_move.is_some() {
        return err(
            "A previous resize is still being settled. Restart the Lifeboat service and try again.",
        );
    }
    if let Err(r) = (st.disk_ops.verify)(st, cfg, limiter, password) {
        return r;
    }
    let _w = st.ctx.write();
    if is_frozen(st) {
        return err("backups are frozen; unfreeze first");
    }
    // Measured while the volume is still open, before the lease goes.
    let used = (st.disk_ops.in_use)(st);
    st.ctx.lease.release_now();
    let result = run_move(st, cfg, guid, password, new_size, used);
    st.ctx.caches.invalidate_all();
    result
}

fn run_move(
    st: &State,
    cfg: &Config,
    old: &str,
    password: &str,
    new_size: u64,
    used: Option<u64>,
) -> Response {
    let ops = &st.disk_ops;
    let plan = match (ops.move_plan)(old) {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    let Some(used) = used else {
        return err("Lifeboat cannot tell how much space is in use right now, so it cannot make a safe copy. Try again in a moment.");
    };
    let x = match plan.check_target(used, new_size) {
        Ok(x) => x,
        Err(e) => return err(e.to_string()),
    };
    let target = plan.current_size + x;
    let mut j = PendingMove {
        stage: MoveStage::Building,
        old_guid: old.to_string(),
        new_guid: String::new(),
        host_guid: plan.host_guid.clone(),
        target,
    };
    // Written before the disk is touched, so a crash can be settled on the next start.
    if let Err(e) = st.update(|c| c.pending_move = Some(j.clone())) {
        let _ = st.update(|c| c.pending_move = None);
        return err(e);
    }
    // Undo everything up to the switch. The old partition and the config stay as they were.
    let undo = |st: &State, j: &PendingMove, why: String| -> Response {
        match (st.disk_ops.move_rollback)(&j.old_guid, &j.host_guid, &j.new_guid) {
            Ok(()) => {
                let _ = st.update(|c| {
                    c.partition_guid = j.old_guid.clone();
                    c.pending_move = None;
                });
                err(format!(
                    "{why} Nothing was changed: your backups and the old Lifeboat are untouched."
                ))
            }
            Err(e) => err(format!(
                "{why} Undoing the change also failed ({e}). Your backups are untouched; Lifeboat will try to undo it again when the service restarts."
            )),
        }
    };
    if let Err(e) = (ops.move_shrink)(&j.host_guid, x) {
        return undo(st, &j, format!("Could not take space from the drive: {e}."));
    }
    let new = match (ops.move_create)(old, x) {
        Ok(g) => g,
        Err(e) => {
            return undo(
                st,
                &j,
                format!("Could not create the bigger Lifeboat: {e}."),
            )
        }
    };
    j.new_guid = new.clone();
    if let Err(e) = st.update(|c| c.pending_move = Some(j.clone())) {
        return undo(st, &j, e);
    }
    if let Err(e) = (ops.move_copy)(old, &new) {
        return undo(st, &j, format!("Could not copy the backups: {e}."));
    }
    if let Err(e) = (ops.move_check)(cfg, &new, password) {
        return undo(st, &j, e);
    }
    // The point of no return: from here the new partition is the real one.
    j.stage = MoveStage::Switched;
    if let Err(e) = st.update(|c| {
        c.partition_guid = new.clone();
        c.pending_move = Some(j.clone());
    }) {
        j.stage = MoveStage::Building;
        return undo(st, &j, e);
    }
    if let Err(e) = (ops.move_remove_old)(old) {
        // Backups already use the new partition; the old one is removed at the next start.
        return Response::Moved {
            new_size: x,
            note: format!(
                "Lifeboat is now bigger, but the old copy could not be removed yet ({e}). It will be removed when the service restarts."
            ),
        };
    }
    let mut note = String::new();
    let size = match (ops.move_grow)(&new, target) {
        Ok(n) => n,
        Err(e) => {
            note = format!(
                "Lifeboat is bigger and your backups are safe, but it could not use all of the freed space yet ({e})."
            );
            x
        }
    };
    let _ = st.update(|c| c.pending_move = None);
    Response::Moved {
        new_size: size,
        note,
    }
}

/// Grow the Lifeboat partition. Password-gated, refused while frozen. Holds the repository
/// lock exclusively and drops the volume lease first, so no backup or read is running.
fn resize_lifeboat(
    st: &State,
    limiter: &RateLimiter,
    cfg: &Config,
    password: &str,
    new_size: u64,
) -> Response {
    if is_frozen(st) {
        return err("backups are frozen; unfreeze first");
    }
    let guid = match lifeboat_guid(cfg) {
        Ok(g) => g,
        Err(r) => return r,
    };
    if let Err(r) = (st.disk_ops.verify)(st, cfg, limiter, password) {
        return r;
    }
    let _w = st.ctx.write();
    // A freeze may have started while we waited for the lock.
    if is_frozen(st) {
        return err("backups are frozen; unfreeze first");
    }
    st.ctx.lease.release_now();
    let result = (st.disk_ops.grow)(guid, new_size);
    st.ctx.caches.invalidate_all();
    match result {
        Ok(n) => Response::Resized { new_size: n },
        Err(e) => err(e),
    }
}

/// Delete the Lifeboat partition and forget the setup. The confirm word is checked before
/// anything else, so a stray request never reaches the disk.
fn remove_lifeboat(
    st: &State,
    limiter: &RateLimiter,
    cfg: &Config,
    password: &str,
    confirm: &str,
) -> Response {
    if is_frozen(st) {
        return err("backups are frozen; unfreeze first");
    }
    if confirm != REMOVE_CONFIRM_WORD {
        return err(format!(
            "type {REMOVE_CONFIRM_WORD} to confirm deleting Lifeboat"
        ));
    }
    let guid = match lifeboat_guid(cfg) {
        Ok(g) => g,
        Err(r) => return r,
    };
    if let Err(r) = (st.disk_ops.verify)(st, cfg, limiter, password) {
        return r;
    }
    let _w = st.ctx.write();
    // A freeze may have started while we waited for the lock.
    if is_frozen(st) {
        return err("backups are frozen; unfreeze first");
    }
    st.ctx.lease.release_now();
    let note = match (st.disk_ops.remove)(guid) {
        Ok(n) => n,
        Err(e) => return err(e),
    };
    st.ctx.caches.invalidate_all();
    // Back to first-run setup: no repository, no password, no folders, no queued deletes.
    match st.update(|c| *c = Config::default()) {
        Ok(()) => Response::Removed { note },
        Err(e) => err(format!(
            "Lifeboat was deleted but the setup could not be reset: {e}"
        )),
    }
}

/// Create the hidden partition (external or internal shrink), then lock it down to the service.
fn prepare(disk: u32, mode: PrepareMode) -> lifeboat_disk::Result<lifeboat_disk::PreparedVolume> {
    let volume = match mode {
        PrepareMode::External => lifeboat_disk::prepare_external(disk)?,
        PrepareMode::InternalShrink {
            drive_letter,
            size_bytes,
        } => {
            let plan = lifeboat_disk::plan_shrink(drive_letter, size_bytes)?;
            if plan.disk_number != disk {
                return Err(lifeboat_disk::Error::Invalid(format!(
                    "volume {drive_letter}: is on disk {}, not disk {disk}",
                    plan.disk_number
                )));
            }
            lifeboat_disk::execute_shrink(&plan)?
        }
    };
    lifeboat_disk::lock_down(&volume.volume_path)?;
    let _ = lifeboat_disk::hide_lifeboat_volume(&volume.partition_guid);
    Ok(volume)
}

/// Create one pipe instance with the given security and without remote clients.
fn create_instance(
    pipe: &str,
    sd: &Descriptor,
    first: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let mut sa = sd.attributes();
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first).reject_remote_clients(true);
    // SAFETY: `sa` points at a valid descriptor that outlives this call.
    unsafe {
        opts.create_with_security_attributes_raw(pipe, &mut sa as *mut _ as *mut std::ffi::c_void)
    }
}

/// Serve on `pipe` until `stop` becomes true.
pub async fn serve(
    pipe: &str,
    sddl: &str,
    st: Arc<State>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let sd = Descriptor::from_sddl(sddl)?;
    let limiter = Arc::new(RateLimiter::default());
    let mut server = create_instance(pipe, &sd, true)?;
    loop {
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            conn = server.connect() => {
                conn?;
                let client = server;
                server = create_instance(pipe, &sd, false)?;
                let st = st.clone();
                let limiter = limiter.clone();
                // Raw server handle for the per-request admin check. The pipe object lives in
                // this task (split below) until the connection ends, so it stays valid.
                let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&client) as usize;
                tokio::spawn(async move {
                    let (r, mut w) = tokio::io::split(client);
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let st = st.clone();
                        let limiter = limiter.clone();
                        let resp = tokio::task::spawn_blocking(move || {
                            handle_line(&line, &st, &limiter, &|| pipe_client_is_admin(raw))
                        })
                        .await
                        .unwrap_or_else(err);
                        let mut out = serde_json::to_string(&resp).unwrap_or_default();
                        out.push('\n');
                        if w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    }
}

/// Serve on the default pipe with a shared state.
pub async fn serve_default(st: Arc<State>, stop: watch::Receiver<bool>) -> io::Result<()> {
    serve(PIPE_NAME, &production_sddl(), st, stop).await
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {

    #[test]
    fn restore_list_includes_other_pcs_and_names_them() {
        let snap = |id: &str, host: &str, tags: &[&str]| Snapshot {
            id: id.into(),
            short_id: id.into(),
            time: "2026-01-01T10:00:00Z".into(),
            paths: vec![],
            hostname: host.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        };
        let v = visible_snapshots(vec![
            snap("a", "THIS-PC", &[]),
            snap("b", "OLD-PC", &[]),
            snap("c", "OLD-PC", &[lifeboat_engine::VAULT_TAG]),
        ]);
        let hosts: Vec<&str> = v.iter().map(|s| s.hostname.as_str()).collect();
        assert_eq!(hosts, ["THIS-PC", "OLD-PC"]);
    }
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn repo_missing_matches_restic_exit_10_only() {
        let seen = r"restic failed (Some(10)): Fatal: repository does not exist: unable to open config file: GetFileAttributesEx \\?\Volume{x}\repo\config: The system cannot find the path specified.";
        assert!(repo_missing(seen));
        assert!(!repo_missing(
            "restic failed (Some(1)): wrong password or no key found"
        ));
        assert!(!repo_missing("backup repository is not configured"));
    }

    fn state_with(cfg: Config, dir: &std::path::Path) -> State {
        let mut st = State::new(
            cfg,
            Some(Store::at(dir.join("config.json"))),
            Paths::in_dir(dir),
        );
        st.prepare = Box::new(|_, _| Ok(("V:\\".into(), "{guid-1}".into())));
        st
    }

    fn with_repo() -> Config {
        Config {
            repo: PathBuf::from("C:/nonexistent-repo"),
            password: "pw".into(),
            ..Config::default()
        }
    }

    fn call(line: &str, st: &State, admin: bool) -> Response {
        handle_line(line, st, &RateLimiter::default(), &|| admin)
    }

    fn message(r: Response) -> String {
        match r {
            Response::Error { message } => message,
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn backup_unconfigured_is_error() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        assert!(matches!(
            call(r#"{"cmd":"Backup"}"#, &st, false),
            Response::Error { .. }
        ));
        assert!(matches!(
            call(r#"{"cmd":"ListSnapshots"}"#, &st, false),
            Response::Error { .. }
        ));
    }

    #[test]
    fn ping_status_garbage() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        assert_eq!(call(r#"{"cmd":"Ping"}"#, &st, false), Response::Pong);
        assert!(matches!(
            call(r#"{"cmd":"Status"}"#, &st, false),
            Response::Status {
                frozen: false,
                last_run: None,
                ..
            }
        ));
        assert!(matches!(call("nope", &st, false), Response::Error { .. }));
    }

    #[test]
    fn drive_stats_closed_disk_returns_what_is_known() {
        use lifeboat_core::disk::{DiskInfo, PartitionInfo, BASIC_DATA_TYPE_GUID};
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            partition_guid: "{guid-1}".into(),
            folders: vec![PathBuf::from("C:/Users")],
            ..Config::default()
        };
        let st = state_with(cfg.clone(), d.path());
        let p = |letter, guid: &str| PartitionInfo {
            number: 1,
            offset: 0,
            size: 100,
            gpt_type: BASIC_DATA_TYPE_GUID.into(),
            drive_letter: letter,
            is_hidden: false,
            no_default_letter: false,
            guid: guid.into(),
        };
        let disk = |number, partitions| DiskInfo {
            number,
            model: "m".into(),
            size: 1000,
            bus_type: "USB".into(),
            is_boot: false,
            is_system: false,
            partitions,
        };
        let disks = vec![
            disk(0, vec![p(Some('C'), "{x}")]),
            disk(2, vec![p(None, "{guid-1}")]),
        ];
        let Response::DriveStats {
            disks: out,
            lifeboat,
        } = drive_stats_from(&st, &cfg, &disks, |_| panic!("must not read a closed disk"))
        else {
            panic!("wrong reply")
        };
        assert_eq!(out.len(), 2);
        assert!(out[1].holds_lifeboat && !out[0].holds_lifeboat);
        let l = lifeboat.unwrap();
        assert_eq!(l.disk_number, Some(2));
        assert_eq!(l.partition_size, Some(100));
        assert!(!l.open && l.free_bytes.is_none() && l.snapshot_count.is_none());
        assert!(!l.same_physical_disk);
    }

    #[test]
    fn drive_stats_without_a_lifeboat_disk_has_no_lifeboat() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        let Response::DriveStats { lifeboat, .. } =
            drive_stats_from(&st, &Config::default(), &[], |_| None)
        else {
            panic!("wrong reply")
        };
        assert!(lifeboat.is_none());
    }

    #[test]
    fn status_frozen_when_state_unreadable_or_set() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        std::fs::write(&st.paths.freeze, b"{broken").unwrap();
        assert!(matches!(
            call(r#"{"cmd":"Status"}"#, &st, false),
            Response::Status { frozen: true, .. }
        ));
        let mut f = FreezeState::default();
        f.freeze(vec!["mass rename".into()]);
        f.save(&st.paths.freeze).unwrap();
        match call(r#"{"cmd":"Status"}"#, &st, false) {
            Response::Status {
                frozen,
                frozen_since,
                freeze_reasons,
                ..
            } => {
                assert!(frozen);
                assert!(frozen_since.unwrap().ends_with('Z'));
                assert_eq!(freeze_reasons, vec!["mass rename".to_string()]);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn status_reports_last_run_and_pending_deletes() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            last_run: 1000,
            pending_deletes: vec![PendingDelete {
                id: "a".into(),
                due: 5,
            }],
            ..Config::default()
        };
        let st = state_with(cfg, d.path());
        match call(r#"{"cmd":"Status"}"#, &st, false) {
            Response::Status {
                last_run,
                last_backup,
                pending_deletes,
                ..
            } => {
                assert_eq!(last_run, Some(1000));
                assert_eq!(last_backup, Some(1000));
                assert_eq!(pending_deletes.len(), 1);
            }
            o => panic!("{o:?}"),
        }
    }

    const PREPARE: &str =
        r#"{"cmd":"PrepareDisk","disk":1,"mode":{"mode":"External"},"password":""}"#;

    #[test]
    fn prepare_disk_refused_for_non_admin() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        assert_eq!(message(call(PREPARE, &st, false)), "needs administrator");
    }

    #[test]
    fn prepare_disk_allowed_for_admin_without_repo_and_saves_guid() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        assert!(matches!(
            call(PREPARE, &st, true),
            Response::Prepared { .. }
        ));
        assert_eq!(st.snapshot_cfg().partition_guid, "{guid-1}");
        // Persisted too.
        let saved = Store::at(d.path().join("config.json")).load().unwrap();
        assert_eq!(saved.partition_guid, "{guid-1}");
    }

    #[test]
    fn prepare_disk_needs_password_once_repo_exists() {
        let d = tempfile::tempdir().unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let mut st = state_with(with_repo(), d.path());
        let c = called.clone();
        st.prepare = Box::new(move |_, _| {
            c.store(true, Ordering::SeqCst);
            Ok(("V:\\".into(), "{g}".into()))
        });
        // Empty password is wrong; the disk is never touched.
        assert_eq!(message(call(PREPARE, &st, true)), "password wrong");
        assert!(!called.load(Ordering::SeqCst));
    }

    #[test]
    fn init_repo_saved_repo_path_does_not_block_a_retry() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(with_repo(), d.path());
        let line = r#"{"cmd":"InitRepo","password":"x","folders":["C:/Docs"]}"#;
        assert_eq!(message(call(line, &st, false)), "needs administrator");
        // A repo path left by an unfinished setup is no reason to refuse: it goes on to the
        // disk (here none is prepared).
        let m = message(call(line, &st, true));
        assert!(!m.contains("already exists"), "{m}");
        assert!(m.contains("no backup disk"), "{m}");
    }

    #[test]
    fn existing_repo_decision() {
        assert_eq!(existing_repo(Ok(())), Ok(Existing::Reuse));
        assert_eq!(
            existing_repo(Err(Refusal::Unavailable(
                "Fatal: repository does not exist: unable to open config file".into()
            ))),
            Ok(Existing::None)
        );
        assert_eq!(
            existing_repo(Err(Refusal::WrongPassword)),
            Err(OTHER_PASSWORD.to_string())
        );
        assert!(existing_repo(Err(Refusal::Locked(5))).is_err());
        assert!(existing_repo(Err(Refusal::Unavailable("disk gone".into()))).is_err());
    }

    #[test]
    fn set_aside_moves_never_deletes() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("config"), b"x").unwrap();
        set_aside(&repo).unwrap();
        assert!(!repo.exists());
        let moved: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().collect();
        assert_eq!(moved.len(), 1);
        let p = moved[0].path();
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("repo-old-"));
        assert!(p.join("config").exists());
        // Nothing there: nothing to do.
        set_aside(&repo).unwrap();
    }

    #[test]
    fn init_repo_fresh_defaults_to_false() {
        let r: Request =
            serde_json::from_str(r#"{"cmd":"InitRepo","password":"x","folders":[]}"#).unwrap();
        assert!(matches!(r, Request::InitRepo { fresh: false, .. }));
    }

    #[test]
    fn init_repo_without_prepared_disk_is_error() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        let m = message(call(
            r#"{"cmd":"InitRepo","password":"x","folders":["C:/Docs"]}"#,
            &st,
            true,
        ));
        assert!(m.contains("no backup disk"), "{m}");
    }

    #[test]
    fn recovery_key_format() {
        for _ in 0..20 {
            let k = generate_recovery_key().unwrap();
            assert_eq!(k.len(), 35);
            let groups: Vec<&str> = k.split('-').collect();
            assert_eq!(groups.len(), 6);
            for g in groups {
                assert_eq!(g.len(), 5);
                assert!(g.bytes().all(|b| KEY_ALPHABET.contains(&b)), "{k}");
            }
        }
        assert_ne!(
            generate_recovery_key().unwrap(),
            generate_recovery_key().unwrap()
        );
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn vault_and_browse_requests_validate() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        for line in [
            r#"{"cmd":"VaultAdd","paths":["C:/x"]}"#,
            r#"{"cmd":"VaultExtract","paths":[],"target":"C:/x"}"#,
            r#"{"cmd":"VaultRemove","paths":["C:/x"]}"#,
            r#"{"cmd":"ListFiles"}"#,
        ] {
            assert!(
                message(call(line, &st, true)).starts_with("bad request"),
                "{line}"
            );
        }
        for line in [
            r#"{"cmd":"ListFiles","snapshot":"latest","path":null}"#,
            r#"{"cmd":"VaultList"}"#,
            r#"{"cmd":"VaultAdd","password":"pw","paths":["C:/x"]}"#,
            r#"{"cmd":"VaultExtract","password":"pw","paths":["C:/x"],"target":"C:/y"}"#,
            r#"{"cmd":"VaultRemove","password":"pw","paths":["C:/x"]}"#,
        ] {
            assert!(
                message(call(line, &st, true)).contains("not configured"),
                "{line}"
            );
        }
    }

    #[test]
    fn vault_refuses_system_and_odd_paths() {
        let prot = vec![r"C:\Windows".to_string(), r"C:\Program Files".to_string()];
        for bad in [
            r"C:\",
            "D:/",
            r"C:\Windows",
            r"c:\windows\system32",
            r"C:\Program Files\App",
            r"relative\dir",
            r"\\server\share\x",
            r"C:\Users\..\Windows",
        ] {
            assert!(refuse_vault_path(bad, &prot).is_err(), "{bad}");
        }
        for good in [r"C:\Users\me\Docs", "D:/stuff/a.txt", r"C:\Windowsish"] {
            assert!(refuse_vault_path(good, &prot).is_ok(), "{good}");
        }
    }

    #[test]
    fn delete_original_removes_files_and_trees() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("t");
        std::fs::create_dir_all(t.join("a/b")).unwrap();
        std::fs::write(t.join("a/b/x.txt"), b"1").unwrap();
        std::fs::write(t.join("y.txt"), b"2").unwrap();
        delete_original(&t).unwrap();
        assert!(!t.exists());
        let f = d.path().join("f.txt");
        std::fs::write(&f, b"1").unwrap();
        delete_original(&f).unwrap();
        assert!(!f.exists());
        delete_original(&f).unwrap();
    }

    /// Real restic through `handle_line`: list, partial restore into a new folder, vault
    /// add / list / extract, and the password gates. Skipped when vendor/restic is missing.
    #[test]
    fn real_restic_browse_restore_and_vault_flows() {
        let restic_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/restic");
        let restic = restic_dir.join("restic.exe");
        if !restic.exists() {
            eprintln!("SKIPPED: vendor/restic/restic.exe missing; run scripts/fetch-restic.ps1");
            return;
        }
        // The service finds restic next to its exe or on PATH; tests use PATH.
        let mut path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![restic_dir.canonicalize().unwrap()];
        dirs.extend(std::env::split_paths(&path));
        path = std::env::join_paths(dirs).unwrap();
        std::env::set_var("PATH", path);

        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("docs");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("a [1].txt"), b"bracket").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), b"beta").unwrap();
        let repo = t.path().join("repo");
        let e = Engine::new(restic, repo.clone(), "pw", vec![src.clone()]);
        e.init().unwrap();
        let snap_id = e.backup().unwrap();

        let cfg = Config {
            repo,
            password: "pw".into(),
            folders: vec![src.clone()],
            ..Config::default()
        };
        let state_dir = t.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let st = state_with(cfg, &state_dir);
        FreezeState::default().save(&st.paths.freeze).unwrap();
        let send = |r: Request| {
            let line = serde_json::to_string(&r).unwrap();
            call(&line, &st, false)
        };

        // Top level: restic path, Windows display name.
        let Response::Files { entries } = send(Request::ListFiles {
            snapshot: snap_id.clone(),
            path: None,
        }) else {
            panic!("expected files")
        };
        assert_eq!(entries.len(), 1);
        assert!(entries[0].path.starts_with('/') && !entries[0].path.contains('\\'));
        assert_eq!(
            entries[0].name.to_lowercase(),
            src.to_string_lossy().to_lowercase()
        );
        // One level down, using the path exactly as returned.
        let Response::Files { entries: kids } = send(Request::ListFiles {
            snapshot: snap_id.clone(),
            path: Some(entries[0].path.clone()),
        }) else {
            panic!("expected files")
        };
        let mut names: Vec<_> = kids.iter().map(|k| k.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["a [1].txt", "a.txt", "sub"]);
        let bracket = kids.iter().find(|k| k.name == "a [1].txt").unwrap();
        let sub = kids.iter().find(|k| k.name == "sub").unwrap();
        assert!(sub.dir && sub.path.ends_with("/sub"));

        // Wrong password is refused and restores nothing.
        let out = t.path().join("new folder").join("restored");
        let r = send(Request::Restore {
            password: "nope".into(),
            snapshot: snap_id.clone(),
            target: out.to_string_lossy().into(),
            paths: vec![bracket.path.clone()],
        });
        assert!(matches!(r, Response::Error { .. }));
        assert!(!out.exists());

        // Partial restore into a target folder that does not exist yet.
        let r = send(Request::Restore {
            password: "pw".into(),
            snapshot: snap_id.clone(),
            target: out.to_string_lossy().into(),
            paths: vec![bracket.path.clone(), sub.path.clone()],
        });
        assert!(matches!(r, Response::Restored { .. }), "{r:?}");
        assert_eq!(std::fs::read(out.join("a [1].txt")).unwrap(), b"bracket");
        assert_eq!(
            std::fs::read(out.join("sub").join("b.txt")).unwrap(),
            b"beta"
        );
        assert!(!out.join("a.txt").exists());
        // A second restore into the now non-empty folder is refused.
        let r = send(Request::Restore {
            password: "pw".into(),
            snapshot: snap_id.clone(),
            target: out.to_string_lossy().into(),
            paths: vec![],
        });
        assert!(matches!(r, Response::Error { .. }));

        // Vault: move a folder in, see it listed, take it out again.
        let vault_dir = t.path().join("secret");
        std::fs::create_dir_all(&vault_dir).unwrap();
        std::fs::write(vault_dir.join("s.txt"), b"shh").unwrap();
        let vp = vault_dir.to_string_lossy().to_string();
        let r = send(Request::VaultAdd {
            password: "pw".into(),
            paths: vec![vp.clone()],
        });
        assert!(matches!(r, Response::VaultAdded { moved: 1, .. }), "{r:?}");
        assert!(
            !vault_dir.exists(),
            "original must be deleted after the check"
        );
        let Response::Vault { items } = send(Request::VaultList) else {
            panic!("expected vault")
        };
        assert_eq!(items.len(), 1);
        assert!(items[0].dir && items[0].path.eq_ignore_ascii_case(&vp));
        let back = t.path().join("vault out");
        let r = send(Request::VaultExtract {
            password: "pw".into(),
            paths: vec![items[0].path.clone()],
            target: back.to_string_lossy().into(),
        });
        assert!(matches!(r, Response::Restored { .. }), "{r:?}");
        assert_eq!(
            std::fs::read(back.join("secret").join("s.txt")).unwrap(),
            b"shh"
        );
        // Vault snapshots stay out of the normal list.
        let Response::Snapshots { snapshots } = send(Request::ListSnapshots) else {
            panic!("expected snapshots")
        };
        assert_eq!(snapshots.len(), 1);

        // The password gates open the real repo: right password passes, wrong one fails.
        assert!(matches!(
            send(Request::CancelDelete {
                password: "pw".into()
            }),
            Response::Done
        ));
        assert!(matches!(
            send(Request::CancelDelete {
                password: "bad".into()
            }),
            Response::Error { .. }
        ));
    }

    #[test]
    fn restic_paths_from_ui_are_checked() {
        assert!(check_restic_path("/C/Users/x").is_ok());
        for bad in ["C/x", "/C/../x", r"/C\x"] {
            assert!(check_restic_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn restore_needs_password_field() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        let r = call(
            r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","paths":[]}"#,
            &st,
            true,
        );
        assert!(message(r).starts_with("bad request"));
        let m = message(call(
            r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","paths":[],"password":""}"#,
            &st,
            true,
        ));
        assert!(m.contains("not configured"), "{m}");
    }

    #[test]
    fn empty_password_counts_as_wrong_and_locks_out() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(with_repo(), d.path());
        let l = RateLimiter::default();
        let line = r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","paths":[],"password":""}"#;
        for _ in 0..5 {
            assert_eq!(
                message(handle_line(line, &st, &l, &|| true)),
                "password wrong"
            );
        }
        assert_eq!(
            message(handle_line(line, &st, &l, &|| true)),
            "locked for 15 minutes"
        );
        // Open commands stay open while locked.
        assert_eq!(
            handle_line(r#"{"cmd":"Ping"}"#, &st, &l, &|| false),
            Response::Pong
        );
    }

    #[test]
    fn folders_add_is_open_remove_needs_password() {
        let d = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (pa, pb) = (
            a.path().to_string_lossy().replace('\\', "/"),
            b.path().to_string_lossy().replace('\\', "/"),
        );
        let mut cfg = with_repo();
        cfg.folders = vec![PathBuf::from(&pa)];
        let st = state_with(cfg, d.path());
        // Adding needs no password and plants canaries.
        let add = format!(r#"{{"cmd":"SetFolders","folders":["{pa}","{pb}"]}}"#);
        assert_eq!(call(&add, &st, false), Response::Done);
        assert_eq!(
            call(r#"{"cmd":"ListFolders"}"#, &st, false),
            Response::Folders {
                folders: vec![pa.clone(), pb.clone()]
            }
        );
        assert!(!cycle::load_canaries(&st.paths.canaries).is_empty());
        // Removing without a password is refused and changes nothing.
        let rm = format!(r#"{{"cmd":"SetFolders","folders":["{pb}"]}}"#);
        assert_eq!(message(call(&rm, &st, false)), "password wrong");
        assert_eq!(st.snapshot_cfg().folders.len(), 2);
    }

    #[test]
    fn vault_remove_needs_the_password_and_queues_nothing_without_it() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(with_repo(), d.path());
        let line = r#"{"cmd":"VaultRemove","password":"","paths":["C:/x"]}"#;
        assert_eq!(message(call(line, &st, false)), "password wrong");
        assert!(st.snapshot_cfg().pending_vault_removes.is_empty());
    }

    #[test]
    fn vault_removal_that_ran_is_dropped_from_the_live_config() {
        let d = tempfile::tempdir().unwrap();
        let mk = |path: &str| PendingVaultRemove {
            path: path.into(),
            snapshot: String::new(),
            due: 1,
        };
        let cfg = Config {
            pending_vault_removes: vec![mk("C:/a"), mk("C:/b")],
            ..with_repo()
        };
        let st = state_with(cfg.clone(), d.path());
        // A removal queued while the cycle ran must survive the merge.
        st.update(|c| c.pending_vault_removes.push(mk("C:/late")))
            .unwrap();
        struct Done;
        impl Env for Done {
            fn now(&self) -> u64 {
                10
            }
            fn mount(&self) -> Result<cycle::Mount, String> {
                Ok(cycle::Mount { disk: None })
            }
            fn offline(&self, _: u32) {}
            fn scan(&self, _: u64) -> Result<lifeboat_detect::ChangeSet, String> {
                Ok(Default::default())
            }
            fn backup(&self) -> Result<String, String> {
                Ok("s".into())
            }
            fn latest_snapshot(&self) -> Result<Option<String>, String> {
                Ok(None)
            }
            fn tag_pre_alert(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn retention(&self) -> Result<(), String> {
                Ok(())
            }
            fn check_subset(&self) -> Result<(), String> {
                Ok(())
            }
            fn forget(&self, _: &[String]) -> Result<(), String> {
                Ok(())
            }
            fn vault_remove(&self, path: &str) -> Result<(), String> {
                if path == "C:/a" {
                    Ok(())
                } else {
                    Err("busy".into())
                }
            }
        }
        st.cycle_with(&Done, cfg);
        let left: Vec<String> = st
            .snapshot_cfg()
            .pending_vault_removes
            .into_iter()
            .map(|d| d.path)
            .collect();
        assert!(!left.contains(&"C:/a".to_string()), "{left:?}");
        assert!(left.contains(&"C:/b".to_string()), "{left:?}");
        assert!(left.contains(&"C:/late".to_string()), "{left:?}");
    }

    #[test]
    fn delete_snapshots_are_queued_with_delay_and_need_password() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(with_repo(), d.path());
        let line = r#"{"cmd":"DeleteSnapshots","password":"","ids":["s1"]}"#;
        assert_eq!(message(call(line, &st, false)), "password wrong");
        assert!(st.snapshot_cfg().pending_deletes.is_empty());
        let m = message(call(r#"{"cmd":"CancelDelete","password":""}"#, &st, false));
        assert_eq!(m, "password wrong");
        let m = message(call(r#"{"cmd":"Unfreeze","password":""}"#, &st, false));
        assert_eq!(m, "password wrong");
    }

    #[tokio::test]
    async fn pipe_roundtrip() {
        let pipe = r"\\.\pipe\lifeboat-test";
        let d = tempfile::tempdir().unwrap();
        let st = Arc::new(state_with(Config::default(), d.path()));
        let (tx, rx) = watch::channel(false);
        // Permissive SDDL (Everyone): CI runners may lack the INTERACTIVE SID.
        let h = tokio::spawn(async move { serve(pipe, "D:(A;;GA;;;WD)", st, rx).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let resp = tokio::task::spawn_blocking(move || {
            lifeboat_core::ipc::client::request_on(pipe, &Request::Ping)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(resp, Response::Pong);
        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    // --- Resize and delete the Lifeboat partition (#54, #55) ---

    use std::sync::atomic::AtomicU32;

    /// A state with a Lifeboat guid, a verify that accepts the password "good", and disk
    /// steps that only count calls.
    fn disk_state(dir: &std::path::Path, calls: Arc<AtomicU32>) -> State {
        let mut cfg = with_repo();
        cfg.partition_guid = "{guid-1}".into();
        cfg.folders = vec![PathBuf::from("C:/Docs")];
        let mut st = state_with(cfg, dir);
        st.disk_ops.verify = Box::new(|_, _, _, pw| {
            if pw == "good" {
                Ok(())
            } else {
                Err(Response::Error {
                    message: "password wrong".into(),
                })
            }
        });
        let c1 = calls.clone();
        st.disk_ops.grow = Box::new(move |_, n| {
            c1.fetch_add(1, Ordering::SeqCst);
            Ok(n)
        });
        let c2 = calls.clone();
        st.disk_ops.remove = Box::new(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        });
        st.disk_ops.grow_info = Box::new(|_| {
            Ok(lifeboat_disk::GrowInfo {
                current_size: 10,
                max_size: 30,
                can_grow: true,
                reason: String::new(),
                used: Some(4),
                can_move: false,
                move_max: 10,
            })
        });
        st
    }

    fn freeze(st: &State) {
        let mut f = FreezeState::default();
        f.freeze(vec!["mass rename".into()]);
        f.save(&st.paths.freeze).unwrap();
    }

    #[test]
    fn remove_needs_the_exact_confirm_word_before_touching_the_disk() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let st = disk_state(d.path(), calls.clone());
        for word in ["", "delete", "DELETE ", "Delete", "yes"] {
            let line =
                format!(r#"{{"cmd":"RemoveLifeboat","password":"good","confirm":"{word}"}}"#);
            assert!(
                message(call(&line, &st, true)).contains("type DELETE"),
                "{word:?}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!st.snapshot_cfg().partition_guid.is_empty());
    }

    #[test]
    fn resize_and_remove_are_refused_while_frozen() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let st = disk_state(d.path(), calls.clone());
        freeze(&st);
        for line in [
            r#"{"cmd":"ResizeLifeboat","password":"good","new_size":99}"#,
            r#"{"cmd":"RemoveLifeboat","password":"good","confirm":"DELETE"}"#,
        ] {
            assert_eq!(
                message(call(line, &st, true)),
                "backups are frozen; unfreeze first"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn wrong_password_never_reaches_the_disk() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let st = disk_state(d.path(), calls.clone());
        for line in [
            r#"{"cmd":"ResizeLifeboat","password":"bad","new_size":99}"#,
            r#"{"cmd":"RemoveLifeboat","password":"bad","confirm":"DELETE"}"#,
        ] {
            assert_eq!(message(call(line, &st, true)), "password wrong");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!st.snapshot_cfg().partition_guid.is_empty());
    }

    #[test]
    fn no_partition_refuses_before_any_password_work() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = with_repo();
        cfg.partition_guid = String::new();
        let st = state_with(cfg, d.path());
        // No partition: refused before any password work.
        let m = message(call(
            r#"{"cmd":"RemoveLifeboat","password":"","confirm":"DELETE"}"#,
            &st,
            true,
        ));
        assert!(m.contains("nothing to resize or delete"), "{m}");
    }

    #[test]
    fn resize_grows_and_reports_the_new_size() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let st = disk_state(d.path(), calls.clone());
        let r = call(
            r#"{"cmd":"ResizeLifeboat","password":"good","new_size":2048}"#,
            &st,
            true,
        );
        assert_eq!(r, Response::Resized { new_size: 2048 });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn resize_error_from_the_disk_is_passed_on() {
        let d = tempfile::tempdir().unwrap();
        let mut st = disk_state(d.path(), Arc::new(AtomicU32::new(0)));
        st.disk_ops.grow = Box::new(|_, _| {
            Err("Lifeboat can't grow here because the free space is in front of it".into())
        });
        let m = message(call(
            r#"{"cmd":"ResizeLifeboat","password":"good","new_size":2048}"#,
            &st,
            true,
        ));
        assert!(m.starts_with("Lifeboat can't grow here because"), "{m}");
    }

    #[test]
    fn size_info_reports_used_total_and_max() {
        let d = tempfile::tempdir().unwrap();
        let st = disk_state(d.path(), Arc::new(AtomicU32::new(0)));
        assert_eq!(
            call(r#"{"cmd":"LifeboatSizeInfo"}"#, &st, true),
            Response::ResizeInfo {
                total: 10,
                used: None, // closed: nothing to measure
                max_size: 30,
                can_grow: true,
                reason: String::new(),
                can_move: false,
                move_max: 10,
            }
        );
    }

    #[test]
    fn size_info_used_matches_the_drive_card_when_open() {
        let d = tempfile::tempdir().unwrap();
        let st = disk_state(d.path(), Arc::new(AtomicU32::new(0)));
        let _v = st
            .ctx
            .lease
            .acquire(|| Ok(crate::lease::Volume::new("X:\\".into(), || {})))
            .unwrap();
        let mut cfg = with_repo();
        cfg.partition_guid = "{guid-1}".into();
        match lifeboat_size_info_with(&st, &cfg, |_| Some(7)) {
            Response::ResizeInfo { total, used, .. } => {
                assert_eq!(total, 10);
                assert_eq!(used, Some(3)); // size 10 minus 7 free
            }
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn remove_deletes_and_resets_the_setup() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let st = disk_state(d.path(), calls.clone());
        let r = call(
            r#"{"cmd":"RemoveLifeboat","password":"good","confirm":"DELETE"}"#,
            &st,
            true,
        );
        assert_eq!(
            r,
            Response::Removed {
                note: String::new()
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(st.snapshot_cfg(), Config::default());
        // Saved too: a restart comes back to first-run setup.
        let saved = Store::at(d.path().join("config.json")).load().unwrap();
        assert_eq!(saved, Config::default());
    }

    #[test]
    fn failed_delete_keeps_the_setup() {
        let d = tempfile::tempdir().unwrap();
        let mut st = disk_state(d.path(), Arc::new(AtomicU32::new(0)));
        st.disk_ops.remove = Box::new(|_| Err("marker file does not match".into()));
        let m = message(call(
            r#"{"cmd":"RemoveLifeboat","password":"good","confirm":"DELETE"}"#,
            &st,
            true,
        ));
        assert!(m.contains("marker"), "{m}");
        assert!(!st.snapshot_cfg().partition_guid.is_empty());
    }

    #[test]
    fn resize_and_remove_need_an_admin_before_any_password_guess() {
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let st = disk_state(d.path(), calls.clone());
        let limiter = RateLimiter::default();
        for line in [
            r#"{"cmd":"ResizeLifeboat","password":"bad","new_size":99}"#,
            r#"{"cmd":"RemoveLifeboat","password":"bad","confirm":"DELETE"}"#,
            r#"{"cmd":"ResizeLifeboat","password":"good","new_size":99}"#,
            r#"{"cmd":"RemoveLifeboat","password":"good","confirm":"DELETE"}"#,
        ] {
            let m = message(handle_line(line, &st, &limiter, &|| false));
            // The refusal is the admin one, never "password wrong".
            assert_ne!(m, "password wrong", "{line}");
            assert_eq!(m, Refusal::NeedsAdmin.to_string(), "{line}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!st.snapshot_cfg().partition_guid.is_empty());
    }

    #[test]
    fn a_freeze_that_starts_while_waiting_for_the_lock_stops_resize_and_remove() {
        for line in [
            r#"{"cmd":"ResizeLifeboat","password":"good","new_size":99}"#,
            r#"{"cmd":"RemoveLifeboat","password":"good","confirm":"DELETE"}"#,
        ] {
            let d = tempfile::tempdir().unwrap();
            let calls = Arc::new(AtomicU32::new(0));
            let mut st = disk_state(d.path(), calls.clone());
            // The freeze lands after the first check, during the password step.
            let freeze_path = st.paths.freeze.clone();
            st.disk_ops.verify = Box::new(move |_, _, _, _| {
                let mut f = FreezeState::default();
                f.freeze(vec!["mass rename".into()]);
                f.save(&freeze_path).unwrap();
                Ok(())
            });
            assert_eq!(
                message(call(line, &st, true)),
                "backups are frozen; unfreeze first",
                "{line}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{line}");
            assert!(!st.snapshot_cfg().partition_guid.is_empty());
        }
    }

    // --- Move and grow (#62) ---

    const GIB: u64 = 1024 * 1024 * 1024;
    type Log = Arc<Mutex<Vec<String>>>;

    /// A state whose move steps only write their name into a log. `fail_at` names the step
    /// that returns an error ("" for none).
    fn move_state(dir: &std::path::Path, log: Log, fail_at: &'static str) -> State {
        let mut st = disk_state(dir, Arc::new(AtomicU32::new(0)));
        let step = move |name: &'static str, log: &Log| -> Result<(), String> {
            log.lock().unwrap().push(name.to_string());
            if name == fail_at {
                Err(format!("{name} broke"))
            } else {
                Ok(())
            }
        };
        let l = log.clone();
        st.disk_ops.move_plan = Box::new(move |_| {
            l.lock().unwrap().push("plan".into());
            Ok(lifeboat_disk::MovePlan {
                disk_number: 1,
                host_guid: "{host}".into(),
                host_letter: 'D',
                current_size: 10 * GIB,
                room: 100 * GIB,
            })
        });
        st.disk_ops.in_use = Box::new(|_| Some(GIB));
        let l = log.clone();
        st.disk_ops.move_shrink = Box::new(move |_, x| step("shrink", &l).map(|_| x));
        let l = log.clone();
        st.disk_ops.move_create = Box::new(move |_, _| step("create", &l).map(|_| "{new}".into()));
        let l = log.clone();
        st.disk_ops.move_copy = Box::new(move |_, _| step("copy", &l));
        let l = log.clone();
        st.disk_ops.move_check = Box::new(move |_, _, pw| {
            assert_eq!(pw, "good");
            step("check", &l)
        });
        let l = log.clone();
        st.disk_ops.move_remove_old = Box::new(move |_| step("remove_old", &l));
        let l = log.clone();
        st.disk_ops.move_grow = Box::new(move |_, n| step("grow", &l).map(|_| n));
        let l = log.clone();
        st.disk_ops.move_rollback = Box::new(move |o, h, n| {
            l.lock().unwrap().push(format!("rollback {o} {h} {n}"));
            Ok(())
        });
        st.disk_ops.move_exists = Box::new(|_| true);
        st
    }

    const MOVE_LINE: &str =
        r#"{"cmd":"MoveGrowLifeboat","password":"good","new_size":14000000000}"#;

    fn names(log: &Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    #[test]
    fn move_grow_runs_the_steps_in_order_and_switches() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let st = move_state(d.path(), log.clone(), "");
        let r = call(MOVE_LINE, &st, true);
        assert!(
            matches!(&r, Response::Moved { note, .. } if note.is_empty()),
            "{r:?}"
        );
        assert_eq!(
            names(&log),
            [
                "plan",
                "shrink",
                "create",
                "copy",
                "check",
                "remove_old",
                "grow"
            ]
        );
        let cfg = st.snapshot_cfg();
        assert_eq!(cfg.partition_guid, "{new}");
        assert!(cfg.pending_move.is_none());
        // Saved too.
        let saved = Store::at(d.path().join("config.json")).load().unwrap();
        assert_eq!(saved.partition_guid, "{new}");
        assert!(saved.pending_move.is_none());
    }

    #[test]
    fn move_grow_failure_before_the_switch_rolls_back() {
        for step in ["shrink", "create", "copy", "check"] {
            let d = tempfile::tempdir().unwrap();
            let log: Log = Default::default();
            let st = move_state(d.path(), log.clone(), step);
            let m = message(call(MOVE_LINE, &st, true));
            assert!(m.contains(&format!("{step} broke")), "{step}: {m}");
            assert!(m.contains("untouched"), "{m}");
            let n = names(&log);
            // Nothing after the failing step except the rollback; the old one is never removed.
            assert!(!n.contains(&"remove_old".to_string()), "{step}: {n:?}");
            assert!(!n.contains(&"grow".to_string()), "{step}: {n:?}");
            let last = n.last().unwrap();
            let new = if step == "shrink" || step == "create" {
                ""
            } else {
                "{new}"
            };
            assert_eq!(
                last,
                &format!("rollback {{guid-1}} {{host}} {new}"),
                "{n:?}"
            );
            let cfg = st.snapshot_cfg();
            assert_eq!(cfg.partition_guid, "{guid-1}", "{step}");
            assert!(cfg.pending_move.is_none(), "{step}");
            let saved = Store::at(d.path().join("config.json"))
                .load()
                .unwrap_or_default();
            assert!(saved.pending_move.is_none(), "{step}");
        }
    }

    #[test]
    fn move_grow_failed_rollback_keeps_the_journal_for_restart() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let mut st = move_state(d.path(), log, "copy");
        st.disk_ops.move_rollback = Box::new(|_, _, _| Err("disk busy".into()));
        let m = message(call(MOVE_LINE, &st, true));
        assert!(m.contains("try to undo it again"), "{m}");
        let cfg = st.snapshot_cfg();
        assert_eq!(cfg.partition_guid, "{guid-1}");
        let j = cfg.pending_move.unwrap();
        assert_eq!(j.stage, MoveStage::Building);
        assert_eq!(j.new_guid, "{new}");
    }

    #[test]
    fn move_grow_final_grow_failure_still_succeeds_with_a_note() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let st = move_state(d.path(), log, "grow");
        let r = call(MOVE_LINE, &st, true);
        match r {
            Response::Moved { new_size, note } => {
                assert!(note.contains("backups are safe"), "{note}");
                assert!(new_size > 0);
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(st.snapshot_cfg().partition_guid, "{new}");
        assert!(st.snapshot_cfg().pending_move.is_none());
    }

    #[test]
    fn move_grow_old_partition_removal_failure_keeps_the_switched_journal() {
        let d = tempfile::tempdir().unwrap();
        let st = move_state(d.path(), Default::default(), "remove_old");
        let r = call(MOVE_LINE, &st, true);
        assert!(
            matches!(&r, Response::Moved { note, .. } if note.contains("old copy")),
            "{r:?}"
        );
        let cfg = st.snapshot_cfg();
        assert_eq!(cfg.partition_guid, "{new}");
        assert_eq!(cfg.pending_move.unwrap().stage, MoveStage::Switched);
    }

    #[test]
    fn move_grow_wrong_password_frozen_and_non_admin_never_reach_the_disk() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let st = move_state(d.path(), log.clone(), "");
        let bad = MOVE_LINE.replace("good", "bad");
        assert_eq!(message(call(&bad, &st, true)), "password wrong");
        let limiter = RateLimiter::default();
        assert_eq!(
            message(handle_line(MOVE_LINE, &st, &limiter, &|| false)),
            Refusal::NeedsAdmin.to_string()
        );
        freeze(&st);
        assert_eq!(
            message(call(MOVE_LINE, &st, true)),
            "backups are frozen; unfreeze first"
        );
        assert!(names(&log).is_empty(), "{:?}", names(&log));
        assert_eq!(st.snapshot_cfg().partition_guid, "{guid-1}");
    }

    #[test]
    fn move_grow_refuses_a_size_that_is_too_small_before_touching_the_disk() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let st = move_state(d.path(), log.clone(), "");
        // 10.5 GiB: the copy needs more than 0.5 GiB of new room.
        let line = r#"{"cmd":"MoveGrowLifeboat","password":"good","new_size":11000000000}"#;
        let m = message(call(line, &st, true));
        assert!(m.contains("smallest size that works"), "{m}");
        assert_eq!(names(&log), ["plan"]);
        assert!(st.snapshot_cfg().pending_move.is_none());
    }

    #[test]
    fn move_grow_refuses_when_usage_is_unknown() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let mut st = move_state(d.path(), log.clone(), "");
        st.disk_ops.in_use = Box::new(|_| None);
        let m = message(call(MOVE_LINE, &st, true));
        assert!(m.contains("cannot tell how much space"), "{m}");
        assert!(!names(&log).contains(&"shrink".to_string()));
    }

    fn journal(stage: MoveStage) -> PendingMove {
        PendingMove {
            stage,
            old_guid: "{old}".into(),
            new_guid: "{new}".into(),
            host_guid: "{host}".into(),
            target: 20 * GIB,
        }
    }

    #[test]
    fn recovery_of_a_half_built_move_deletes_the_new_partition_and_gives_the_space_back() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let st = move_state(d.path(), log.clone(), "");
        st.update(|c| {
            c.partition_guid = "{old}".into();
            c.pending_move = Some(journal(MoveStage::Building));
        })
        .unwrap();
        st.recover_move();
        assert_eq!(names(&log), ["rollback {old} {host} {new}"]);
        let cfg = st.snapshot_cfg();
        assert_eq!(cfg.partition_guid, "{old}");
        assert!(cfg.pending_move.is_none());
    }

    #[test]
    fn recovery_of_a_half_built_move_keeps_the_journal_if_the_undo_fails() {
        let d = tempfile::tempdir().unwrap();
        let mut st = move_state(d.path(), Default::default(), "");
        st.disk_ops.move_rollback = Box::new(|_, _, _| Err("busy".into()));
        st.update(|c| c.pending_move = Some(journal(MoveStage::Building)))
            .unwrap();
        st.recover_move();
        assert!(st.snapshot_cfg().pending_move.is_some());
    }

    #[test]
    fn recovery_after_the_switch_removes_the_old_partition_and_grows() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let st = move_state(d.path(), log.clone(), "");
        st.update(|c| {
            c.partition_guid = "{new}".into();
            c.pending_move = Some(journal(MoveStage::Switched));
        })
        .unwrap();
        st.recover_move();
        assert_eq!(names(&log), ["remove_old", "grow"]);
        let cfg = st.snapshot_cfg();
        assert_eq!(cfg.partition_guid, "{new}");
        assert!(cfg.pending_move.is_none());
    }

    #[test]
    fn recovery_after_the_switch_with_the_old_partition_gone_only_grows() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let mut st = move_state(d.path(), log.clone(), "");
        st.disk_ops.move_exists = Box::new(|_| false);
        st.update(|c| {
            c.partition_guid = "{new}".into();
            c.pending_move = Some(journal(MoveStage::Switched));
        })
        .unwrap();
        st.recover_move();
        assert_eq!(names(&log), ["grow"]);
        assert!(st.snapshot_cfg().pending_move.is_none());
    }

    #[test]
    fn recovery_never_removes_anything_when_the_config_does_not_match_the_journal() {
        let d = tempfile::tempdir().unwrap();
        let log: Log = Default::default();
        let st = move_state(d.path(), log.clone(), "");
        // Switched journal, but the config still points at the old partition.
        st.update(|c| {
            c.partition_guid = "{old}".into();
            c.pending_move = Some(journal(MoveStage::Switched));
        })
        .unwrap();
        st.recover_move();
        assert!(names(&log).is_empty());
        assert!(st.snapshot_cfg().pending_move.is_none());
    }

    #[test]
    fn recovery_failing_to_remove_the_old_partition_keeps_the_journal() {
        let d = tempfile::tempdir().unwrap();
        let st = move_state(d.path(), Default::default(), "remove_old");
        st.update(|c| {
            c.partition_guid = "{new}".into();
            c.pending_move = Some(journal(MoveStage::Switched));
        })
        .unwrap();
        st.recover_move();
        assert!(st.snapshot_cfg().pending_move.is_some());
    }

    #[test]
    fn size_info_carries_the_move_fields() {
        let d = tempfile::tempdir().unwrap();
        let mut st = disk_state(d.path(), Arc::new(AtomicU32::new(0)));
        st.disk_ops.grow_info = Box::new(|_| {
            Ok(lifeboat_disk::GrowInfo {
                current_size: 10,
                max_size: 10,
                can_grow: false,
                reason: "x".into(),
                used: None,
                can_move: true,
                move_max: 50,
            })
        });
        match call(r#"{"cmd":"LifeboatSizeInfo"}"#, &st, true) {
            Response::ResizeInfo {
                can_move, move_max, ..
            } => {
                assert!(can_move);
                assert_eq!(move_max, 50);
            }
            r => panic!("{r:?}"),
        }
    }
}

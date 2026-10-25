//! Named-pipe server speaking the JSON-lines protocol.

use crate::cache::{Caches, MAX_TREE_NODES};
use crate::ctx::Ctx;
use crate::cycle::{self, Env, Outcome, Paths, RealEnv};
use crate::security::{
    gate, pipe_client_is_admin, production_sddl, require_admin, Descriptor, RateLimiter, Refusal,
};
use crate::store::{self, Store};
use lifeboat_core::config::{Config, PendingDelete, DELETE_DELAY_SECS};
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
}

impl State {
    pub fn new(cfg: Config, store: Option<Store>, paths: Paths) -> Self {
        State {
            cfg: Mutex::new(cfg),
            store,
            paths,
            cycle_lock: Mutex::new(()),
            ctx: Arc::new(Ctx::new()),
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
        let _ = self.update(|c| {
            c.last_run = after.last_run;
            c.last_check = after.last_check.clone();
            c.pending_deletes.retain(|d| !removed.contains(&d.id));
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
    }
}

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

fn list_snapshots(c: &Config, caches: &Caches) -> Result<Response, String> {
    let e = engine(c)?;
    let v = all_snapshots(&e, c, caches)?;
    Ok(Response::Snapshots {
        snapshots: v
            .into_iter()
            .filter(|s| !is_vault(s))
            .map(|s| SnapshotInfo {
                id: s.id,
                short_id: s.short_id,
                time: s.time,
                paths: s.paths,
            })
            .collect(),
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
                Ok((
                    nodes
                        .iter()
                        .any(|n| n.dir && n.path.eq_ignore_ascii_case(&rp)),
                    nodes.iter().filter(|n| !n.dir).map(|n| n.size).sum(),
                ))
            })?;
            items.push(VaultItem {
                path: p.clone(),
                dir,
                size,
                added: s.time.clone(),
                snapshot: s.id.clone(),
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
            st.update(|c| c.pending_deletes.clear())
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
mod tests {
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
}

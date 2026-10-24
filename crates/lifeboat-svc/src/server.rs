//! Named-pipe server speaking the JSON-lines protocol.

use crate::cycle::{self, Env, Outcome, Paths, RealEnv};
use crate::security::{
    gate, pipe_client_is_admin, production_sddl, require_admin, Descriptor, RateLimiter, Refusal,
};
use crate::store::{self, Store};
use lifeboat_core::config::{Config, PendingDelete, DELETE_DELAY_SECS};
use lifeboat_core::disk::PrepareMode;
use lifeboat_core::ipc::{Request, Response, SnapshotInfo, PIPE_NAME};
use lifeboat_detect::FreezeState;
use lifeboat_engine::Engine;
use std::io;
use std::path::PathBuf;
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
}

impl State {
    pub fn new(cfg: Config, store: Option<Store>, paths: Paths) -> Self {
        State {
            cfg: Mutex::new(cfg),
            store,
            paths,
            cycle_lock: Mutex::new(()),
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

/// Create the repository on the Lifeboat volume, add the recovery key, plant canaries, save.
fn init_repo(st: &State, password: String, folders: Vec<String>) -> Response {
    let cfg = st.snapshot_cfg();
    if !cfg.repo.as_os_str().is_empty() {
        return err("a backup repository already exists");
    }
    if password.is_empty() {
        return err("password must not be empty");
    }
    if folders.is_empty() {
        return err("no folders to back up");
    }
    let folders: Vec<PathBuf> = folders.into_iter().map(PathBuf::from).collect();
    // Find the volume, bringing the disk online for the duration.
    let volume = match locate_volume(&cfg) {
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
    let key = match generate_recovery_key() {
        Ok(k) => k,
        Err(e) => return err(e),
    };
    if let Err(e) = eng.init() {
        return err(format!("cannot create repository: {e}"));
    }
    if let Err(e) = eng.add_key(&key) {
        return err(format!("cannot add recovery key: {e}"));
    }
    if let Err(e) = cycle::plant_canaries(&folders, &st.paths.canaries) {
        return err(e);
    }
    drop(volume);
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

/// The mounted Lifeboat volume; takes the disk offline again when dropped.
struct Located {
    path: String,
    disk: Option<u32>,
}

impl Drop for Located {
    fn drop(&mut self) {
        if let Some(d) = self.disk {
            let _ = lifeboat_disk::set_disk_offline(d);
        }
    }
}

fn locate_volume(cfg: &Config) -> Result<Located, String> {
    let guid = &cfg.partition_guid;
    if guid.is_empty() {
        return Err("no backup disk is prepared yet".into());
    }
    let (disk, shared) = cycle::disk_of(guid).ok_or("backup disk not found")?;
    let mut located = Located {
        path: String::new(),
        disk: None,
    };
    if !shared {
        lifeboat_disk::set_disk_online(disk).map_err(|e| e.to_string())?;
        located.disk = Some(disk);
    }
    located.path = lifeboat_disk::find_lifeboat_volume(guid)
        .map_err(|e| e.to_string())?
        .ok_or("backup volume not found or marker mismatch")?;
    Ok(located)
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
        if let Err(r) = password_gate(&cfg, limiter, &pw) {
            return refused(r);
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

/// Compute the reply for one request line. `limiter` counts wrong passwords; `is_admin` tells
/// whether the pipe client is an elevated administrator (asked only when a request needs it).
pub fn handle_line(
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
        Request::ListSnapshots => match engine(&cfg) {
            Ok(e) => e
                .snapshots()
                .map(|v| Response::Snapshots {
                    snapshots: v
                        .into_iter()
                        .map(|s| SnapshotInfo {
                            id: s.id,
                            short_id: s.short_id,
                            time: s.time,
                            paths: s.paths,
                        })
                        .collect(),
                })
                .unwrap_or_else(err),
            Err(m) => err(m),
        },
        Request::Restore {
            snapshot,
            target,
            path,
            password,
        } => {
            if let Err(r) = password_gate(&cfg, limiter, &password) {
                return refused(r);
            }
            match engine(&cfg) {
                Ok(e) => e
                    .restore(&snapshot, std::path::Path::new(&target), path.as_deref())
                    .map(|_| Response::Restored)
                    .unwrap_or_else(err),
                Err(m) => err(m),
            }
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
                if let Err(r) = password_gate(&cfg, limiter, &password) {
                    return refused(r);
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
        Request::InitRepo { password, folders } => {
            if let Err(r) = require_admin(is_admin()) {
                return refused(r);
            }
            init_repo(st, password, folders)
        }
        Request::DeleteSnapshots { password, ids } => {
            if let Err(r) = password_gate(&cfg, limiter, &password) {
                return refused(r);
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
            if let Err(r) = password_gate(&cfg, limiter, &password) {
                return refused(r);
            }
            st.update(|c| c.pending_deletes.clear())
                .map(|_| Response::Done)
                .unwrap_or_else(err)
        }
        Request::Unfreeze { password } => {
            if let Err(r) = password_gate(&cfg, limiter, &password) {
                return refused(r);
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
                return Err(lifeboat_disk::Error::Parse(format!(
                    "volume {drive_letter}: is on disk {}, not disk {disk}",
                    plan.disk_number
                )));
            }
            lifeboat_disk::execute_shrink(&plan)?
        }
    };
    lifeboat_disk::lock_down(&volume.volume_path)?;
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
    fn init_repo_refused_when_repo_exists_or_not_admin() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(with_repo(), d.path());
        let line = r#"{"cmd":"InitRepo","password":"x","folders":["C:/Docs"]}"#;
        assert_eq!(message(call(line, &st, false)), "needs administrator");
        let m = message(call(line, &st, true));
        assert!(m.contains("already exists"), "{m}");
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
    fn restore_needs_password_field() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(Config::default(), d.path());
        let r = call(
            r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","path":null}"#,
            &st,
            true,
        );
        assert!(message(r).starts_with("bad request"));
        let m = message(call(
            r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","path":null,"password":""}"#,
            &st,
            true,
        ));
        assert!(m.starts_with("cannot check password"), "{m}");
    }

    #[test]
    fn empty_password_counts_as_wrong_and_locks_out() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with(with_repo(), d.path());
        let l = RateLimiter::default();
        let line = r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","path":null,"password":""}"#;
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

//! Backup engine: a thin, safe wrapper around `restic.exe`.
//!
//! The repository password is passed to restic only through the
//! `RESTIC_PASSWORD` environment variable of the child process, never on the
//! command line.

use serde::Deserialize;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

/// Engine errors.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// restic exited with an error.
    Restic {
        code: Option<i32>,
        stderr: String,
    },
    /// The backup disk has no space left; old backups are untouched.
    DiskFull {
        stderr: String,
    },
    /// restic output could not be parsed.
    Parse(String),
    /// Restore target refused by the safety guard.
    UnsafeTarget(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Restic { code, stderr } => write!(f, "restic failed ({code:?}): {stderr}"),
            Error::DiskFull { .. } => f.write_str(DISK_FULL_MESSAGE),
            Error::Parse(m) => write!(f, "cannot parse restic output: {m}"),
            Error::UnsafeTarget(m) => write!(f, "unsafe restore target: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// Plain message shown to the user when the backup disk is full.
pub const DISK_FULL_MESSAGE: &str =
    "The backup disk is full. Old backups are kept. Free space or use a bigger disk.";

/// True when restic output says the target ran out of space (Unix ENOSPC, Windows error 112).
pub fn is_disk_full(stderr: &str) -> bool {
    let l = stderr.to_lowercase();
    l.contains("no space left on device")
        || l.contains("there is not enough space on the disk")
        || l.contains("enospc")
        || l.contains("os error 112")
        || l.contains("error 112")
        || l.contains("errno 112")
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// One snapshot as reported by `restic snapshots --json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Snapshot {
    pub id: String,
    #[serde(default)]
    pub short_id: String,
    pub time: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Tag put on the latest snapshot when an attack is detected; retention never removes it.
pub const PRE_ALERT_TAG: &str = "pre-alert";

/// Tag put on vault snapshots (files moved into the safe); retention never removes them.
pub const VAULT_TAG: &str = "vault";

/// Retention policy arguments for `restic forget` (kept in one place so it is tested).
pub fn retention_args() -> Vec<String> {
    [
        "forget",
        "--keep-hourly",
        "48",
        "--keep-daily",
        "30",
        "--keep-weekly",
        "12",
        "--keep-monthly",
        "24",
        "--keep-tag",
        PRE_ALERT_TAG,
        "--keep-tag",
        VAULT_TAG,
        "--prune",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Arguments of the weekly integrity check.
pub fn check_subset_args() -> Vec<String> {
    vec!["check".into(), "--read-data-subset=5%".into()]
}

/// Arguments to delete specific snapshots and free their data.
pub fn forget_ids_args(ids: &[String]) -> Vec<String> {
    let mut v = vec!["forget".to_string(), "--prune".to_string()];
    v.extend(ids.iter().cloned());
    v
}

/// Arguments to drop one path from a snapshot: `rewrite` writes a copy without it and, with
/// `--forget`, forgets the original. The data is freed by a separate `prune`.
pub fn rewrite_exclude_args(snapshot: &str, restic_path: &str) -> Vec<String> {
    vec![
        "rewrite".into(),
        "--exclude".into(),
        restic_path.into(),
        "--forget".into(),
        snapshot.into(),
    ]
}

/// A path restic would read as an exclude pattern with wildcards; such a path cannot be
/// removed with `rewrite --exclude` without risking more than the one item.
pub fn has_pattern_chars(restic_path: &str) -> bool {
    restic_path.contains(['*', '?', '['])
}

/// Parse the output of `restic snapshots --json`.
pub fn parse_snapshots(json: &str) -> Result<Vec<Snapshot>, Error> {
    serde_json::from_str(json.trim()).map_err(|e| Error::Parse(e.to_string()))
}

/// Parse the `--json` line stream of `restic backup` and return the id of the
/// created snapshot (from the final `summary` message).
pub fn parse_backup_summary(stdout: &str) -> Result<String, Error> {
    for line in stdout.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if v.get("message_type").and_then(|m| m.as_str()) == Some("summary") {
            if let Some(id) = v.get("snapshot_id").and_then(|s| s.as_str()) {
                return Ok(id.to_string());
            }
        }
    }
    Err(Error::Parse("no backup summary with snapshot_id".into()))
}

/// One node of `restic ls --json` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsNode {
    pub name: String,
    /// Path inside the snapshot, restic style (`/C/Users/x`).
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: String,
}

/// Windows path (`C:\Users\x`, `C:/Users/x`) to the form restic stores (`/C/Users/x`).
/// Without a drive letter the path just gets forward slashes and a leading `/`.
pub fn to_restic_path(win: &str) -> String {
    let s = win.replace('\\', "/");
    let s = s.trim_end_matches('/');
    let b = s.as_bytes();
    let rest = if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        format!("{}{}", &s[..1], &s[2..])
    } else {
        s.to_string()
    };
    format!("/{}", rest.trim_start_matches('/'))
}

/// restic path (`/C/Users/x`) to a Windows path (`C:\Users\x`); `/C` is `C:\`.
pub fn to_windows_path(restic: &str) -> String {
    let t = restic.trim_matches('/');
    let mut parts = t.splitn(2, '/');
    let first = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    if first.len() == 1 && first.as_bytes()[0].is_ascii_alphabetic() {
        format!("{}:\\{}", first, rest.replace('/', "\\"))
    } else {
        t.replace('/', "\\")
    }
}

/// Split a restic path into (parent, name): `/C/Users/x` -> (`/C/Users`, `x`).
pub fn split_restic(path: &str) -> (String, String) {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(i) => (t[..i].to_string(), t[i + 1..].to_string()),
        None => (String::new(), t.to_string()),
    }
}

/// restic reads path filters (`ls`, `--include`) as glob patterns; a literal `[` must be
/// written `[[]` (a backslash escape is rejected by restic). `*` and `?` are escaped the same
/// way, as `[*]` and `[?]`.
pub fn escape_pattern(p: &str) -> String {
    let mut o = String::with_capacity(p.len());
    for c in p.chars() {
        match c {
            '[' => o.push_str("[[]"),
            '*' => o.push_str("[*]"),
            '?' => o.push_str("[?]"),
            c => o.push(c),
        }
    }
    o
}

/// Parse the node lines of `restic ls --json` (the leading snapshot line is skipped).
pub fn parse_ls(stdout: &str) -> Vec<LsNode> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let (Some(name), Some(path), Some(ty)) = (
            v.get("name").and_then(|x| x.as_str()),
            v.get("path").and_then(|x| x.as_str()),
            v.get("type").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        out.push(LsNode {
            name: name.to_string(),
            path: path.to_string(),
            dir: ty == "dir",
            size: v.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            mtime: v
                .get("mtime")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

/// Direct children of `dir` among `nodes`: folders first, then files, each sorted by name.
pub fn children_of(nodes: Vec<LsNode>, dir: &str) -> Vec<LsNode> {
    let dir = dir.trim_end_matches('/');
    let mut v: Vec<LsNode> = nodes
        .into_iter()
        .filter(|n| split_restic(&n.path).0 == dir)
        .collect();
    v.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    v
}

/// Files under `local` (a Windows file or folder) as `(restic path, size)`, walking the disk.
pub fn local_files(local: &Path) -> std::io::Result<Vec<(String, u64)>> {
    fn walk(p: &Path, out: &mut Vec<(String, u64)>) -> std::io::Result<()> {
        let md = std::fs::symlink_metadata(p)?;
        if md.is_dir() {
            for e in std::fs::read_dir(p)? {
                walk(&e?.path(), out)?;
            }
        } else {
            out.push((to_restic_path(&p.to_string_lossy()), md.len()));
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(local, &mut out)?;
    Ok(out)
}

/// Which of `expected` `(restic path, size)` files are missing from the snapshot listing or
/// have a different size? Empty means everything is safely in the snapshot.
pub fn vault_mismatches(expected: &[(String, u64)], listed: &[LsNode]) -> Vec<String> {
    let have: std::collections::HashMap<String, u64> = listed
        .iter()
        .filter(|n| !n.dir)
        .map(|n| (n.path.to_lowercase(), n.size))
        .collect();
    expected
        .iter()
        .filter(|(p, size)| have.get(&p.to_lowercase()) != Some(size))
        .map(|(p, _)| to_windows_path(p))
        .collect()
}

/// A snapshot id as given by the UI: hex or `latest`, never anything restic could read as
/// an option or a `snapshot:subfolder` split.
pub fn valid_snapshot_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Make a path comparable: resolve symlinks of the longest existing ancestor,
/// append the not-yet-existing rest, and lowercase (Windows is case-insensitive).
pub fn normalize(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let base = loop {
        if let Ok(c) = std::fs::canonicalize(&existing) {
            break c;
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => break path.to_path_buf(),
        }
    };
    let mut out = PathBuf::new();
    for c in base.components() {
        match c {
            Component::Normal(s) => out.push(s.to_string_lossy().to_lowercase()),
            other => out.push(other.as_os_str().to_string_lossy().to_lowercase()),
        }
    }
    for name in rest.iter().rev() {
        if name == ".." {
            out.pop();
        } else if name != "." {
            out.push(name.to_string_lossy().to_lowercase());
        }
    }
    out
}

/// Refuse a restore target that is inside (or equal to) a backed-up source,
/// or that is an existing non-empty folder / file.
pub fn check_restore_target(target: &Path, sources: &[PathBuf]) -> Result<(), Error> {
    let t = normalize(target);
    for s in sources {
        if t.starts_with(normalize(s)) {
            return Err(Error::UnsafeTarget(format!(
                "{} is inside backed-up folder {}",
                target.display(),
                s.display()
            )));
        }
    }
    if target.exists() {
        if !target.is_dir() {
            return Err(Error::UnsafeTarget(format!(
                "{} exists and is not a folder",
                target.display()
            )));
        }
        if std::fs::read_dir(target)?.next().is_some() {
            return Err(Error::UnsafeTarget(format!(
                "{} is not empty",
                target.display()
            )));
        }
    }
    Ok(())
}

/// restore recreates parent folders such as `C:\Users` and cannot always set
/// their timestamps (access denied). That is harmless: file data is intact.
fn only_timestamp_errors(stderr: &str) -> bool {
    let mut seen = false;
    for l in stderr.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if l.contains("failed to restore timestamp") {
            seen = true;
        } else if !l.starts_with("Fatal: There were") {
            return false;
        }
    }
    seen
}

/// restic exits with code 12 when the repository password is wrong.
const RESTIC_WRONG_PASSWORD: i32 = 12;

/// Map the result of `restic cat config` to "password is right".
fn probe_result(r: Result<Output, Error>) -> Result<bool, Error> {
    match r {
        Ok(_) => Ok(true),
        Err(Error::Restic {
            code: Some(RESTIC_WRONG_PASSWORD),
            ..
        }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Sort like [`children_of`]: folders first, then files, each by lowercase name.
pub fn sort_nodes(v: &mut [LsNode]) {
    v.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Group a whole-snapshot listing by parent folder (restic path, no trailing `/`); every
/// group is sorted like [`children_of`].
pub fn group_by_dir(nodes: Vec<LsNode>) -> std::collections::HashMap<String, Vec<LsNode>> {
    let mut m: std::collections::HashMap<String, Vec<LsNode>> = std::collections::HashMap::new();
    for n in nodes {
        m.entry(split_restic(&n.path).0).or_default().push(n);
    }
    for v in m.values_mut() {
        sort_nodes(v);
    }
    m
}

/// Wrapper around one restic repository.
#[derive(Debug, Clone)]
pub struct Engine {
    restic: PathBuf,
    repo: PathBuf,
    password: String,
    /// Folders that are backed up; restores never go into these.
    sources: Vec<PathBuf>,
}

impl Engine {
    pub fn new(
        restic: impl Into<PathBuf>,
        repo: impl Into<PathBuf>,
        password: impl Into<String>,
        sources: Vec<PathBuf>,
    ) -> Self {
        Engine {
            restic: restic.into(),
            repo: repo.into(),
            password: password.into(),
            sources,
        }
    }

    /// Bundled restic next to the running exe if present, else `restic` from PATH.
    pub fn default_restic() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let p = dir.join("restic.exe");
                if p.exists() {
                    return p;
                }
            }
        }
        PathBuf::from("restic")
    }

    fn run<I, S>(&self, args: I) -> Result<Output, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = Command::new(&self.restic)
            .arg("--repo")
            .arg(&self.repo)
            .arg("--no-cache")
            .args(args)
            .env("RESTIC_PASSWORD", &self.password)
            .env_remove("RESTIC_PASSWORD_FILE")
            .env_remove("RESTIC_PASSWORD_COMMAND")
            .output()?;
        if out.status.success() {
            Ok(out)
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if is_disk_full(&stderr) {
                return Err(Error::DiskFull { stderr });
            }
            Err(Error::Restic {
                code: out.status.code(),
                stderr,
            })
        }
    }

    /// Create the repository.
    pub fn init(&self) -> Result<(), Error> {
        self.run(["init"]).map(|_| ())
    }

    /// Back up the configured sources; returns the new snapshot id.
    pub fn backup(&self) -> Result<String, Error> {
        let mut args: Vec<&OsStr> = vec![OsStr::new("backup"), OsStr::new("--json")];
        args.extend(self.sources.iter().map(|s| s.as_os_str()));
        let out = self.run(args)?;
        parse_backup_summary(&String::from_utf8_lossy(&out.stdout))
    }

    /// List snapshots.
    pub fn snapshots(&self) -> Result<Vec<Snapshot>, Error> {
        let out = self.run(["snapshots", "--json"])?;
        parse_snapshots(&String::from_utf8_lossy(&out.stdout))
    }

    /// Restore a snapshot (or only `include` from it) into a new folder.
    pub fn restore(
        &self,
        snapshot_id: &str,
        target: &Path,
        include: Option<&str>,
    ) -> Result<(), Error> {
        let mut guard: Vec<PathBuf> = self.sources.clone();
        if let Some(s) = self
            .snapshots()?
            .iter()
            .find(|s| s.id.starts_with(snapshot_id))
        {
            guard.extend(s.paths.iter().map(PathBuf::from));
        }
        check_restore_target(target, &guard)?;
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("restore"),
            OsStr::new(snapshot_id),
            OsStr::new("--target"),
            target.as_os_str(),
        ];
        if let Some(p) = include {
            args.push(OsStr::new("--include"));
            args.push(OsStr::new(p));
        }
        match self.run(args) {
            Err(Error::Restic { stderr, .. }) if only_timestamp_errors(&stderr) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Back up only `paths` (absolute Windows paths) with `tag`; returns the new snapshot id.
    pub fn backup_paths(&self, paths: &[PathBuf], tag: &str) -> Result<String, Error> {
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("backup"),
            OsStr::new("--json"),
            OsStr::new("--tag"),
            OsStr::new(tag),
        ];
        args.extend(paths.iter().map(|s| s.as_os_str()));
        let out = self.run(args)?;
        parse_backup_summary(&String::from_utf8_lossy(&out.stdout))
    }

    /// Nodes of a snapshot at `restic_path`: the node itself and, for a folder, its direct
    /// children (with `recursive`, everything below).
    pub fn ls(
        &self,
        snapshot_id: &str,
        restic_path: &str,
        recursive: bool,
    ) -> Result<Vec<LsNode>, Error> {
        if !valid_snapshot_id(snapshot_id) {
            return Err(Error::Parse(format!("bad snapshot id {snapshot_id:?}")));
        }
        let pat = escape_pattern(restic_path);
        let mut args = vec!["ls", "--json"];
        if recursive {
            args.push("--recursive");
        }
        args.push(snapshot_id);
        args.push(&pat);
        let out = self.run(args)?;
        Ok(parse_ls(&String::from_utf8_lossy(&out.stdout)))
    }

    /// Every node of a snapshot from one `restic ls --json <id>` run. Gives up (kills restic,
    /// returns `None`) when the snapshot has more than `max_nodes` nodes, so memory stays bounded.
    pub fn ls_all_capped(
        &self,
        snapshot_id: &str,
        max_nodes: usize,
    ) -> Result<Option<Vec<LsNode>>, Error> {
        use std::io::{BufRead, BufReader, Read};
        use std::process::Stdio;
        if !valid_snapshot_id(snapshot_id) {
            return Err(Error::Parse(format!("bad snapshot id {snapshot_id:?}")));
        }
        let mut child = Command::new(&self.restic)
            .arg("--repo")
            .arg(&self.repo)
            .arg("--no-cache")
            .args(["ls", "--json", snapshot_id])
            .env("RESTIC_PASSWORD", &self.password)
            .env_remove("RESTIC_PASSWORD_FILE")
            .env_remove("RESTIC_PASSWORD_COMMAND")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take();
        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_string(&mut s);
            }
            s
        });
        let mut nodes = Vec::new();
        let mut too_big = false;
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                nodes.extend(parse_ls(&line));
                if nodes.len() > max_nodes {
                    too_big = true;
                    break;
                }
            }
        }
        if too_big {
            let _ = child.kill();
            let _ = child.wait();
            let _ = err_thread.join();
            return Ok(None);
        }
        let status = child.wait()?;
        let stderr = err_thread.join().unwrap_or_default().trim().to_string();
        if !status.success() {
            if is_disk_full(&stderr) {
                return Err(Error::DiskFull { stderr });
            }
            return Err(Error::Restic {
                code: status.code(),
                stderr,
            });
        }
        Ok(Some(nodes))
    }

    /// One folder level: the direct children of `restic_path`.
    pub fn list_dir(&self, snapshot_id: &str, restic_path: &str) -> Result<Vec<LsNode>, Error> {
        Ok(children_of(
            self.ls(snapshot_id, restic_path, false)?,
            restic_path,
        ))
    }

    /// Restore `items` (restic paths) from a snapshot into `target`; each lands directly in
    /// `target` under its own name. `target` is checked once, before anything is written.
    pub fn restore_items(
        &self,
        snapshot_id: &str,
        target: &Path,
        items: &[String],
    ) -> Result<(), Error> {
        self.restore_groups(&[(snapshot_id.to_string(), items.to_vec())], target)
    }

    /// Like [`Engine::restore_items`] for items spread over several snapshots (the vault keeps
    /// each item in the snapshot it was added with). One target check up front, one name space.
    pub fn restore_groups(
        &self,
        groups: &[(String, Vec<String>)],
        target: &Path,
    ) -> Result<(), Error> {
        let mut names = std::collections::HashSet::new();
        for (id, items) in groups {
            if !valid_snapshot_id(id) {
                return Err(Error::Parse(format!("bad snapshot id {id:?}")));
            }
            for it in items {
                let (_, name) = split_restic(it);
                if name.is_empty() || !names.insert(name.to_lowercase()) {
                    return Err(Error::UnsafeTarget(format!(
                        "two chosen items would both be restored as \"{name}\""
                    )));
                }
            }
        }
        let mut guard: Vec<PathBuf> = self.sources.clone();
        let all = self.snapshots()?;
        for (id, _) in groups {
            for s in all.iter().filter(|s| s.id.starts_with(id.as_str())) {
                guard.extend(s.paths.iter().map(PathBuf::from));
            }
        }
        check_restore_target(target, &guard)?;
        for (id, items) in groups {
            for it in items {
                let (parent, name) = split_restic(it);
                let spec = format!("{id}:{}", if parent.is_empty() { "/" } else { &parent });
                let inc = escape_pattern(&format!("/{name}"));
                let args: Vec<&OsStr> = vec![
                    OsStr::new("restore"),
                    OsStr::new(&spec),
                    OsStr::new("--target"),
                    target.as_os_str(),
                    OsStr::new("--include"),
                    OsStr::new(&inc),
                ];
                match self.run(args) {
                    Err(Error::Restic { stderr, .. }) if only_timestamp_errors(&stderr) => {}
                    Err(e) => return Err(e),
                    Ok(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Is `password` the repository's password? Runs `restic cat config` with that password
    /// (the engine's own configured password is not used). `Ok(false)` only for restic's
    /// "wrong password" exit code; any other failure (missing repo, I/O) is an `Err`.
    pub fn verify_password(&self, password: &str) -> Result<bool, Error> {
        let probe = Engine {
            password: password.to_string(),
            ..self.clone()
        };
        probe_result(probe.run(["cat", "config"]))
    }

    /// Add a tag to a snapshot.
    pub fn tag_add(&self, snapshot_id: &str, tag: &str) -> Result<(), Error> {
        self.run(["tag", "--add", tag, snapshot_id]).map(|_| ())
    }

    /// Apply the retention policy (never removes `pre-alert` snapshots) and prune.
    pub fn apply_retention(&self) -> Result<(), Error> {
        self.run(retention_args()).map(|_| ())
    }

    /// Delete the given snapshots and prune.
    pub fn forget_snapshots(&self, ids: &[String]) -> Result<(), Error> {
        if ids.is_empty() {
            return Ok(());
        }
        self.run(forget_ids_args(ids)).map(|_| ())
    }

    /// Remove one path from a snapshot (the snapshot is replaced by a copy without it) and
    /// free the data. `restic_path` is the restic form (`/C/Users/x`).
    pub fn rewrite_exclude(&self, snapshot: &str, restic_path: &str) -> Result<(), Error> {
        if !valid_snapshot_id(snapshot) || has_pattern_chars(restic_path) {
            return Err(Error::Parse(format!("cannot rewrite for {restic_path}")));
        }
        self.run(rewrite_exclude_args(snapshot, restic_path))?;
        self.run(["prune"]).map(|_| ())
    }

    /// Integrity check that also reads a 5% sample of the data.
    pub fn check_subset(&self) -> Result<(), Error> {
        self.run(check_subset_args()).map(|_| ())
    }

    /// Add a second password (the recovery key). It goes through a short-lived password file
    /// because restic has no other non-interactive way; the file is removed afterwards.
    pub fn add_key(&self, new_password: &str) -> Result<(), Error> {
        let mut path = std::env::temp_dir();
        path.push(format!("lifeboat-key-{}.tmp", std::process::id()));
        std::fs::write(&path, new_password)?;
        let r = self.run([
            OsStr::new("key"),
            OsStr::new("add"),
            OsStr::new("--new-password-file"),
            path.as_os_str(),
        ]);
        let _ = std::fs::remove_file(&path);
        r.map(|_| ())
    }

    /// Verify repository integrity.
    pub fn check(&self) -> Result<(), Error> {
        self.run(["check"]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_keeps_pre_alert_and_prunes() {
        let a = retention_args().join(" ");
        assert_eq!(
            a,
            "forget --keep-hourly 48 --keep-daily 30 --keep-weekly 12 --keep-monthly 24 --keep-tag pre-alert --keep-tag vault --prune"
        );
        assert_eq!(check_subset_args(), ["check", "--read-data-subset=5%"]);
        assert_eq!(
            forget_ids_args(&["a".into(), "b".into()]),
            ["forget", "--prune", "a", "b"]
        );
    }

    #[test]
    fn snapshot_tags_parse() {
        let j = r#"[{"time":"t","id":"a","tags":["pre-alert"]},{"time":"t","id":"b"}]"#;
        let s = parse_snapshots(j).unwrap();
        assert_eq!(s[0].tags, ["pre-alert"]);
        assert!(s[1].tags.is_empty());
    }

    #[test]
    fn disk_full_stderr_is_recognised() {
        for s in [
            "Fatal: unable to save snapshot: write x: no space left on device",
            "write R: repo data: There is not enough space on the disk.",
            "write: ENOSPC",
            "WriteFile: Windows error 112",
        ] {
            assert!(is_disk_full(s), "{s}");
        }
        assert!(!is_disk_full("Fatal: wrong password or no key found"));
        let e = Error::DiskFull { stderr: "x".into() };
        assert_eq!(e.to_string(), DISK_FULL_MESSAGE);
    }

    #[test]
    fn wrong_password_is_exit_12() {
        let wrong = Error::Restic {
            code: Some(12),
            stderr: "Fatal: wrong password or no key found".into(),
        };
        assert!(!probe_result(Err(wrong)).unwrap());
        let missing = Error::Restic {
            code: Some(10),
            stderr: "repository does not exist".into(),
        };
        assert!(probe_result(Err(missing)).is_err());
        assert!(probe_result(Err(Error::Parse("x".into()))).is_err());
    }

    #[test]
    fn parses_snapshots() {
        let j = r#"[{"time":"2026-01-01T10:00:00Z","paths":["C:\\Docs"],"hostname":"pc","id":"abcdef0123","short_id":"abcdef01","tree":"x"}]"#;
        let s = parse_snapshots(j).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].short_id, "abcdef01");
        assert_eq!(s[0].paths, vec!["C:\\Docs"]);
        assert!(parse_snapshots("null").is_err());
    }

    #[test]
    fn parses_summary() {
        let o = "{\"message_type\":\"status\",\"percent_done\":0.5}\n{\"message_type\":\"summary\",\"snapshot_id\":\"deadbeef\"}\n";
        assert_eq!(parse_backup_summary(o).unwrap(), "deadbeef");
        assert!(parse_backup_summary("{\"message_type\":\"status\"}").is_err());
    }

    #[test]
    fn guard_rejects_inside_and_equal_case_insensitive() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("Docs");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let srcs = vec![src.clone()];
        assert!(check_restore_target(&src, &srcs).is_err());
        let upper = PathBuf::from(src.to_string_lossy().to_uppercase());
        assert!(check_restore_target(&upper, &srcs).is_err());
        assert!(check_restore_target(&src.join("sub"), &srcs).is_err());
        assert!(check_restore_target(&src.join("new").join("deeper"), &srcs).is_err());
        assert!(check_restore_target(&src.join("sub").join("..").join("x"), &srcs).is_err());
    }

    #[test]
    fn guard_rejects_nonempty_allows_new_and_empty() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("Docs");
        std::fs::create_dir_all(&src).unwrap();
        let srcs = vec![src];
        let full = d.path().join("full");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("a.txt"), b"x").unwrap();
        assert!(check_restore_target(&full, &srcs).is_err());
        let empty = d.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(check_restore_target(&empty, &srcs).is_ok());
        assert!(check_restore_target(&d.path().join("brand-new"), &srcs).is_ok());
        // sibling with the same prefix is not "inside"
        assert!(check_restore_target(&d.path().join("Docs2"), &srcs).is_ok());
    }

    #[test]
    fn path_mapping_round_trips() {
        assert_eq!(to_restic_path(r"C:\Users\x"), "/C/Users/x");
        assert_eq!(to_restic_path("C:/Users/x/"), "/C/Users/x");
        assert_eq!(to_restic_path(r"D:\"), "/D");
        assert_eq!(to_windows_path("/C/Users/x"), r"C:\Users\x");
        assert_eq!(to_windows_path("/C"), r"C:\");
        assert_eq!(
            to_windows_path(&to_restic_path(r"E:\a b\c.txt")),
            r"E:\a b\c.txt"
        );
        assert_eq!(
            split_restic("/C/Users/x"),
            ("/C/Users".to_string(), "x".to_string())
        );
        assert_eq!(split_restic("/C"), (String::new(), "C".to_string()));
    }

    #[test]
    fn patterns_are_escaped() {
        assert_eq!(escape_pattern("/a[1].txt"), "/a[[]1].txt");
        assert_eq!(escape_pattern("/a*b?"), "/a[*]b[?]");
        assert_eq!(escape_pattern("/plain"), "/plain");
    }

    fn node(path: &str, dir: bool, size: u64) -> String {
        let (_, name) = split_restic(path);
        format!(
            r#"{{"name":"{name}","type":"{}","path":"{path}","size":{size},"mtime":"2026-01-01T10:00:00Z","struct_type":"node"}}"#,
            if dir { "dir" } else { "file" }
        )
    }

    fn listing() -> String {
        let head = r#"{"message_type":"snapshot","struct_type":"snapshot","id":"abc"}"#;
        [
            head.to_string(),
            node("/C/Docs", true, 0),
            node("/C/Docs/zeta.txt", false, 5),
            node("/C/Docs/Alpha.txt", false, 7),
            node("/C/Docs/sub", true, 0),
            node("/C/Docs/Beta", true, 0),
        ]
        .join("\n")
    }

    #[test]
    fn ls_parse_and_children_sorted_dirs_first() {
        let nodes = parse_ls(&listing());
        assert_eq!(nodes.len(), 5);
        let kids = children_of(nodes, "/C/Docs/");
        let names: Vec<&str> = kids.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["Beta", "sub", "Alpha.txt", "zeta.txt"]);
        assert_eq!(kids[2].size, 7);
        assert_eq!(kids[2].mtime, "2026-01-01T10:00:00Z");
    }

    #[test]
    fn rewrite_args_exclude_one_path_and_forget_the_original() {
        let a = rewrite_exclude_args("abcd1234", "/C/Users/x/secret");
        assert_eq!(
            a,
            [
                "rewrite",
                "--exclude",
                "/C/Users/x/secret",
                "--forget",
                "abcd1234"
            ]
        );
        assert!(has_pattern_chars("/C/a*b") && has_pattern_chars("/C/a[1]"));
        assert!(!has_pattern_chars("/C/Users/x y"));
    }

    #[test]
    fn vault_verify_detects_missing_and_wrong_size() {
        let listed = parse_ls(&listing());
        let ok = vec![
            ("/C/Docs/zeta.txt".to_string(), 5),
            ("/c/docs/alpha.txt".to_string(), 7),
        ];
        assert!(vault_mismatches(&ok, &listed).is_empty());
        let bad = vec![
            ("/C/Docs/zeta.txt".to_string(), 6),
            ("/C/Docs/gone.txt".to_string(), 1),
            ("/C/Docs/Alpha.txt".to_string(), 7),
        ];
        assert_eq!(
            vault_mismatches(&bad, &listed),
            [r"C:\Docs\zeta.txt", r"C:\Docs\gone.txt"]
        );
        // a folder in the listing never counts as a file
        let dir_as_file = vec![("/C/Docs/sub".to_string(), 0)];
        assert_eq!(vault_mismatches(&dir_as_file, &listed).len(), 1);
    }

    #[test]
    fn local_files_walks_folders() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/x.txt"), b"123").unwrap();
        std::fs::write(d.path().join("a/y.txt"), b"12").unwrap();
        let mut f = local_files(&d.path().join("a")).unwrap();
        f.sort();
        assert_eq!(f.len(), 2);
        assert_eq!(f.iter().map(|x| x.1).sum::<u64>(), 5);
        assert!(f[0].0.ends_with("/a/b/x.txt"));
        assert_eq!(local_files(&d.path().join("a/y.txt")).unwrap().len(), 1);
    }

    #[test]
    fn snapshot_ids_are_validated() {
        assert!(valid_snapshot_id("latest"));
        assert!(valid_snapshot_id("abc123DEF"));
        for bad in ["", "-x", "a:b", "a b", "../x", "a/b"] {
            assert!(!valid_snapshot_id(bad), "{bad}");
        }
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    fn n(path: &str, dir: bool) -> LsNode {
        LsNode {
            name: split_restic(path).1,
            path: path.into(),
            dir,
            size: 1,
            mtime: String::new(),
        }
    }

    #[test]
    fn groups_by_parent_and_sorts() {
        let g = group_by_dir(vec![
            n("/C", true),
            n("/C/b.txt", false),
            n("/C/Z", true),
            n("/C/a.txt", false),
            n("/C/Z/x", false),
        ]);
        let c: Vec<_> = g["/C"].iter().map(|x| x.name.as_str()).collect();
        assert_eq!(c, ["Z", "a.txt", "b.txt"]);
        assert_eq!(g["/C/Z"].len(), 1);
        assert_eq!(g[""].len(), 1);
    }
}

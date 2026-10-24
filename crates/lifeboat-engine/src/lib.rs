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
            Error::Parse(m) => write!(f, "cannot parse restic output: {m}"),
            Error::UnsafeTarget(m) => write!(f, "unsafe restore target: {m}"),
        }
    }
}

impl std::error::Error for Error {}

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
            Err(Error::Restic {
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
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

    /// Verify repository integrity.
    pub fn check(&self) -> Result<(), Error> {
        self.run(["check"]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

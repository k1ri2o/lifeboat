//! Named-pipe server speaking the JSON-lines protocol.

use crate::security::{
    gate, pipe_client_is_admin, production_sddl, require_admin, Descriptor, RateLimiter, Refusal,
};
use lifeboat_core::config::Config;
use lifeboat_core::disk::PrepareMode;
use lifeboat_core::ipc::{Request, Response, SnapshotInfo, PIPE_NAME};
use lifeboat_engine::Engine;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::watch;

/// Config from the environment for now (`LIFEBOAT_REPO`, `LIFEBOAT_PASSWORD`,
/// `LIFEBOAT_FOLDERS` separated by `;`). The real partition comes in M3.
pub fn config_from_env() -> Config {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    Config {
        folders: var("LIFEBOAT_FOLDERS")
            .split(';')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        repo: PathBuf::from(var("LIFEBOAT_REPO")),
        password: var("LIFEBOAT_PASSWORD"),
        partition_guid: var("LIFEBOAT_PARTITION_GUID"),
    }
}

fn err(e: impl std::fmt::Display) -> Response {
    Response::Error {
        message: e.to_string(),
    }
}

fn engine(cfg: &Config) -> Result<Engine, Response> {
    if cfg.repo.as_os_str().is_empty() || cfg.password.is_empty() {
        return Err(err("backup repository is not configured"));
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

/// Compute the reply for one request line. `limiter` counts wrong passwords; `is_admin` tells
/// whether the pipe client is an elevated administrator (asked only when a request needs it).
pub fn handle_line(
    line: &str,
    cfg: &Config,
    limiter: &RateLimiter,
    is_admin: &dyn Fn() -> bool,
) -> Response {
    match serde_json::from_str::<Request>(line) {
        Ok(Request::Backup) => match engine(cfg) {
            Ok(e) => e
                .backup()
                .map(|snapshot| Response::BackupDone { snapshot })
                .unwrap_or_else(err),
            Err(r) => r,
        },
        Ok(Request::ListSnapshots) => match engine(cfg) {
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
            Err(r) => r,
        },
        Ok(Request::Restore {
            snapshot,
            target,
            path,
            password,
        }) => {
            if let Err(r) = password_gate(cfg, limiter, &password) {
                return refused(r);
            }
            match engine(cfg) {
                Ok(e) => e
                    .restore(&snapshot, std::path::Path::new(&target), path.as_deref())
                    .map(|_| Response::Restored)
                    .unwrap_or_else(err),
                Err(r) => r,
            }
        }
        Ok(Request::Ping) => Response::Pong,
        Ok(Request::Status) => Response::Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            frozen: false,
        },
        Ok(Request::ListDisks) => match lifeboat_disk::list_disks() {
            Ok(disks) => Response::Disks { disks },
            Err(e) => err(e),
        },
        Ok(Request::PrepareDisk {
            disk,
            mode,
            password,
        }) => {
            // Admin first (cheap, no password guess is spent), then the password.
            if let Err(r) = require_admin(is_admin()) {
                return refused(r);
            }
            if let Err(r) = password_gate(cfg, limiter, &password) {
                return refused(r);
            }
            match prepare(disk, mode) {
                Ok(v) => Response::Prepared {
                    volume_path: v.volume_path,
                    partition_guid: v.partition_guid,
                },
                Err(e) => err(e),
            }
        }
        Err(e) => Response::Error {
            message: format!("bad request: {e}"),
        },
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
    cfg: Arc<Config>,
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
                let cfg = cfg.clone();
                let limiter = limiter.clone();
                // Raw server handle for the per-request admin check. The pipe object lives in
                // this task (split below) until the connection ends, so it stays valid.
                let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&client) as usize;
                tokio::spawn(async move {
                    let (r, mut w) = tokio::io::split(client);
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let cfg = cfg.clone();
                        let limiter = limiter.clone();
                        let resp = tokio::task::spawn_blocking(move || {
                            handle_line(&line, &cfg, &limiter, &|| pipe_client_is_admin(raw))
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

/// Serve on the default pipe.
pub async fn serve_default(stop: watch::Receiver<bool>) -> io::Result<()> {
    serve(
        PIPE_NAME,
        &production_sddl(),
        Arc::new(config_from_env()),
        stop,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    fn call(line: &str, admin: bool) -> Response {
        handle_line(line, &cfg(), &RateLimiter::default(), &|| admin)
    }

    #[test]
    fn backup_unconfigured_is_error() {
        assert!(matches!(
            call(r#"{"cmd":"Backup"}"#, false),
            Response::Error { .. }
        ));
        assert!(matches!(
            call(r#"{"cmd":"ListSnapshots"}"#, false),
            Response::Error { .. }
        ));
    }

    #[test]
    fn ping_pong() {
        assert_eq!(call(r#"{"cmd":"Ping"}"#, false), Response::Pong);
    }

    #[test]
    fn status() {
        assert!(matches!(
            call(r#"{"cmd":"Status"}"#, false),
            Response::Status { frozen: false, .. }
        ));
    }

    #[test]
    fn garbage() {
        assert!(matches!(call("nope", false), Response::Error { .. }));
    }

    #[test]
    fn new_requests_parse() {
        assert!(!matches!(
            call(
                r#"{"cmd":"PrepareDisk","disk":1,"mode":{"mode":"Nope"}}"#,
                false
            ),
            Response::Disks { .. } | Response::Prepared { .. }
        ));
    }

    const PREPARE: &str =
        r#"{"cmd":"PrepareDisk","disk":1,"mode":{"mode":"External"},"password":"x"}"#;

    fn message(r: Response) -> String {
        match r {
            Response::Error { message } => message,
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn prepare_disk_refused_for_non_admin() {
        assert_eq!(message(call(PREPARE, false)), "needs administrator");
    }

    #[test]
    fn prepare_disk_admin_still_needs_repo_to_check_password() {
        // Admin passes the first gate; with no repo configured the password cannot be checked,
        // so nothing touches a disk.
        let m = message(call(PREPARE, true));
        assert!(m.starts_with("cannot check password"), "{m}");
    }

    #[test]
    fn restore_needs_password_field() {
        let r = call(
            r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","path":null}"#,
            true,
        );
        assert!(message(r).starts_with("bad request"));
        let m = message(call(
            r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","path":null,"password":""}"#,
            true,
        ));
        assert!(m.starts_with("cannot check password"), "{m}");
    }

    #[test]
    fn empty_password_counts_as_wrong_and_locks_out() {
        let c = Config {
            repo: PathBuf::from("C:/nonexistent-repo"),
            ..Config::default()
        };
        let l = RateLimiter::default();
        let line = r#"{"cmd":"Restore","snapshot":"s","target":"C:/x","path":null,"password":""}"#;
        for _ in 0..5 {
            let m = message(handle_line(line, &c, &l, &|| true));
            assert_eq!(m, "password wrong");
        }
        let m = message(handle_line(line, &c, &l, &|| true));
        assert_eq!(m, "locked for 15 minutes");
        // Open commands stay open while locked.
        assert_eq!(
            handle_line(r#"{"cmd":"Ping"}"#, &c, &l, &|| false),
            Response::Pong
        );
    }

    #[tokio::test]
    async fn pipe_roundtrip() {
        let pipe = r"\\.\pipe\lifeboat-test";
        let (tx, rx) = watch::channel(false);
        // Permissive SDDL (Everyone): CI runners may lack the INTERACTIVE SID.
        let h = tokio::spawn(async move {
            serve(pipe, "D:(A;;GA;;;WD)", Arc::new(Config::default()), rx).await
        });
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

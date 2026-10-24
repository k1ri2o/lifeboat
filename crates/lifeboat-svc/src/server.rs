//! Named-pipe server speaking the JSON-lines protocol.

use crate::security::{production_sddl, Descriptor};
use lifeboat_core::config::Config;
use lifeboat_core::disk::PrepareMode;
use lifeboat_core::ipc::{Request, Response, SnapshotInfo, PIPE_NAME};
use lifeboat_engine::Engine;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
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

/// Compute the reply for one request line.
pub fn handle_line(line: &str, cfg: &Config) -> Response {
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
        }) => match engine(cfg) {
            Ok(e) => e
                .restore(&snapshot, std::path::Path::new(&target), path.as_deref())
                .map(|_| Response::Restored)
                .unwrap_or_else(err),
            Err(r) => r,
        },
        Ok(Request::Ping) => Response::Pong,
        Ok(Request::Status) => Response::Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            frozen: false,
        },
        Ok(Request::ListDisks) => match lifeboat_disk::list_disks() {
            Ok(disks) => Response::Disks { disks },
            Err(e) => err(e),
        },
        Ok(Request::PrepareDisk { disk, mode }) => match prepare(disk, mode) {
            Ok(volume_path) => Response::Prepared { volume_path },
            Err(e) => err(e),
        },
        Err(e) => Response::Error {
            message: format!("bad request: {e}"),
        },
    }
}

/// Create the hidden partition (external or internal shrink), then lock it down to the service.
fn prepare(disk: u32, mode: PrepareMode) -> lifeboat_disk::Result<String> {
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
    lifeboat_disk::lock_down(&volume)?;
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
    let mut server = create_instance(pipe, &sd, true)?;
    loop {
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            conn = server.connect() => {
                conn?;
                let client = server;
                server = create_instance(pipe, &sd, false)?;
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let (r, mut w) = tokio::io::split(client);
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let cfg = cfg.clone();
                        let resp = tokio::task::spawn_blocking(move || handle_line(&line, &cfg))
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

    #[test]
    fn backup_unconfigured_is_error() {
        assert!(matches!(
            handle_line(r#"{"cmd":"Backup"}"#, &cfg()),
            Response::Error { .. }
        ));
        assert!(matches!(
            handle_line(r#"{"cmd":"ListSnapshots"}"#, &cfg()),
            Response::Error { .. }
        ));
    }

    #[test]
    fn ping_pong() {
        assert_eq!(handle_line(r#"{"cmd":"Ping"}"#, &cfg()), Response::Pong);
    }

    #[test]
    fn status() {
        assert!(matches!(
            handle_line(r#"{"cmd":"Status"}"#, &cfg()),
            Response::Status { frozen: false, .. }
        ));
    }

    #[test]
    fn garbage() {
        assert!(matches!(
            handle_line("nope", &cfg()),
            Response::Error { .. }
        ));
    }

    #[test]
    fn new_requests_parse() {
        assert!(!matches!(
            handle_line(
                r#"{"cmd":"PrepareDisk","disk":1,"mode":{"mode":"Nope"}}"#,
                &cfg()
            ),
            Response::Disks { .. } | Response::Prepared { .. }
        ));
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

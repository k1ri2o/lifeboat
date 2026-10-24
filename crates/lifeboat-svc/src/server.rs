//! Named-pipe server speaking the JSON-lines protocol.

use lifeboat_core::config::Config;
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
        Err(e) => Response::Error {
            message: format!("bad request: {e}"),
        },
    }
}

/// Serve on `pipe` until `stop` becomes true.
pub async fn serve(
    pipe: &str,
    cfg: Arc<Config>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(pipe)?;
    loop {
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            conn = server.connect() => {
                conn?;
                let client = server;
                server = ServerOptions::new().create(pipe)?;
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
    serve(PIPE_NAME, Arc::new(config_from_env()), stop).await
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

    #[tokio::test]
    async fn pipe_roundtrip() {
        let pipe = r"\\.\pipe\lifeboat-test";
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(async move { serve(pipe, Arc::new(Config::default()), rx).await });
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

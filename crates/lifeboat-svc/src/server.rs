//! Named-pipe server speaking the JSON-lines protocol.

use lifeboat_core::ipc::{Request, Response, PIPE_NAME};
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::watch;

/// Compute the reply for one request line.
pub fn handle_line(line: &str) -> Response {
    match serde_json::from_str::<Request>(line) {
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
pub async fn serve(pipe: &str, mut stop: watch::Receiver<bool>) -> io::Result<()> {
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
                tokio::spawn(async move {
                    let (r, mut w) = tokio::io::split(client);
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let mut out =
                            serde_json::to_string(&handle_line(&line)).unwrap_or_default();
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
    serve(PIPE_NAME, stop).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_pong() {
        assert_eq!(handle_line(r#"{"cmd":"Ping"}"#), Response::Pong);
    }

    #[test]
    fn status() {
        assert!(matches!(
            handle_line(r#"{"cmd":"Status"}"#),
            Response::Status { frozen: false, .. }
        ));
    }

    #[test]
    fn garbage() {
        assert!(matches!(handle_line("nope"), Response::Error { .. }));
    }

    #[tokio::test]
    async fn pipe_roundtrip() {
        let pipe = r"\\.\pipe\lifeboat-test";
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(async move { serve(pipe, rx).await });
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

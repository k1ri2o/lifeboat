//! IPC protocol between the UI and the service: one JSON object per line over
//! the named pipe [`PIPE_NAME`].

use serde::{Deserialize, Serialize};

/// Name of the named pipe the service listens on.
pub const PIPE_NAME: &str = r"\\.\pipe\lifeboat";

/// A command sent to the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Request {
    Ping,
    Status,
}

/// The service's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply")]
pub enum Response {
    Pong,
    Status {
        /// Service version.
        version: String,
        /// Whether backups are currently frozen (always false until M4).
        frozen: bool,
    },
    Error {
        message: String,
    },
}

/// Blocking client used by the UI.
pub mod client {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{self, BufRead, BufReader, Write};

    /// Send one request to the service on `pipe` and read one reply.
    pub fn request_on(pipe: &str, req: &Request) -> io::Result<Response> {
        let mut f = OpenOptions::new().read(true).write(true).open(pipe)?;
        let mut line = serde_json::to_string(req).map_err(io::Error::other)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.flush()?;
        let mut reader = BufReader::new(f);
        let mut out = String::new();
        reader.read_line(&mut out)?;
        serde_json::from_str(out.trim()).map_err(io::Error::other)
    }

    /// Send one request to the default pipe.
    pub fn request(req: &Request) -> io::Result<Response> {
        request_on(PIPE_NAME, req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        assert_eq!(
            serde_json::to_string(&Request::Ping).unwrap(),
            r#"{"cmd":"Ping"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::Pong).unwrap(),
            r#"{"reply":"Pong"}"#
        );
    }

    #[test]
    fn status_roundtrip() {
        let r = Response::Status {
            version: "0.1.0".into(),
            frozen: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
    }
}

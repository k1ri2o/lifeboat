//! Running PowerShell and parsing its output.

use crate::{Error, Result};
use std::process::Command;

/// Run `script` in a fresh non-interactive PowerShell and return stdout.
pub fn run(script: &str) -> Result<String> {
    let full = format!("$ErrorActionPreference='Stop'; {script}");
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &full])
        .output()?;
    if !out.status.success() {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        return Err(Error::Command(msg));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// PowerShell single-quoted string literal.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting() {
        assert_eq!(super::quote("a'b"), "'a''b'");
    }
}

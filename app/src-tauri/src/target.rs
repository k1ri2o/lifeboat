//! Checks on the folder a restore goes into.

use std::path::Path;

/// A restore must go into a folder that does not exist yet or is empty, so it can never
/// overwrite the user's own files.
pub fn check_restore_target(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("Please choose a folder to restore into.".into());
    }
    let p = Path::new(path.trim());
    if !p.exists() {
        return Ok(());
    }
    if !p.is_dir() {
        return Err("That is a file, not a folder. Please choose a new, empty folder.".into());
    }
    match std::fs::read_dir(p) {
        Ok(mut it) => {
            if it.next().is_some() {
                Err(
                    "That folder already has files in it. Please choose a new, empty folder."
                        .into(),
                )
            } else {
                Ok(())
            }
        }
        Err(_) => Err("Lifeboat can't open that folder. Please choose another one.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lifeboat-target-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn missing_folder_is_fine() {
        assert!(check_restore_target(tmp("missing").to_str().unwrap()).is_ok());
    }

    #[test]
    fn empty_folder_is_fine_and_full_is_not() {
        let d = tmp("dirs");
        std::fs::create_dir_all(&d).unwrap();
        assert!(check_restore_target(d.to_str().unwrap()).is_ok());
        std::fs::write(d.join("a.txt"), "x").unwrap();
        assert!(check_restore_target(d.to_str().unwrap()).is_err());
        assert!(check_restore_target(d.join("a.txt").to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn blank_is_rejected() {
        assert!(check_restore_target("  ").is_err());
    }
}

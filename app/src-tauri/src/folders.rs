//! One-time move of the old UI-only `folders.json` into the service's folder list.
//!
//! Older versions of the app kept their own list that the service never read. On startup, if
//! that file exists and the service has no folders yet, the list is sent to the service once
//! and the file is deleted.

use std::fs;
use std::path::Path;

/// What happened to the old file.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// No old file: nothing to do.
    NoFile,
    /// The service could not be asked or told; try again later.
    Retry,
    /// The old file was sent (or not needed) and deleted.
    Done,
}

/// `current` asks the service for its folders; `send` replaces them.
pub fn migrate(
    file: &Path,
    current: impl FnOnce() -> Option<Vec<String>>,
    send: impl FnOnce(Vec<String>) -> bool,
) -> Migration {
    if !file.exists() {
        return Migration::NoFile;
    }
    let Some(service) = current() else {
        return Migration::Retry;
    };
    let old: Vec<String> = fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if service.is_empty() && !old.is_empty() && !send(old) {
        return Migration::Retry;
    }
    let _ = fs::remove_file(file);
    Migration::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp(name: &str, content: Option<&str>) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lifeboat-mig-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let f = d.join("folders.json");
        if let Some(c) = content {
            fs::write(&f, c).unwrap();
        }
        f
    }

    #[test]
    fn sends_once_and_deletes() {
        let f = temp("send", Some(r#"["C:\\Docs"]"#));
        let sent = RefCell::new(vec![]);
        let r = migrate(
            &f,
            || Some(vec![]),
            |l| {
                *sent.borrow_mut() = l;
                true
            },
        );
        assert_eq!(r, Migration::Done);
        assert_eq!(*sent.borrow(), vec![r"C:\Docs".to_string()]);
        assert!(!f.exists());
        assert_eq!(migrate(&f, || panic!(), |_| panic!()), Migration::NoFile);
    }

    #[test]
    fn keeps_file_when_service_down_or_send_fails() {
        let f = temp("retry", Some(r#"["C:\\Docs"]"#));
        assert_eq!(migrate(&f, || None, |_| true), Migration::Retry);
        assert!(f.exists());
        assert_eq!(migrate(&f, || Some(vec![]), |_| false), Migration::Retry);
        assert!(f.exists());
    }

    #[test]
    fn does_not_overwrite_a_service_list() {
        let f = temp("nonempty", Some(r#"["C:\\Old"]"#));
        let r = migrate(
            &f,
            || Some(vec!["D:/New".into()]),
            |_| panic!("must not send"),
        );
        assert_eq!(r, Migration::Done);
        assert!(!f.exists());
    }
}

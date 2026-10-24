//! The list of folders the user wants backed up, kept by the UI as a small JSON file.
//!
//! NOTE: the service has no command yet to receive this list (its folders still come from its
//! own config), so for now this is the list the UI shows and keeps.

use std::fs;
use std::path::Path;

pub fn load(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(file: &Path, list: &[String]) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let s = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    fs::write(file, s).map_err(|e| e.to_string())
}

pub fn add(file: &Path, folder: &str) -> Result<Vec<String>, String> {
    let mut list = load(file);
    let folder = folder.trim().to_string();
    if !folder.is_empty() && !list.iter().any(|f| f.eq_ignore_ascii_case(&folder)) {
        list.push(folder);
        save(file, &list)?;
    }
    Ok(list)
}

pub fn remove(file: &Path, folder: &str) -> Result<Vec<String>, String> {
    let mut list = load(file);
    list.retain(|f| !f.eq_ignore_ascii_case(folder));
    save(file, &list)?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_roundtrip() {
        let d = std::env::temp_dir().join(format!("lifeboat-folders-{}", std::process::id()));
        let f = d.join("folders.json");
        let _ = fs::remove_dir_all(&d);
        assert!(load(&f).is_empty());
        add(&f, r"C:\Users\Dad\Documents").unwrap();
        add(&f, r"c:\users\dad\documents").unwrap();
        let l = add(&f, r"C:\Users\Dad\Pictures").unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(load(&f), l);
        let l = remove(&f, r"C:\Users\Dad\Documents").unwrap();
        assert_eq!(l, vec![r"C:\Users\Dad\Pictures".to_string()]);
        fs::remove_dir_all(&d).unwrap();
    }
}

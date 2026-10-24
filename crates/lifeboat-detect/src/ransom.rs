//! Known ransomware extensions and ransom-note file names.

use std::path::Path;

/// Extensions appended or substituted by common ransomware families (lowercase, no dot).
pub const RANSOM_EXTENSIONS: &[&str] = &[
    "locked",
    "lock",
    "crypt",
    "crypted",
    "crypto",
    "encrypted",
    "enc",
    "wncry",
    "wcry",
    "wnry",
    "lockbit",
    "ryk",
    "conti",
    "locky",
    "cerber",
    "zepto",
    "odin",
    "thor",
    "aesir",
    "zzzzz",
    "cry",
    "crab",
    "dharma",
    "phobos",
    "akira",
    "blackcat",
    "royal",
    "hive",
    "basta",
    "ryuk",
    "petya",
    "djvu",
    "medusa",
    "rhysida",
];

/// True when the path ends in a known ransomware extension.
pub fn has_ransom_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| RANSOM_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Prefixes of ransom-note names (compared case-insensitively against the file name).
const NOTE_PREFIXES: &[&str] = &[
    "readme_decrypt",
    "how_to_recover",
    "how_to_decrypt",
    "how_to_restore",
    "!!!read",
    "!please_read",
    "decrypt_instruction",
    "restore_files",
    "recover_files",
    "your_files_are_encrypted",
    "read_to_decrypt",
    "@please_read_me@",
];

/// Substrings of ransom-note names (e.g. `LOCKBIT_RESTORE_FILES.txt`).
const NOTE_CONTAINS: &[&str] = &[
    "_restore_files",
    "_recover_files",
    "decrypt_my_files",
    "_readme_decrypt",
];

/// True when the file name looks like a ransom note.
pub fn is_ransom_note(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let n = name.to_ascii_lowercase();
    NOTE_PREFIXES.iter().any(|p| n.starts_with(p)) || NOTE_CONTAINS.iter().any(|c| n.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert!(has_ransom_extension(Path::new("a.docx.LOCKED")));
        assert!(has_ransom_extension(Path::new("x.wncry")));
        assert!(!has_ransom_extension(Path::new("a.docx")));
        assert!(!has_ransom_extension(Path::new("locked")));
    }

    #[test]
    fn notes() {
        for n in [
            "README_DECRYPT.txt",
            "HOW_TO_RECOVER_FILES.html",
            "!!!READ_ME!!!.txt",
            "LockBit_RESTORE_FILES.txt",
        ] {
            assert!(is_ransom_note(Path::new(n)), "{n}");
        }
        assert!(!is_ransom_note(Path::new("readme.md")));
        assert!(!is_ransom_note(Path::new("notes.txt")));
    }
}

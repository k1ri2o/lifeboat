//! Expected file headers ("magic numbers") for common types.

use std::path::Path;

struct Sig {
    offset: usize,
    bytes: &'static [u8],
}

const fn sig(offset: usize, bytes: &'static [u8]) -> Sig {
    Sig { offset, bytes }
}

const ZIP: &[Sig] = &[sig(0, b"PK\x03\x04"), sig(0, b"PK\x05\x06")];
const OLE: &[Sig] = &[sig(0, &[0xD0, 0xCF, 0x11, 0xE0])];
const PDF: &[Sig] = &[sig(0, b"%PDF")];
const JPG: &[Sig] = &[sig(0, &[0xFF, 0xD8, 0xFF])];
const PNG: &[Sig] = &[sig(0, &[0x89, b'P', b'N', b'G'])];
const GIF: &[Sig] = &[sig(0, b"GIF8")];
const MP3: &[Sig] = &[
    sig(0, b"ID3"),
    sig(0, &[0xFF, 0xFB]),
    sig(0, &[0xFF, 0xF3]),
    sig(0, &[0xFF, 0xF2]),
];
const MP4: &[Sig] = &[sig(4, b"ftyp")];
const SEVENZ: &[Sig] = &[sig(0, &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C])];
const RAR: &[Sig] = &[sig(0, b"Rar!\x1a\x07")];

fn expected(ext: &str) -> Option<&'static [Sig]> {
    Some(match ext {
        "docx" | "xlsx" | "pptx" | "zip" | "odt" | "ods" | "odp" | "jar" => ZIP,
        "doc" | "xls" | "ppt" => OLE,
        "pdf" => PDF,
        "jpg" | "jpeg" => JPG,
        "png" => PNG,
        "gif" => GIF,
        "mp3" => MP3,
        "mp4" | "m4a" | "m4v" | "mov" => MP4,
        "7z" => SEVENZ,
        "rar" => RAR,
        _ => return None,
    })
}

/// True when the file's extension has a known header and `bytes` does not start with it.
/// Unknown extensions and empty content never count as a mismatch.
pub fn mismatches(path: &Path, bytes: &[u8]) -> bool {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    let Some(sigs) = expected(&ext) else {
        return false;
    };
    if bytes.is_empty() {
        return false;
    }
    !sigs.iter().any(|s| {
        bytes.len() >= s.offset + s.bytes.len()
            && &bytes[s.offset..s.offset + s.bytes.len()] == s.bytes
    })
}

/// True for types that are compressed and so naturally high entropy.
pub fn is_naturally_high_entropy(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "docx"
            | "xlsx"
            | "pptx"
            | "zip"
            | "odt"
            | "ods"
            | "odp"
            | "jar"
            | "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "mp3"
            | "mp4"
            | "m4a"
            | "m4v"
            | "mov"
            | "7z"
            | "rar"
            | "gz"
            | "xz"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_and_mismatches() {
        assert!(!mismatches(Path::new("a.docx"), b"PK\x03\x04rest"));
        assert!(mismatches(Path::new("a.DOCX"), b"\x91\x02random"));
        assert!(!mismatches(Path::new("a.pdf"), b"%PDF-1.7"));
        assert!(mismatches(Path::new("a.jpg"), b"%PDF"));
        assert!(!mismatches(Path::new("a.mp4"), b"\0\0\0\x18ftypisom"));
        assert!(!mismatches(Path::new("a.txt"), b"anything"));
        assert!(!mismatches(Path::new("noext"), b"x"));
        assert!(!mismatches(Path::new("a.png"), b""));
    }
}

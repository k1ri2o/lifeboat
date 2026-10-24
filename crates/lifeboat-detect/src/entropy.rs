//! Shannon entropy over a sample of a file.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Size of each sampled block (first, middle, last).
pub const SAMPLE_BLOCK: usize = 64 * 1024;

/// Shannon entropy in bits per byte (0.0 to 8.0) of the sample of `bytes`.
/// Inputs up to three blocks are used whole; larger ones use first + middle + last block.
pub fn entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0u64; 256];
    let mut total = 0u64;
    let mut add = |s: &[u8]| {
        for &b in s {
            counts[b as usize] += 1;
        }
        total += s.len() as u64;
    };
    if bytes.len() <= SAMPLE_BLOCK * 3 {
        add(bytes);
    } else {
        let mid = bytes.len() / 2 - SAMPLE_BLOCK / 2;
        add(&bytes[..SAMPLE_BLOCK]);
        add(&bytes[mid..mid + SAMPLE_BLOCK]);
        add(&bytes[bytes.len() - SAMPLE_BLOCK..]);
    }
    if total == 0 {
        return 0.0;
    }
    let t = total as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / t;
            -p * p.log2()
        })
        .sum()
}

/// Read the sample blocks of a file without loading it all and return their entropy.
pub fn entropy_of_file(path: &Path) -> io::Result<f64> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len() as usize;
    let mut buf = Vec::new();
    if len <= SAMPLE_BLOCK * 3 {
        f.read_to_end(&mut buf)?;
        return Ok(entropy(&buf));
    }
    let mid = len / 2 - SAMPLE_BLOCK / 2;
    for off in [0, mid, len - SAMPLE_BLOCK] {
        f.seek(SeekFrom::Start(off as u64))?;
        let mut block = vec![0u8; SAMPLE_BLOCK];
        f.read_exact(&mut block)?;
        buf.extend_from_slice(&block);
    }
    Ok(entropy(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_is_zero_and_uniform_is_eight() {
        assert_eq!(entropy(&[7u8; 1000]), 0.0);
        let all: Vec<u8> = (0..=255u8).cycle().take(256 * 8).collect();
        assert!((entropy(&all) - 8.0).abs() < 1e-9);
        assert_eq!(entropy(&[]), 0.0);
    }

    #[test]
    fn large_input_is_sampled() {
        let big = vec![0u8; SAMPLE_BLOCK * 10];
        assert_eq!(entropy(&big), 0.0);
    }

    #[test]
    fn file_matches_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        let data: Vec<u8> = (0..SAMPLE_BLOCK * 4).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        assert!((entropy_of_file(&p).unwrap() - entropy(&data)).abs() < 1e-9);
    }
}

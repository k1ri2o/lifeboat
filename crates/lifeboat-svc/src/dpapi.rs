//! Windows DPAPI, machine scope: only this PC can decrypt, any process on it can ask. The
//! on-disk config is additionally locked to SYSTEM and the service by ACL.

use std::io;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPT_INTEGER_BLOB,
};

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: DPAPI allocated `cbData` bytes at `pbData`; we copy then free them.
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        v
    }
}

pub fn protect(plain: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(plain);
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: valid in/out blobs; optional parameters are null.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_LOCAL_MACHINE,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(take(out))
}

pub fn unprotect(cipher: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(cipher);
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: as above.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(take(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_not_plaintext() {
        let c = protect(b"hunter2-secret").unwrap();
        assert!(!c.windows(6).any(|w| w == b"hunter"));
        assert_eq!(unprotect(&c).unwrap(), b"hunter2-secret");
        assert!(unprotect(b"garbage").is_err());
    }
}

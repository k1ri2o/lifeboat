//! Pipe security: who may open `\.\pipe\lifeboat`, and the service SID.
//!
//! The service runs as LocalSystem (partition, offline and ACL operations need it; a virtual
//! account `NT SERVICE\lifeboat-svc` cannot manage disks). To still get a per-service identity
//! for the volume ACL and the pipe, install turns on an *unrestricted service SID*
//! (`sc sidtype lifeboat-svc unrestricted`). The SID is derived from the service name, so it can
//! be computed here without the service existing.

use sha1::{Digest, Sha1};
use std::ffi::c_void;

/// Name of the service (also the name in `NT SERVICE\<name>`).
pub const SERVICE_NAME: &str = "lifeboat-svc";

/// Service SID: `S-1-5-80-` plus five little-endian u32 words of SHA-1 over the upper-cased
/// service name encoded as UTF-16LE (same as `sc.exe showsid`).
pub fn service_sid(name: &str) -> String {
    let bytes: Vec<u8> = name
        .to_uppercase()
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let hash = Sha1::digest(&bytes);
    let mut s = String::from("S-1-5-80");
    for i in 0..5 {
        let w: [u8; 4] = hash[i * 4..i * 4 + 4].try_into().expect("4 bytes");
        s.push('-');
        s.push_str(&u32::from_le_bytes(w).to_string());
    }
    s
}

/// SDDL for the production pipe: SYSTEM and the service SID full access, interactive users
/// read/write, network logons denied (the deny ACE comes first). Protected DACL.
pub fn pipe_sddl(service_sid: &str) -> String {
    format!("D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;{service_sid})(A;;GRGW;;;IU)")
}

/// SDDL of the production pipe.
pub fn production_sddl() -> String {
    pipe_sddl(&service_sid(SERVICE_NAME))
}

/// A security descriptor built from SDDL; freed on drop.
pub struct Descriptor(*mut c_void);

// The descriptor is immutable after creation.
unsafe impl Send for Descriptor {}
unsafe impl Sync for Descriptor {}

impl Descriptor {
    pub fn from_sddl(sddl: &str) -> std::io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd: *mut c_void = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(sd))
    }

    /// Raw `SECURITY_ATTRIBUTES` pointing at this descriptor. `sa` must outlive the call using it.
    pub fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
                as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_matches_sc_showsid() {
        assert_eq!(
            service_sid("Spooler"),
            "S-1-5-80-3951239711-1671533544-1416304335-3763227691-3930497994"
        );
        assert_eq!(
            service_sid("lifeboat-svc"),
            "S-1-5-80-3387603332-739407714-2551992211-1991162400-2060127048"
        );
    }

    #[test]
    fn sddl_text() {
        let s = production_sddl();
        assert!(s.starts_with("D:P(D;;GA;;;NU)"), "{s}");
        assert!(s.contains("(A;;GA;;;SY)"));
        assert!(s.contains(";;;S-1-5-80-3387603332-"));
        assert!(s.ends_with("(A;;GRGW;;;IU)"));
        // Deny ACE is first so it wins for network logons.
        assert!(s.find("D;;GA;;;NU").unwrap() < s.find("A;;GA;;;SY").unwrap());
    }

    #[test]
    fn sddl_parses() {
        Descriptor::from_sddl(&production_sddl()).expect("production SDDL parses");
        assert!(Descriptor::from_sddl("not sddl").is_err());
    }
}

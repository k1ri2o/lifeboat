//! Pipe security: who may open `\.\pipe\lifeboat`, and the service SID.
//!
//! The service runs as LocalSystem (partition, offline and ACL operations need it; a virtual
//! account `NT SERVICE\lifeboat-svc` cannot manage disks). To still get a per-service identity
//! for the volume ACL and the pipe, install turns on an *unrestricted service SID*
//! (`sc sidtype lifeboat-svc unrestricted`). The SID is derived from the service name, so it can
//! be computed here without the service existing.

use sha1::{Digest, Sha1};
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// Wrong passwords allowed before the lockout starts.
pub const MAX_FAILURES: u32 = 5;
/// How long the lockout lasts.
pub const LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Why a dangerous request was refused. `Display` is the text sent to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    WrongPassword,
    /// Locked out for this many more minutes (rounded up, at least 1).
    Locked(u64),
    NeedsAdmin,
    /// The password could not be checked (repo missing, restic failed, ...).
    Unavailable(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongPassword => write!(f, "password wrong"),
            Self::Locked(m) => write!(f, "locked for {m} minutes"),
            Self::NeedsAdmin => write!(f, "needs administrator"),
            Self::Unavailable(m) => write!(f, "cannot check password: {m}"),
        }
    }
}

/// Counts wrong passwords: 5 failures lock the gate for 15 minutes. In memory, per service.
/// Every method takes `now` so tests need no clock.
#[derive(Debug, Default)]
pub struct RateLimiter {
    state: Mutex<(u32, Option<Instant>)>, // (failures, locked until)
}

impl RateLimiter {
    /// `Err(minutes)` while locked out.
    pub fn check(&self, now: Instant) -> Result<(), u64> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match s.1 {
            Some(until) if now < until => Err((until - now).as_secs().div_ceil(60).max(1)),
            Some(_) => {
                *s = (0, None); // lockout over: start fresh
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub fn record_failure(&self, now: Instant) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.0 += 1;
        if s.0 >= MAX_FAILURES {
            s.1 = Some(now + LOCKOUT);
        }
    }

    pub fn record_success(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = (0, None);
    }
}

/// Password gate: refuse while locked out, otherwise run `verify` (true = password right) and
/// count a wrong one. A verify error is not counted as a wrong password.
pub fn gate(
    limiter: &RateLimiter,
    now: Instant,
    verify: impl FnOnce() -> Result<bool, String>,
) -> Result<(), Refusal> {
    limiter.check(now).map_err(Refusal::Locked)?;
    match verify() {
        Ok(true) => {
            limiter.record_success();
            Ok(())
        }
        Ok(false) => {
            limiter.record_failure(now);
            Err(Refusal::WrongPassword)
        }
        Err(e) => Err(Refusal::Unavailable(e)),
    }
}

/// Decision for requests that need an elevated administrator (PrepareDisk).
pub fn require_admin(is_admin: bool) -> Result<(), Refusal> {
    if is_admin {
        Ok(())
    } else {
        Err(Refusal::NeedsAdmin)
    }
}

/// Is the client of this server-side pipe handle an elevated administrator?
///
/// Must run on one thread: impersonate the client, read its thread token, revert. Needs the
/// server to have read a message already (a Windows rule). Any failure counts as "no".
pub fn pipe_client_is_admin(pipe_handle: usize) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, RevertToSelf,
        TokenElevation, WinBuiltinAdministratorsSid, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    // SAFETY: plain Win32 calls. `pipe_handle` is a live server pipe handle owned by the caller
    // for the whole call; the token handle is closed and the impersonation reverted before
    // returning, on every path after it started.
    unsafe {
        if ImpersonateNamedPipeClient(pipe_handle as HANDLE) == 0 {
            return false;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        let mut admin = false;
        // `true` = open with the impersonated thread's identity.
        if OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) != 0 {
            let mut sid = [0u8; 68]; // SECURITY_MAX_SID_SIZE
            let mut len = sid.len() as u32;
            let mut member = 0i32;
            let in_group = CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                std::ptr::null_mut(),
                sid.as_mut_ptr() as *mut c_void,
                &mut len,
            ) != 0
                && CheckTokenMembership(token, sid.as_mut_ptr() as *mut c_void, &mut member) != 0
                && member != 0;
            let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut ret = 0u32;
            let elevated = GetTokenInformation(
                token,
                TokenElevation,
                &mut el as *mut _ as *mut c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret,
            ) != 0
                && el.TokenIsElevated != 0;
            // Group enabled in the token AND elevated (UAC-filtered admins fail the second).
            admin = in_group && elevated;
            CloseHandle(token);
        }
        RevertToSelf();
        admin
    }
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

    #[test]
    fn wrong_password_refused_and_right_one_resets() {
        let l = RateLimiter::default();
        let t = Instant::now();
        assert_eq!(gate(&l, t, || Ok(false)), Err(Refusal::WrongPassword));
        assert_eq!(gate(&l, t, || Ok(true)), Ok(()));
        // success cleared the count: 4 more wrong ones do not lock
        for _ in 0..4 {
            assert_eq!(gate(&l, t, || Ok(false)), Err(Refusal::WrongPassword));
        }
        assert_eq!(gate(&l, t, || Ok(true)), Ok(()));
    }

    #[test]
    fn lockout_after_five_tries() {
        let l = RateLimiter::default();
        let t = Instant::now();
        for _ in 0..5 {
            assert_eq!(gate(&l, t, || Ok(false)), Err(Refusal::WrongPassword));
        }
        // Locked: even the right password is refused, and verify is not run.
        let r = gate(&l, t, || panic!("must not verify while locked"));
        assert_eq!(r, Err(Refusal::Locked(15)));
        assert_eq!(r.unwrap_err().to_string(), "locked for 15 minutes");
        assert_eq!(
            gate(&l, t + Duration::from_secs(14 * 60 + 1), || Ok(true)),
            Err(Refusal::Locked(1))
        );
        // After 15 minutes it opens again with a fresh count.
        assert_eq!(gate(&l, t + LOCKOUT, || Ok(true)), Ok(()));
    }

    #[test]
    fn verify_error_is_not_a_failure() {
        let l = RateLimiter::default();
        let t = Instant::now();
        for _ in 0..10 {
            assert!(matches!(
                gate(&l, t, || Err("no repo".into())),
                Err(Refusal::Unavailable(_))
            ));
        }
        assert_eq!(gate(&l, t, || Ok(true)), Ok(()));
    }

    #[test]
    fn non_admin_refused() {
        assert_eq!(require_admin(true), Ok(()));
        let r = require_admin(false).unwrap_err();
        assert_eq!(r.to_string(), "needs administrator");
        assert_eq!(Refusal::WrongPassword.to_string(), "password wrong");
    }

    #[test]
    fn invalid_pipe_handle_is_not_admin() {
        // Impersonation fails on a handle that is not a pipe: must answer "no", not crash.
        assert!(!pipe_client_is_admin(0));
    }
}

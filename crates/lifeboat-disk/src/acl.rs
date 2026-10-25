//! Service-only ACL on the hidden volume's root.
//!
//! After [`lock_down`] the root of the volume grants full control to SYSTEM and the Lifeboat
//! service SID only, with inheritance from the parent cut and the owner set to SYSTEM. Users,
//! Everyone, Authenticated Users and Administrators are removed.
//!
//! What an administrator can still do (an ACL is not a security boundary against admins):
//! take ownership (SeTakeOwnershipPrivilege) and rewrite the ACL, bring the disk online and
//! reformat or delete the partition, or run as SYSTEM. This raises the bar for malware running
//! as the user or as an unelevated process; destroying snapshots still needs the password and
//! a delay, which are enforced by the service (later milestones).

use crate::ps::run;
use crate::{Error, Result};
use std::process::Command;

/// Well-known SIDs.
const SYSTEM: &str = "*S-1-5-18";
const USERS: &str = "*S-1-5-32-545";
const EVERYONE: &str = "*S-1-1-0";
const AUTH_USERS: &str = "*S-1-5-11";
const ADMINS: &str = "*S-1-5-32-544";

/// Name of the service account that keeps access (`NT SERVICE\<service>`).
pub const SERVICE_ACCOUNT: &str = r"NT SERVICE\lifeboat-svc";

/// The icacls argument lists, in the order they must run, for `root` (the volume root path).
/// Pure function so the exact ACL is unit-tested.
pub fn lock_down_args(root: &str, service_account: &str) -> Vec<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    vec![
        // Owner first, while Administrators still have full control (once they are removed,
        // setting the owner is refused). Then grant, so we never lock ourselves out mid-way,
        // then cut inheritance.
        vec![root.to_string(), "/setowner".into(), SYSTEM.into()],
        vec![
            root.to_string(),
            "/grant:r".into(),
            format!("{SYSTEM}:(OI)(CI)F"),
            format!("{service_account}:(OI)(CI)F"),
        ],
        {
            let mut v = vec![root.to_string()];
            v.extend(s(&["/inheritance:r"]));
            v
        },
        {
            let mut v = vec![root.to_string(), "/remove:g".into()];
            v.extend(s(&[USERS, EVERYONE, AUTH_USERS, ADMINS]).into_iter());
            v
        },
    ]
}

/// Apply the service-only ACL to the root of `volume_path` (`\\?\Volume{...}\`).
/// Must run as SYSTEM or an administrator holding the right privileges.
pub fn lock_down(volume_path: &str) -> Result<()> {
    if !volume_path.to_ascii_lowercase().starts_with(r"\\?\volume{") {
        return Err(Error::Parse(format!(
            "not a volume GUID path: {volume_path}"
        )));
    }
    for args in lock_down_args(volume_path, SERVICE_ACCOUNT) {
        // A volume Windows has just mounted (after unhiding) can refuse for a moment: retry.
        let mut tries = 0;
        loop {
            let out = Command::new("icacls.exe").args(&args).output()?;
            if out.status.success() {
                break;
            }
            tries += 1;
            if tries >= 5 {
                return Err(Error::Command(format!(
                    "icacls {}: {} {}",
                    args[1],
                    String::from_utf8_lossy(&out.stdout).trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    Ok(())
}

/// Show the current ACL (read-only), for diagnostics and the integration test.
pub fn read_acl(volume_path: &str) -> Result<String> {
    run(&format!("icacls {}", crate::ps::quote(volume_path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acl_plan() {
        let a = lock_down_args(r"\\?\Volume{x}\", SERVICE_ACCOUNT);
        assert_eq!(a.len(), 4);
        assert_eq!(a[0][2], "*S-1-5-18");
        assert_eq!(a[0][1], "/setowner");
        assert_eq!(
            a[1],
            vec![
                r"\\?\Volume{x}\",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                r"NT SERVICE\lifeboat-svc:(OI)(CI)F"
            ]
        );
        assert_eq!(a[2][1], "/inheritance:r");
        for sid in ["*S-1-5-32-545", "*S-1-1-0", "*S-1-5-11", "*S-1-5-32-544"] {
            assert!(a[3].contains(&sid.to_string()));
        }
        // Only SYSTEM and the service are ever granted anything.
        let grants: Vec<_> = a.iter().flatten().filter(|x| x.contains(":(")).collect();
        assert_eq!(grants.len(), 2);
    }

    #[test]
    fn rejects_non_volume_paths() {
        assert!(lock_down(r"C:\").is_err());
        assert!(lock_down(r"E:\backup").is_err());
    }
}

//! Integration test on a throwaway VHDX virtual disk. Never touches a real disk: it creates its
//! own VHDX, refuses to continue unless the new disk is "File Backed Virtual" and not
//! boot/system, and detaches and deletes the file at the end (also on failure).
//!
//! Needs an elevated shell. Run (admin PowerShell):
//!   $env:LIFEBOAT_VHDX_TESTS=1; cargo test -p lifeboat-disk --test vhdx -- --ignored --nocapture

use lifeboat_disk::*;
use std::path::{Path, PathBuf};
use std::process::Command;

fn diskpart(script: &str, dir: &Path) {
    let f = dir.join("dp.txt");
    std::fs::write(&f, script).unwrap();
    let out = Command::new("diskpart.exe")
        .arg("/s")
        .arg(&f)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

struct Vhdx {
    dir: PathBuf,
    file: PathBuf,
}

impl Drop for Vhdx {
    fn drop(&mut self) {
        let f = self.file.display();
        let s = format!("select vdisk file=\"{f}\"\ndetach vdisk\n");
        let _ = std::fs::write(self.dir.join("dp2.txt"), s);
        let _ = Command::new("diskpart.exe")
            .arg("/s")
            .arg(self.dir.join("dp2.txt"))
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "needs admin; set LIFEBOAT_VHDX_TESTS=1 and pass --ignored"]
fn full_cycle_on_vhdx() {
    if std::env::var("LIFEBOAT_VHDX_TESTS").as_deref() != Ok("1") {
        eprintln!("LIFEBOAT_VHDX_TESTS not set; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lifeboat-vhdx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.vhdx");
    let guard = Vhdx {
        dir: dir.clone(),
        file: file.clone(),
    };
    let before: Vec<u32> = list_disks().unwrap().iter().map(|d| d.number).collect();
    diskpart(
        &format!(
            "create vdisk file=\"{}\" maximum=512 type=expandable\nattach vdisk\n",
            file.display()
        ),
        &dir,
    );
    let disks = list_disks().unwrap();
    let new: Vec<_> = disks
        .iter()
        .filter(|d| !before.contains(&d.number))
        .collect();
    assert_eq!(new.len(), 1, "expected exactly one new disk");
    let d = new[0];
    assert!(
        !d.is_boot && !d.is_system,
        "new disk must not be boot/system"
    );
    assert!(
        d.bus_type.to_lowercase().contains("virtual"),
        "bus {}",
        d.bus_type
    );
    let n = d.number;

    let vol = prepare_external(n).expect("prepare");
    assert!(vol.to_lowercase().starts_with(r"\\?\volume{"), "{vol}");
    assert_eq!(
        find_lifeboat_volume().unwrap().as_deref(),
        Some(vol.as_str())
    );
    let after = list_disks().unwrap();
    let part = after.iter().find(|x| x.number == n).unwrap();
    assert!(part.has_lifeboat_partition());
    assert_eq!(
        part.partitions
            .iter()
            .find(|p| p.is_lifeboat())
            .unwrap()
            .drive_letter,
        None
    );
    // Second prepare is refused by the guard.
    assert!(matches!(prepare_external(n), Err(Error::Guard(_))));

    lock_down(&vol).expect("lock_down");
    let acl = acl::read_acl(&vol).unwrap();
    assert!(acl.contains("SYSTEM"), "{acl}");
    assert!(!acl.contains(r"BUILTIN\Users"), "{acl}");

    set_disk_offline(n).expect("offline");
    set_disk_online(n).expect("online");
    drop(guard);
}

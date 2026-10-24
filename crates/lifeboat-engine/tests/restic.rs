use lifeboat_engine::Engine;
use std::path::{Path, PathBuf};

fn restic() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/restic/restic.exe");
    p.exists().then_some(p)
}

/// Find the folder below `root` that directly contains `a.txt`.
fn find_dir_with_a(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for en in std::fs::read_dir(&d).ok()?.flatten() {
            let p = en.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "a.txt") {
                return p.parent().map(|x| x.to_path_buf());
            }
        }
    }
    None
}

#[test]
fn backup_list_restore_roundtrip() {
    let Some(restic) = restic() else {
        eprintln!("SKIPPED: vendor/restic/restic.exe missing; run scripts/fetch-restic.ps1");
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src");
    std::fs::create_dir_all(src.join("nested")).unwrap();
    std::fs::write(src.join("a.txt"), b"hello lifeboat").unwrap();
    let bin: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    std::fs::write(src.join("nested").join("b.bin"), &bin).unwrap();

    let e = Engine::new(restic, t.path().join("repo"), "pw-test", vec![src.clone()]);
    e.init().unwrap();
    let id = e.backup().unwrap();
    let snaps = e.snapshots().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].id, id);
    e.check().unwrap();

    // Restoring into the source is refused.
    assert!(e.restore(&id, &src.join("out"), None).is_err());

    let out = t.path().join("restored");
    e.restore(&id, &out, None).unwrap();
    // restic recreates the original path below the target.
    let base = find_dir_with_a(&out).expect("a.txt not restored");
    assert_eq!(
        std::fs::read(base.join("a.txt")).unwrap(),
        b"hello lifeboat"
    );
    assert_eq!(
        std::fs::read(base.join("nested").join("b.bin")).unwrap(),
        bin
    );

    // A second restore into the now non-empty folder is refused.
    assert!(e.restore(&id, &out, None).is_err());
}

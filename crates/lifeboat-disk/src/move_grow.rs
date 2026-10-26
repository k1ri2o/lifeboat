//! Move-and-grow (#62): make Lifeboat bigger when the free space is in FRONT of it.
//!
//! Windows can only grow a partition into free space behind it, but space freed by shrinking
//! the drive in front of Lifeboat (D:) lands before it. So Lifeboat builds a second, bigger
//! partition in that space, copies its repository there, checks the copy, switches over, and
//! only then deletes the old partition and grows the new one into the room it left.
//!
//! This module holds the pure planner, the script text and the small execute steps. The order
//! of the steps, the config switch and the rollback live in the service, which owns the
//! password and the config. The old partition is never deleted here before the caller says
//! the copy was checked and the switch was saved.

use crate::acl::lock_down;
use crate::guard::{check, Op};
use crate::ops::{close_volume, create_and_find, hide_lifeboat_volume, list_disks, open_volume};
use crate::ps;
use crate::resize::{
    disk_with, extend_host_script, find_into_p, gib, is_removable_bus, lifeboat_of, remove_script,
    supported_size_script, HOST_GAP, MIB, MIN_GROW,
};
use crate::{Error, Result};
use lifeboat_core::disk::{is_basic_data_type, DiskInfo, PartitionInfo};
use serde::Deserialize;
use std::path::Path;

/// Room always left on the host drive on top of what Windows says it can give up.
pub const HOST_MARGIN: u64 = 1024 * MIB;
/// The new partition must hold the data plus this much headroom (and 10% on top).
const COPY_HEADROOM: u64 = 256 * MIB;

fn round_up_mib(b: u64) -> u64 {
    b.div_ceil(MIB) * MIB
}

/// Smallest amount taken from the host: what is in use, plus 10% and some headroom.
pub fn min_take(used: u64) -> u64 {
    round_up_mib(used + used / 10 + COPY_HEADROOM)
}

/// How much the host can give up: its supported shrink room minus a safety margin.
pub fn host_room(size: u64, size_min: u64) -> u64 {
    let room = size.saturating_sub(size_min).saturating_sub(HOST_MARGIN);
    room / MIB * MIB
}

/// What a move-and-grow would work with. Produced without side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovePlan {
    pub disk_number: u32,
    pub host_guid: String,
    pub host_letter: char,
    pub current_size: u64,
    /// How much the host can give up.
    pub room: u64,
}

impl MovePlan {
    /// `room` is [`host_room`] of the drive in front of Lifeboat. Refuses disk 0, the Windows
    /// disk, removable disks and a Lifeboat with no ordinary drive directly in front of it.
    pub fn build(disk: &DiskInfo, guid: &str, room: u64) -> Result<Self> {
        check(disk, Op::MoveLifeboat, false)?;
        if is_removable_bus(&disk.bus_type) {
            return Err(Error::Invalid(
                "Lifeboat only moves on an internal disk, never on a USB drive or memory card."
                    .into(),
            ));
        }
        let lb = lifeboat_of(disk, guid)?;
        let host = disk
            .partitions
            .iter()
            .filter(|p| host_candidate(p, guid, lb))
            .max_by_key(|p| p.offset)
            .ok_or_else(|| {
                Error::Invalid(
                    "There is no ordinary drive directly in front of Lifeboat to take space from."
                        .into(),
                )
            })?;
        Ok(Self {
            disk_number: disk.number,
            host_guid: host.guid.clone(),
            host_letter: host.drive_letter.unwrap_or('?'),
            current_size: lb.size,
            room,
        })
    }

    /// Largest total size reachable.
    pub fn max_target(&self) -> u64 {
        (self.current_size + self.room) / MIB * MIB
    }

    /// Validate `new_size` against what is in use. Returns the amount to take from the host.
    pub fn check_target(&self, used: u64, new_size: u64) -> Result<u64> {
        let size = new_size / MIB * MIB;
        if self.room < MIN_GROW {
            return Err(Error::Invalid(format!(
                "{}: has no room left to give to Lifeboat.",
                self.host_letter
            )));
        }
        if size < self.current_size + MIN_GROW {
            return Err(Error::Invalid(format!(
                "The new size must be at least {} bigger than now ({}).",
                gib(MIN_GROW),
                gib(self.current_size)
            )));
        }
        let x = size - self.current_size;
        let need = min_take(used);
        if x < need {
            return Err(Error::Invalid(format!(
                "To make a safe copy, Lifeboat needs at least {} of new room from {}:. The smallest size that works is {}.",
                gib(need),
                self.host_letter,
                gib(self.current_size + need)
            )));
        }
        if x > self.room {
            return Err(Error::Invalid(format!(
                "{}: can give at most {} to Lifeboat, so Lifeboat can grow to at most {}.",
                self.host_letter,
                gib(self.room),
                gib(self.max_target())
            )));
        }
        Ok(x)
    }
}

fn host_candidate(p: &PartitionInfo, guid: &str, lb: &PartitionInfo) -> bool {
    !p.has_guid(guid)
        && !p.guid.is_empty()
        && p.drive_letter.is_some()
        && is_basic_data_type(&p.gpt_type)
        && p.offset + p.size <= lb.offset
        && lb.offset - (p.offset + p.size) <= HOST_GAP
}

/// Shrink the host (found by GUID) by `x` bytes; throws when Windows would not allow it.
/// Prints the new size.
pub fn shrink_host_script(host_guid: &str, x: u64) -> String {
    format!(
        "{}$s = Get-PartitionSupportedSize -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber; \
         if ([uint64]$p.Size -le [uint64]{x}) {{ throw 'the drive is too small to give that much' }}; \
         $n = [uint64]$p.Size - [uint64]{x}; \
         if ($n -lt [uint64]$s.SizeMin) {{ throw 'the drive cannot shrink that far' }}; \
         Resize-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber -Size $n; \
         (Get-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber).Size",
        find_into_p(host_guid)
    )
}

/// Grow the partition to `target`, or as far as Windows allows when that is less (alignment
/// loses a little). A no-op when it cannot grow. Prints the size it ended with.
pub fn grow_up_to_script(guid: &str, target: u64) -> String {
    format!(
        "{}$s = Get-PartitionSupportedSize -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber; \
         $n = [Math]::Min([uint64]$s.SizeMax, [uint64]{target}); \
         if ($n -gt [uint64]$p.Size) {{ \
         Resize-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber -Size $n }}; \
         (Get-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber).Size",
        find_into_p(guid)
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Supported {
    size: u64,
    #[serde(default)]
    size_min: u64,
}

/// Parse [`supported_size_script`] output into the shrink room (see [`host_room`]).
pub fn parse_host_room(out: &str) -> Result<u64> {
    let s: Supported = serde_json::from_str(out.trim()).map_err(|e| Error::Parse(e.to_string()))?;
    Ok(host_room(s.size, s.size_min))
}

fn last_u64(out: &str) -> Result<u64> {
    out.lines()
        .rfind(|l| !l.trim().is_empty())
        .and_then(|l| l.trim().parse().ok())
        .ok_or_else(|| Error::Parse(format!("unexpected resize output: {out}")))
}

/// Read-only: the plan for growing by moving, with the host's real shrink room.
pub fn plan(guid: &str) -> Result<MovePlan> {
    let disk = disk_with(guid)?;
    let rough = MovePlan::build(&disk, guid, 0)?;
    let out = ps::run(&supported_size_script(&rough.host_guid))?;
    MovePlan::build(&disk, guid, parse_host_room(&out)?)
}

/// Shrink the host by `x`. Returns its new size.
pub fn shrink_host(host_guid: &str, x: u64) -> Result<u64> {
    let disk = disk_with(host_guid)?;
    check(&disk, Op::MoveLifeboat, false)?;
    last_u64(&ps::run(&shrink_host_script(host_guid, x))?)
}

/// Create the new hidden partition of about `x` bytes next to the old one, locked down to the
/// service and hidden. Returns its GUID. The caller records it in the journal at once.
pub fn create_new(old_guid: &str, x: u64) -> Result<String> {
    let disk = disk_with(old_guid)?;
    check(&disk, Op::MoveLifeboat, false)?;
    let old_offset = lifeboat_of(&disk, old_guid)?.offset;
    let made = create_and_find(disk.number, Some(x))?;
    let guid = made.partition_guid.clone();
    let finish = || -> Result<()> {
        // It must sit in the space taken from the host, in front of the old partition.
        let d = disk_with(&guid)?;
        let p = lifeboat_of(&d, &guid)?;
        if p.offset + p.size > old_offset {
            return Err(Error::Invalid(
                "The new partition did not land in the space taken from the drive.".into(),
            ));
        }
        lock_down(&made.volume_path)
    };
    let done = finish();
    let _ = hide_lifeboat_volume(&guid);
    done.map(|_| guid)
}

/// Files and bytes under `dir`.
pub fn tree_stats(dir: &Path) -> std::io::Result<(u64, u64)> {
    let (mut files, mut bytes) = (0, 0);
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let m = e.metadata()?;
        if m.is_dir() {
            let (f, b) = tree_stats(&e.path())?;
            files += f;
            bytes += b;
        } else {
            files += 1;
            bytes += m.len();
        }
    }
    Ok((files, bytes))
}

/// Copy a directory tree (the restic repository). Existing files are replaced.
pub fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.metadata()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// Copy `src/repo` to `dst/repo` and verify the file count and byte total match.
pub fn copy_and_verify(src_vol: &str, dst_vol: &str) -> Result<()> {
    let src = Path::new(src_vol).join("repo");
    let dst = Path::new(dst_vol).join("repo");
    let want = tree_stats(&src)?;
    copy_tree(&src, &dst)?;
    let got = tree_stats(&dst)?;
    if want != got {
        return Err(Error::Invalid(format!(
            "The copy does not match the original ({} files / {} bytes instead of {} / {}).",
            got.0, got.1, want.0, want.1
        )));
    }
    Ok(())
}

/// Open both volumes (marker verified), copy, and always hide them again.
pub fn copy_repo(old_guid: &str, new_guid: &str) -> Result<()> {
    let disk = disk_with(old_guid)?;
    check(&disk, Op::MoveLifeboat, false)?;
    let run = || -> Result<()> {
        let old = open_volume(old_guid, None)?
            .ok_or_else(|| Error::NotFound("the old Lifeboat volume (marker mismatch)".into()))?;
        let new = open_volume(new_guid, None)?
            .ok_or_else(|| Error::NotFound("the new Lifeboat volume (marker mismatch)".into()))?;
        copy_and_verify(&old, &new)
    };
    let result = run();
    let _ = close_volume(new_guid, None);
    let _ = close_volume(old_guid, None);
    result
}

/// Delete a Lifeboat partition by GUID after its marker file matched. The freed space is NOT
/// given to the drive in front of it.
pub fn remove_partition(guid: &str) -> Result<()> {
    let disk = disk_with(guid)?;
    check(&disk, Op::MoveLifeboat, false)?;
    let opened = open_volume(guid, None);
    if !matches!(opened, Ok(Some(_))) {
        let _ = close_volume(guid, None);
        return Err(Error::NotFound(
            "the Lifeboat volume (or its marker file does not match)".into(),
        ));
    }
    ps::run(&remove_script(guid)).map(|_| ())
}

/// Grow the (new) partition up to `target`. Returns the size it ended with.
pub fn grow_new(guid: &str, target: u64) -> Result<u64> {
    let disk = disk_with(guid)?;
    check(&disk, Op::MoveLifeboat, false)?;
    last_u64(&ps::run(&grow_up_to_script(guid, target))?)
}

/// Undo a move that stopped before the switch: delete the new partition (when there is one,
/// marker verified) and give the host its space back. `new_guid` may be empty when the
/// journal missed it: then the partition between the host and the old one is looked for.
pub fn rollback(old_guid: &str, host_guid: &str, new_guid: &str) -> Result<()> {
    let new = if new_guid.is_empty() {
        orphan_between(old_guid, host_guid)
    } else {
        Some(new_guid.to_string())
    };
    if let Some(n) = new {
        // Already gone (a repeated recovery): nothing to remove.
        if disk_with(&n).is_ok() {
            remove_partition(&n)?;
        }
    }
    let disk = disk_with(host_guid)?;
    check(&disk, Op::MoveLifeboat, false)?;
    ps::run(&extend_host_script(host_guid))?;
    Ok(())
}

/// A partition squeezed between the host and the old Lifeboat: only ours can be there.
fn orphan_between(old_guid: &str, host_guid: &str) -> Option<String> {
    let disks = list_disks().ok()?;
    let disk = disks
        .iter()
        .find(|d| d.partitions.iter().any(|p| p.has_guid(old_guid)))?;
    let old = disk.partitions.iter().find(|p| p.has_guid(old_guid))?;
    let host = disk.partitions.iter().find(|p| p.has_guid(host_guid))?;
    disk.partitions
        .iter()
        .find(|p| {
            !p.guid.is_empty()
                && !p.has_guid(old_guid)
                && !p.has_guid(host_guid)
                && p.offset >= host.offset + host.size
                && p.offset + p.size <= old.offset
        })
        .map(|p| p.guid.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lifeboat_core::disk::BASIC_DATA_TYPE_GUID;

    const G: u64 = 1024 * MIB;
    const LB: &str = "{aaaaaaaa-0000-0000-0000-000000000001}";
    const HOST: &str = "{bbbbbbbb-0000-0000-0000-000000000002}";

    fn part(
        number: u32,
        offset: u64,
        size: u64,
        letter: Option<char>,
        guid: &str,
    ) -> PartitionInfo {
        PartitionInfo {
            number,
            offset,
            size,
            gpt_type: BASIC_DATA_TYPE_GUID.into(),
            drive_letter: letter,
            is_hidden: letter.is_none(),
            no_default_letter: letter.is_none(),
            guid: guid.into(),
        }
    }

    fn disk(number: u32, bus: &str) -> DiskInfo {
        DiskInfo {
            number,
            model: "X".into(),
            size: 2000 * G,
            bus_type: bus.into(),
            is_boot: false,
            is_system: false,
            partitions: vec![
                part(1, MIB, 900 * G, Some('D'), HOST),
                part(2, 900 * G + MIB, 10 * G, None, LB),
                part(
                    3,
                    910 * G + 2 * MIB,
                    MIB,
                    None,
                    "{cccccccc-0000-0000-0000-000000000003}",
                ),
            ],
        }
    }

    #[test]
    fn plan_finds_the_drive_in_front() {
        let p = MovePlan::build(&disk(2, "SATA"), LB, 500 * G).unwrap();
        assert_eq!(p.host_guid, HOST);
        assert_eq!(p.host_letter, 'D');
        assert_eq!(p.current_size, 10 * G);
        assert_eq!(p.max_target(), 510 * G);
    }

    #[test]
    fn plan_refuses_disk_zero_windows_disk_and_removable() {
        assert!(MovePlan::build(&disk(0, "SATA"), LB, 500 * G).is_err());
        let mut d = disk(2, "SATA");
        d.is_system = true;
        assert!(MovePlan::build(&d, LB, 500 * G).is_err());
        let mut d = disk(2, "SATA");
        d.is_boot = true;
        assert!(MovePlan::build(&d, LB, 500 * G).is_err());
        assert!(MovePlan::build(&disk(2, "USB"), LB, 500 * G).is_err());
    }

    #[test]
    fn plan_needs_a_drive_directly_in_front() {
        let mut d = disk(2, "SATA");
        d.partitions[0].drive_letter = None;
        assert!(MovePlan::build(&d, LB, 500 * G).is_err());
        let mut d = disk(2, "SATA");
        d.partitions[0].size = 100 * G; // a big gap before Lifeboat
        assert!(MovePlan::build(&d, LB, 500 * G).is_err());
    }

    #[test]
    fn target_rules() {
        let p = MovePlan::build(&disk(2, "SATA"), LB, 100 * G).unwrap();
        let used = 4 * G;
        let need = min_take(used);
        assert!(need > used + 256 * MIB && need < used + used / 10 + 257 * MIB);
        assert_eq!(p.check_target(used, 10 * G + need).unwrap(), need);
        let low = p
            .check_target(used, 10 * G + 2 * G)
            .unwrap_err()
            .to_string();
        assert!(low.contains("smallest size that works is"), "{low}");
        let high = p.check_target(used, 200 * G).unwrap_err().to_string();
        assert!(high.contains("at most"), "{high}");
        assert!(p.check_target(used, 10 * G).is_err());
        assert!(p.check_target(used, 110 * G).is_ok());
        assert!(p.check_target(used, 110 * G + MIB).is_err());
    }

    #[test]
    fn no_room_is_said_plainly() {
        let p = MovePlan::build(&disk(2, "SATA"), LB, 0).unwrap();
        let e = p.check_target(G, 20 * G).unwrap_err().to_string();
        assert!(e.contains("D: has no room left to give"), "{e}");
    }

    #[test]
    fn host_room_keeps_a_margin() {
        assert_eq!(host_room(900 * G, 400 * G), 499 * G);
        assert_eq!(host_room(100 * G, 100 * G), 0);
        assert_eq!(host_room(100 * G, 200 * G), 0);
    }

    #[test]
    fn parses_host_room() {
        let out = r#"{"Size":966367641600,"SizeMin":429496729600,"SizeMax":966367641600}"#;
        assert_eq!(parse_host_room(out).unwrap(), 499 * G);
        assert!(parse_host_room("nope").is_err());
    }

    #[test]
    fn shrink_script_is_by_guid_and_guarded() {
        let s = shrink_host_script(HOST, 5 * G);
        assert!(s.contains(HOST));
        assert!(s.contains("Resize-Partition"));
        assert!(s.contains("$s.SizeMin"));
        assert!(s.contains("throw"));
        assert!(!s.contains("Remove-Partition"));
    }

    #[test]
    fn grow_script_is_clamped_and_never_shrinks() {
        let s = grow_up_to_script(LB, 20 * G);
        assert!(s.contains("[Math]::Min"));
        assert!(s.contains("-gt [uint64]$p.Size"));
        assert!(!s.contains("Remove-Partition"));
    }

    #[test]
    fn copies_and_verifies_a_tree() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let repo = a.path().join("repo");
        std::fs::create_dir_all(repo.join("data/ab")).unwrap();
        std::fs::write(repo.join("config"), b"c").unwrap();
        std::fs::write(repo.join("data/ab/pack"), vec![7u8; 1000]).unwrap();
        copy_and_verify(a.path().to_str().unwrap(), b.path().to_str().unwrap()).unwrap();
        assert_eq!(tree_stats(&b.path().join("repo")).unwrap(), (2, 1001));
        assert_eq!(
            std::fs::read(b.path().join("repo/data/ab/pack"))
                .unwrap()
                .len(),
            1000
        );
    }

    #[test]
    fn a_missing_source_fails() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert!(copy_and_verify(a.path().to_str().unwrap(), b.path().to_str().unwrap()).is_err());
    }
}

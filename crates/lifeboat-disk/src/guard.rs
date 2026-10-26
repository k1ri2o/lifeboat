//! Safety guard: decides whether an operation may touch a disk. Pure logic.

use lifeboat_core::disk::DiskInfo;

/// What the caller wants to do to a disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Initialise an empty external disk and create the hidden partition.
    PrepareExternal,
    /// Shrink a volume on the disk and create the hidden partition (the only op allowed on
    /// a disk that holds other drives).
    InternalShrink,
    SetOffline,
    SetOnline,
    /// Grow the Lifeboat partition in place (any disk that holds it).
    GrowLifeboat,
    /// Delete the Lifeboat partition (any disk that holds it).
    RemoveLifeboat,
    /// Shrink the drive in front of Lifeboat and build a bigger Lifeboat there (#62). Only on
    /// a data disk that holds Lifeboat and another drive; never the Windows disk or disk 0.
    MoveLifeboat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// Boot or system disk and the op is not the explicit internal shrink.
    BootOrSystemDisk(u32),
    /// Internal shrink on a disk with no drive on it to make room from.
    NotInternalDisk(u32),
    /// Offline/online on a disk that also holds other drives (they would vanish).
    SharedDisk(u32),
    /// The disk already has a Lifeboat partition.
    AlreadyHasLifeboat(u32),
    /// External prepare on a disk that already has partitions.
    DiskNotEmpty(u32),
    /// Grow or remove on a disk that has no Lifeboat partition.
    NoLifeboat(u32),
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::BootOrSystemDisk(n) => {
                write!(f, "disk {n} holds Windows (boot/system)")
            }
            GuardError::NotInternalDisk(n) => {
                write!(f, "disk {n} has no drive on it to make room from")
            }
            GuardError::SharedDisk(n) => {
                write!(f, "disk {n} also holds other drives; it must stay online")
            }
            GuardError::AlreadyHasLifeboat(n) => {
                write!(f, "disk {n} already has a Lifeboat partition")
            }
            GuardError::DiskNotEmpty(n) => write!(f, "disk {n} is not empty; refusing to wipe it"),
            GuardError::NoLifeboat(n) => write!(f, "disk {n} has no Lifeboat partition"),
        }
    }
}

impl std::error::Error for GuardError {}

/// Check `op` on `disk`. `reuse` allows a disk that already carries a Lifeboat partition.
pub fn check(disk: &DiskInfo, op: Op, reuse: bool) -> Result<(), GuardError> {
    let protected = disk.is_boot || disk.is_system;
    match op {
        Op::GrowLifeboat | Op::RemoveLifeboat => {
            // Allowed on a boot disk too (the partition is found by GUID by the caller), but
            // only when the disk really holds a Lifeboat partition.
            if !disk.has_lifeboat_partition() {
                return Err(GuardError::NoLifeboat(disk.number));
            }
        }
        Op::MoveLifeboat => {
            if protected || disk.number == 0 {
                return Err(GuardError::BootOrSystemDisk(disk.number));
            }
            if !disk.has_lifeboat_partition() {
                return Err(GuardError::NoLifeboat(disk.number));
            }
            if !disk.is_shared() {
                return Err(GuardError::NotInternalDisk(disk.number));
            }
        }
        Op::SetOffline | Op::SetOnline => {
            if protected {
                return Err(GuardError::BootOrSystemDisk(disk.number));
            }
            // Only going offline can make other drives vanish. Bringing a disk online is
            // always safe, and an offline disk reports its partitions unreliably.
            if op == Op::SetOffline && disk.is_shared() {
                return Err(GuardError::SharedDisk(disk.number));
            }
        }
        Op::PrepareExternal => {
            if protected {
                return Err(GuardError::BootOrSystemDisk(disk.number));
            }
            if disk.has_lifeboat_partition() {
                if !reuse {
                    return Err(GuardError::AlreadyHasLifeboat(disk.number));
                }
            } else if !disk.partitions.is_empty() {
                // Never initialise over somebody's partitions.
                return Err(GuardError::DiskNotEmpty(disk.number));
            }
        }
        Op::InternalShrink => {
            // Any internal disk with a drive on it (C: on the Windows disk, or D: on a
            // second drive). The disk then counts as shared and is never taken offline.
            if !disk.is_shared() {
                return Err(GuardError::NotInternalDisk(disk.number));
            }
            if disk.has_lifeboat_partition() && !reuse {
                return Err(GuardError::AlreadyHasLifeboat(disk.number));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lifeboat_core::disk::{PartitionInfo, BASIC_DATA_TYPE_GUID};

    fn disk(boot: bool, system: bool, parts: Vec<PartitionInfo>) -> DiskInfo {
        DiskInfo {
            number: 3,
            model: "X".into(),
            size: 1 << 40,
            bus_type: "USB".into(),
            is_boot: boot,
            is_system: system,
            partitions: parts,
        }
    }

    fn part(guid: &str) -> PartitionInfo {
        PartitionInfo {
            number: 1,
            offset: 0,
            size: 1 << 30,
            gpt_type: guid.into(),
            drive_letter: None,
            is_hidden: true,
            no_default_letter: true,
            guid: "{g}".into(),
        }
    }

    /// An ordinary visible data partition (same basic data type, but with a drive letter).
    fn user_part() -> PartitionInfo {
        PartitionInfo {
            drive_letter: Some('E'),
            is_hidden: false,
            no_default_letter: false,
            ..part(BASIC_DATA_TYPE_GUID)
        }
    }

    #[test]
    fn boot_system_refused_except_shrink() {
        for d in [disk(true, false, vec![]), disk(false, true, vec![])] {
            assert!(check(&d, Op::PrepareExternal, false).is_err());
            assert!(check(&d, Op::SetOffline, false).is_err());
            assert!(check(&d, Op::SetOnline, true).is_err());
            assert_eq!(check(&d, Op::InternalShrink, false), Ok(()));
        }
    }

    #[test]
    fn shrink_needs_internal_disk() {
        assert_eq!(
            check(&disk(false, false, vec![]), Op::InternalShrink, false),
            Err(GuardError::NotInternalDisk(3))
        );
        // A second internal drive holding D: may make room for Lifeboat...
        let second = disk(false, false, vec![user_part()]);
        assert_eq!(check(&second, Op::InternalShrink, false), Ok(()));
        // ...but must never be taken offline afterwards, or D: would vanish.
        assert_eq!(
            check(&second, Op::SetOffline, true),
            Err(GuardError::SharedDisk(3))
        );
        assert_eq!(check(&second, Op::SetOnline, true), Ok(()));
        let with_lb = disk(false, false, vec![user_part(), part(BASIC_DATA_TYPE_GUID)]);
        assert!(check(&with_lb, Op::InternalShrink, false).is_err());
        assert!(check(&with_lb, Op::SetOffline, true).is_err());
    }

    #[test]
    fn external_ok_when_empty() {
        assert_eq!(
            check(&disk(false, false, vec![]), Op::PrepareExternal, false),
            Ok(())
        );
    }

    #[test]
    fn existing_lifeboat_needs_reuse() {
        let d = disk(false, false, vec![part(BASIC_DATA_TYPE_GUID)]);
        assert_eq!(
            check(&d, Op::PrepareExternal, false),
            Err(GuardError::AlreadyHasLifeboat(3))
        );
        assert_eq!(check(&d, Op::PrepareExternal, true), Ok(()));
        let sys = disk(true, true, vec![part(BASIC_DATA_TYPE_GUID)]);
        assert!(check(&sys, Op::InternalShrink, false).is_err());
        assert_eq!(check(&sys, Op::InternalShrink, true), Ok(()));
    }

    #[test]
    fn non_empty_external_refused() {
        let d = disk(false, false, vec![user_part()]);
        assert_eq!(
            check(&d, Op::PrepareExternal, true),
            Err(GuardError::DiskNotEmpty(3))
        );
    }

    #[test]
    fn grow_and_remove_need_a_lifeboat_partition() {
        let with = disk(true, true, vec![user_part(), part(BASIC_DATA_TYPE_GUID)]);
        let without = disk(false, false, vec![user_part()]);
        for op in [Op::GrowLifeboat, Op::RemoveLifeboat] {
            assert_eq!(check(&with, op, false), Ok(()));
            assert_eq!(check(&without, op, false), Err(GuardError::NoLifeboat(3)));
        }
    }

    #[test]
    fn offline_online_ok_on_data_disk() {
        let d = disk(false, false, vec![part(BASIC_DATA_TYPE_GUID)]);
        assert_eq!(check(&d, Op::SetOffline, false), Ok(()));
        assert_eq!(check(&d, Op::SetOnline, false), Ok(()));
    }

    #[test]
    fn move_never_touches_the_windows_disk_or_disk_zero() {
        let parts = vec![user_part(), part(BASIC_DATA_TYPE_GUID)];
        assert_eq!(
            check(&disk(false, false, parts.clone()), Op::MoveLifeboat, false),
            Ok(())
        );
        for d in [
            disk(true, false, parts.clone()),
            disk(false, true, parts.clone()),
        ] {
            assert_eq!(
                check(&d, Op::MoveLifeboat, false),
                Err(GuardError::BootOrSystemDisk(3))
            );
        }
        let mut zero = disk(false, false, parts.clone());
        zero.number = 0;
        assert_eq!(
            check(&zero, Op::MoveLifeboat, false),
            Err(GuardError::BootOrSystemDisk(0))
        );
        // No Lifeboat, or nothing in front of it to take space from.
        assert_eq!(
            check(&disk(false, false, vec![user_part()]), Op::MoveLifeboat, false),
            Err(GuardError::NoLifeboat(3))
        );
        assert_eq!(
            check(
                &disk(false, false, vec![part(BASIC_DATA_TYPE_GUID)]),
                Op::MoveLifeboat,
                false
            ),
            Err(GuardError::NotInternalDisk(3))
        );
    }
}

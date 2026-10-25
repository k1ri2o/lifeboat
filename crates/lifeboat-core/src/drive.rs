//! Read-only drive statistics for the "Drives" card in Settings.

use crate::config::CheckResult;
use crate::disk::{same_guid, DiskInfo};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One disk in the Drives list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveDisk {
    pub number: u32,
    pub model: String,
    pub size: u64,
    /// Bus type as Windows names it ("USB", "SATA", "NVMe", ...).
    pub bus_type: String,
    /// Windows runs from this disk.
    pub is_system: bool,
    /// Drive letters of its partitions, e.g. `["C", "D"]`.
    pub letters: Vec<String>,
    /// The Lifeboat partition is on this disk.
    pub holds_lifeboat: bool,
}

/// What is known about the Lifeboat partition. Every number is `None` when it could not be
/// read (for example while the disk is closed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifeboatDrive {
    pub disk_number: Option<u32>,
    pub partition_size: Option<u64>,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// Size of the backup repository on disk.
    pub repo_bytes: Option<u64>,
    pub snapshot_count: Option<u32>,
    pub vault_count: Option<u32>,
    /// A backed-up folder is on the same physical disk as the backups.
    pub same_physical_disk: bool,
    pub last_check: Option<CheckResult>,
    /// The backup disk is open right now; the numbers are fresh. Otherwise they are the last
    /// ones seen (or missing).
    pub open: bool,
}

/// The disk that holds the partition with `guid`, if any.
pub fn lifeboat_disk<'a>(disks: &'a [DiskInfo], guid: &str) -> Option<&'a DiskInfo> {
    disks
        .iter()
        .find(|d| d.partitions.iter().any(|p| same_guid(&p.guid, guid)))
}

/// The disk list for the UI. `guid` is the saved Lifeboat partition GUID (may be empty).
pub fn drive_disks(disks: &[DiskInfo], guid: &str) -> Vec<DriveDisk> {
    let home = lifeboat_disk(disks, guid).map(|d| d.number);
    disks
        .iter()
        .map(|d| DriveDisk {
            number: d.number,
            model: d.model.clone(),
            size: d.size,
            bus_type: d.bus_type.clone(),
            is_system: d.is_system || d.is_boot,
            letters: d
                .partitions
                .iter()
                .filter_map(|p| p.drive_letter)
                .map(|c| c.to_ascii_uppercase().to_string())
                .collect(),
            holds_lifeboat: Some(d.number) == home,
        })
        .collect()
}

/// Is a backed-up folder on the same physical disk as the Lifeboat partition?
pub fn same_disk(disks: &[DiskInfo], guid: &str, folders: &[PathBuf]) -> bool {
    let Some(d) = lifeboat_disk(disks, guid) else {
        return false;
    };
    folders.iter().any(|f| {
        let s = f.to_string_lossy();
        let letter = s.chars().next().filter(|c| c.is_ascii_alphabetic());
        letter.is_some_and(|l| {
            d.partitions
                .iter()
                .any(|p| p.drive_letter.is_some_and(|x| x.eq_ignore_ascii_case(&l)))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::{PartitionInfo, BASIC_DATA_TYPE_GUID};

    fn part(letter: Option<char>, guid: &str, hidden: bool) -> PartitionInfo {
        PartitionInfo {
            number: 1,
            offset: 0,
            size: 1 << 30,
            gpt_type: BASIC_DATA_TYPE_GUID.into(),
            drive_letter: letter,
            is_hidden: hidden,
            no_default_letter: hidden,
            guid: guid.into(),
        }
    }

    fn disks() -> Vec<DiskInfo> {
        vec![
            DiskInfo {
                number: 0,
                model: "Samsung SSD".into(),
                size: 500 << 30,
                bus_type: "NVMe".into(),
                is_boot: true,
                is_system: true,
                partitions: vec![part(Some('c'), "{a}", false)],
            },
            DiskInfo {
                number: 1,
                model: "WD Elements".into(),
                size: 2000 << 30,
                bus_type: "USB".into(),
                is_boot: false,
                is_system: false,
                partitions: vec![part(Some('E'), "{b}", false), part(None, "{C-1}", true)],
            },
        ]
    }

    #[test]
    fn marks_the_disk_that_holds_lifeboat() {
        let v = drive_disks(&disks(), "c-1");
        assert!(!v[0].holds_lifeboat && v[1].holds_lifeboat);
        assert_eq!(v[0].letters, vec!["C"]);
        assert_eq!(v[1].letters, vec!["E"]);
        assert!(v[0].is_system && !v[1].is_system);
    }

    #[test]
    fn no_guid_marks_nothing() {
        assert!(drive_disks(&disks(), "").iter().all(|d| !d.holds_lifeboat));
    }

    #[test]
    fn same_disk_looks_at_folder_letters() {
        let f = |s: &str| vec![PathBuf::from(s)];
        assert!(same_disk(&disks(), "c-1", &f("e:/Photos")));
        assert!(!same_disk(&disks(), "c-1", &f("C:/Users")));
        assert!(!same_disk(&disks(), "", &f("E:/")));
    }
}

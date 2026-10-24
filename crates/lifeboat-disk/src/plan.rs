//! Internal-disk setup: shrink a volume, then create the hidden partition in the freed space.

use crate::guard::{check, Op};
use crate::ops::{create_and_find, disk_by_number, PreparedVolume};
use crate::ps::{self, quote};
use crate::{Error, Result};
use serde::Deserialize;

const MIB: u64 = 1024 * 1024;

/// What a shrink will do. Produced without side effects by [`plan_shrink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShrinkPlan {
    pub disk_number: u32,
    pub drive_letter: char,
    pub current_size: u64,
    /// Smallest size Windows allows the volume to shrink to.
    pub min_size: u64,
    /// Requested size of the Lifeboat partition.
    pub lifeboat_size: u64,
    /// Size of the volume after the shrink.
    pub new_size: u64,
}

fn gib(b: u64) -> String {
    format!("{:.1} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0))
}

impl ShrinkPlan {
    /// Build a plan from the numbers Windows reports; fails when the free space is too small.
    pub fn build(
        disk_number: u32,
        drive_letter: char,
        current_size: u64,
        min_size: u64,
        lifeboat_size: u64,
    ) -> Result<Self> {
        let lifeboat_size = lifeboat_size / MIB * MIB;
        if lifeboat_size < 512 * MIB {
            return Err(Error::Invalid(
                "Lifeboat partition must be at least 512 MiB".into(),
            ));
        }
        let new_size = current_size.saturating_sub(lifeboat_size);
        if new_size < min_size {
            return Err(Error::Invalid(format!(
                "not enough free space on {drive_letter}: can free at most {}, asked for {}",
                gib(current_size.saturating_sub(min_size)),
                gib(lifeboat_size)
            )));
        }
        Ok(Self {
            disk_number,
            drive_letter,
            current_size,
            min_size,
            lifeboat_size,
            new_size,
        })
    }

    /// Plain-words description for the confirmation screen.
    pub fn describe(&self) -> String {
        format!(
            "Volume {l}: on disk {d} is {cur}. Windows can shrink it to {min} at the smallest.\n\
             Lifeboat will shrink {l}: to {new} (frees {lb}) and create a hidden {lb} partition \
             in the freed space. No files are moved or deleted. The new partition gets no drive \
             letter and only the Lifeboat service can open it.",
            l = self.drive_letter,
            d = self.disk_number,
            cur = gib(self.current_size),
            min = gib(self.min_size),
            new = gib(self.new_size),
            lb = gib(self.lifeboat_size),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Sizes {
    disk: u32,
    size: u64,
    size_min: u64,
}

/// Read-only: compute what shrinking volume `drive_letter` by `lifeboat_size` bytes would do.
pub fn plan_shrink(drive_letter: char, lifeboat_size: u64) -> Result<ShrinkPlan> {
    if !drive_letter.is_ascii_alphabetic() {
        return Err(Error::Invalid("drive letter must be A-Z".into()));
    }
    let out = ps::run(&format!(
        "$l = {}; $p = Get-Partition -DriveLetter $l; $s = Get-PartitionSupportedSize -DriveLetter $l; \
         ConvertTo-Json -Compress -InputObject ([pscustomobject]@{{ Disk=[int]$p.DiskNumber; \
         Size=[uint64]$p.Size; SizeMin=[uint64]$s.SizeMin }})",
        quote(&drive_letter.to_string())
    ))?;
    let s: Sizes = serde_json::from_str(&out).map_err(|e| Error::Parse(e.to_string()))?;
    ShrinkPlan::build(
        s.disk,
        drive_letter.to_ascii_uppercase(),
        s.size,
        s.size_min,
        lifeboat_size,
    )
}

/// Execute `plan`: shrink the volume, create, format and hide the partition. Returns the new
/// volume and partition GUID. The guard is re-checked against the live disk first.
pub fn execute_shrink(plan: &ShrinkPlan) -> Result<PreparedVolume> {
    let disk = disk_by_number(plan.disk_number)?;
    check(&disk, Op::InternalShrink, false)?;
    ps::run(&format!(
        "Resize-Partition -DriveLetter {} -Size {}",
        quote(&plan.drive_letter.to_string()),
        plan.new_size
    ))?;
    create_and_find(plan.disk_number, Some(plan.lifeboat_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1024 * 1024 * 1024;

    #[test]
    fn plan_numbers_and_text() {
        let p = ShrinkPlan::build(0, 'C', 500 * G, 200 * G, 100 * G).unwrap();
        assert_eq!(p.new_size, 400 * G);
        let t = p.describe();
        assert!(t.contains("500.0 GiB"), "{t}");
        assert!(t.contains("200.0 GiB"), "{t}");
        assert!(t.contains("400.0 GiB"), "{t}");
        assert!(t.contains("100.0 GiB"), "{t}");
        assert!(t.contains("no drive letter"), "{t}");
    }

    #[test]
    fn plan_rejects_too_big_or_tiny() {
        assert!(ShrinkPlan::build(0, 'C', 500 * G, 450 * G, 100 * G).is_err());
        assert!(ShrinkPlan::build(0, 'C', 500 * G, 100 * G, 10 * MIB).is_err());
    }

    #[test]
    fn size_rounds_down_to_mib() {
        let p = ShrinkPlan::build(0, 'C', 500 * G, 100 * G, 10 * G + 12345).unwrap();
        assert_eq!(p.lifeboat_size, 10 * G);
    }
}

//! Grow and delete the Lifeboat partition.
//!
//! Layout decision (grow): Windows can only grow a partition into free space directly AFTER it.
//! Setup shrinks a drive and creates Lifeboat in the freed space behind it, so Lifeboat sits at
//! the end of the old extent. Free space after Lifeboat exists only when the disk had
//! unallocated room there already (an external disk that was not fully used, a gap before a
//! recovery partition). Space freed later by shrinking a drive lands BEFORE Lifeboat and cannot
//! be used in place; moving or recreating a partition that holds the backups is far too risky,
//! so then growing is refused with a plain explanation.
//!
//! Everything that decides what to do is a pure function (planner, script text); the execute
//! functions at the bottom run the scripts and are never exercised by unit tests.

use crate::acl::lock_down;
use crate::guard::{check, Op};
use crate::ops::{braced, close_volume, list_disks, open_volume};
use crate::ps::{self, quote};
use crate::{Error, Result};
use lifeboat_core::disk::{is_basic_data_type, DiskInfo, PartitionInfo, BASIC_DATA_TYPE_GUID};
use serde::Deserialize;

pub(crate) const MIB: u64 = 1024 * 1024;
/// Growing by less than this is not worth a resize.
pub(crate) const MIN_GROW: u64 = 64 * MIB;
/// Largest gap between the host volume and Lifeboat that still counts as "directly before".
pub(crate) const HOST_GAP: u64 = 8 * MIB;
/// Free space in front of Lifeboat worth mentioning in the refusal text.
const FRONT_HINT: u64 = 1024 * MIB;

pub(crate) fn gib(b: u64) -> String {
    format!("{:.1} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0))
}

pub(crate) fn lifeboat_of<'a>(disk: &'a DiskInfo, guid: &str) -> Result<&'a PartitionInfo> {
    disk.partitions
        .iter()
        .find(|p| p.has_guid(guid))
        .ok_or_else(|| Error::NotFound("the Lifeboat partition".into()))
}

/// How far the Lifeboat partition can grow, and why not when it can't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowInfo {
    pub current_size: u64,
    /// Largest size Lifeboat can be given (never below `current_size`).
    pub max_size: u64,
    pub can_grow: bool,
    /// Plain-English reason when `can_grow` is false; empty otherwise.
    pub reason: String,
    /// The smallest size Windows could shrink this partition to (its data plus a safety
    /// margin). Not the space in use: the service measures that from the free space.
    pub used: Option<u64>,
}

impl GrowInfo {
    /// `supported_max` is what `Get-PartitionSupportedSize` reports (None: not asked, the
    /// free space after the partition is used alone).
    pub fn build(disk: &DiskInfo, guid: &str, supported_max: Option<u64>) -> Result<Self> {
        let lb = lifeboat_of(disk, guid)?;
        let end = lb.offset + lb.size;
        let next = disk
            .partitions
            .iter()
            .filter(|p| p.offset > lb.offset)
            .map(|p| p.offset)
            .min()
            .unwrap_or(disk.size);
        let free_after = next.saturating_sub(end);
        let mut max = lb.size + free_after;
        if let Some(s) = supported_max {
            max = max.min(s);
        }
        let max_size = (max / MIB * MIB).max(lb.size);
        let can_grow = max_size >= lb.size + MIN_GROW;
        let reason = if can_grow {
            String::new()
        } else {
            let before = disk
                .partitions
                .iter()
                .filter(|p| p.offset < lb.offset)
                .map(|p| p.offset + p.size)
                .max()
                .unwrap_or(0);
            let free_before = lb.offset.saturating_sub(before);
            if free_before >= FRONT_HINT {
                format!(
                    "Lifeboat can't grow here because the free space ({}) is in front of it, and Windows can only grow a partition into space behind it. Moving a partition that holds your backups is too risky, so Lifeboat stays the size it is.",
                    gib(free_before)
                )
            } else {
                "Lifeboat can't grow here because there is no free space directly behind it on its disk. Free some space behind it first, or use a bigger drive.".to_string()
            }
        };
        Ok(Self {
            current_size: lb.size,
            max_size,
            can_grow,
            reason,
            used: None,
        })
    }

    /// Validate a requested size; returns it rounded down to whole MiB.
    pub fn check_size(&self, new_size: u64) -> Result<u64> {
        if !self.can_grow {
            return Err(Error::Invalid(self.reason.clone()));
        }
        let size = new_size / MIB * MIB;
        if size < self.current_size + MIN_GROW {
            return Err(Error::Invalid(format!(
                "The new size must be at least {} bigger than now ({}).",
                gib(MIN_GROW),
                gib(self.current_size)
            )));
        }
        if size > self.max_size {
            return Err(Error::Invalid(format!(
                "Lifeboat can grow to at most {}; {} is too much.",
                gib(self.max_size),
                gib(size)
            )));
        }
        Ok(size)
    }
}

/// PowerShell that finds the partition with unique GUID `guid` into `$p` (throws when absent).
pub(crate) fn find_into_p(guid: &str) -> String {
    format!(
        "$p = Get-Partition | Where-Object {{ $_.Guid -eq {} }} | Select-Object -First 1; \
         if (-not $p) {{ throw 'partition not found' }}; ",
        quote(&braced(guid))
    )
}

/// Read-only: current size and the supported size range of the partition, as JSON.
pub fn supported_size_script(guid: &str) -> String {
    format!(
        "{}$s = Get-PartitionSupportedSize -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber; \
         [pscustomobject]@{{ Size = [uint64]$p.Size; SizeMin = [uint64]$s.SizeMin; SizeMax = [uint64]$s.SizeMax }} | ConvertTo-Json -Compress",
        find_into_p(guid)
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Supported {
    size: u64,
    #[serde(default)]
    size_min: u64,
    size_max: u64,
}

/// Parse [`supported_size_script`] output into (current size, supported maximum).
pub fn parse_supported(out: &str) -> Result<(u64, u64)> {
    let s: Supported = serde_json::from_str(out.trim()).map_err(|e| Error::Parse(e.to_string()))?;
    Ok((s.size, s.size_max))
}

/// The minimum supported size from [`supported_size_script`] output: about what is in use.
pub fn parse_used(out: &str) -> Option<u64> {
    let s: Supported = serde_json::from_str(out.trim()).ok()?;
    (s.size_min > 0).then_some(s.size_min)
}

/// Grow the partition to `new_size` bytes. Throws when Windows allows less or when the size
/// would not be a growth. The partition is found by its unique GUID, never by number alone.
pub fn grow_script(guid: &str, new_size: u64) -> String {
    format!(
        "{}$s = Get-PartitionSupportedSize -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber; \
         if ([uint64]$s.SizeMax -lt {n}) {{ throw ('Windows allows a size of at most ' + $s.SizeMax + ' bytes here') }}; \
         if ([uint64]$p.Size -ge {n}) {{ throw 'the new size is not bigger than the current one' }}; \
         Resize-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber -Size {n}; \
         (Get-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber).Size",
        find_into_p(guid),
        n = new_size
    )
}

pub(crate) fn disk_with(guid: &str) -> Result<DiskInfo> {
    list_disks()?
        .into_iter()
        .find(|d| d.partitions.iter().any(|p| p.has_guid(guid)))
        .ok_or_else(|| Error::NotFound("the disk holding Lifeboat".into()))
}

/// Read-only: can the Lifeboat partition grow, and by how much?
pub fn grow_info(guid: &str) -> Result<GrowInfo> {
    let disk = disk_with(guid)?;
    let out = ps::run(&supported_size_script(guid)).ok();
    let supported = out
        .as_deref()
        .and_then(|o| parse_supported(o).ok())
        .map(|(_, max)| max);
    let mut info = GrowInfo::build(&disk, guid, supported)?;
    info.used = out.as_deref().and_then(parse_used);
    Ok(info)
}

/// Grow the Lifeboat partition to `new_size`, keep it hidden and locked down. Returns the
/// size it ended up with.
pub fn grow_lifeboat(guid: &str, new_size: u64) -> Result<u64> {
    let disk = disk_with(guid)?;
    check(&disk, Op::GrowLifeboat, false)?;
    let info = GrowInfo::build(&disk, guid, None)?;
    let size = info.check_size(new_size)?;
    let own = (!disk.is_shared()).then_some(&disk);
    let volume = open_volume(guid, own)?.ok_or_else(|| {
        Error::NotFound("the Lifeboat volume (or its marker file does not match)".into())
    })?;
    let result = ps::run(&grow_script(guid, size))
        .and_then(|out| {
            out.lines()
                .last()
                .and_then(|l| l.trim().parse::<u64>().ok())
                .ok_or_else(|| Error::Parse(format!("unexpected resize output: {out}")))
        })
        .and_then(|new| lock_down(&volume).map(|_| new));
    // Always hide it again (and take a Lifeboat-only disk offline), also when growing failed.
    let closed = close_volume(guid, own);
    let new = result?;
    closed?;
    Ok(new)
}

/// What deleting Lifeboat will do. Produced without side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovePlan {
    pub disk_number: u32,
    /// The drive directly in front of Lifeboat that gets the freed space back (internal disks).
    pub host_guid: Option<String>,
    pub host_letter: Option<char>,
}

/// USB and other removable buses (SD cards, 1394) count as external.
pub(crate) fn is_removable_bus(bus: &str) -> bool {
    let b = bus.to_ascii_lowercase();
    b.contains("usb") || b.contains("sd") || b.contains("mmc") || b.contains("1394")
}

impl RemovePlan {
    pub fn build(disk: &DiskInfo, guid: &str) -> Result<Self> {
        let lb = lifeboat_of(disk, guid)?;
        // Only an internal disk gives space back to a neighbour: on a USB or other removable
        // disk the partition in front is somebody's data drive, never ours to grow.
        let host = if disk.is_shared() && !is_removable_bus(&disk.bus_type) {
            disk.partitions
                .iter()
                .filter(|p| {
                    !p.has_guid(guid)
                        && !p.guid.is_empty()
                        && p.drive_letter.is_some()
                        && is_basic_data_type(&p.gpt_type)
                        && p.offset + p.size <= lb.offset
                        && lb.offset - (p.offset + p.size) <= HOST_GAP
                })
                .max_by_key(|p| p.offset)
        } else {
            None
        };
        Ok(Self {
            disk_number: disk.number,
            host_guid: host.map(|p| p.guid.clone()),
            host_letter: host.and_then(|p| p.drive_letter),
        })
    }
}

/// Delete the partition with unique GUID `guid`. Found by GUID, checked to be a basic-data
/// partition, then removed by the number it has right now: never by number alone.
pub fn remove_script(guid: &str) -> String {
    format!(
        "{}if (-not ($p.GptType -ieq {})) {{ throw 'not a basic data partition' }}; \
         Remove-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber -Confirm:$false",
        find_into_p(guid),
        quote(BASIC_DATA_TYPE_GUID)
    )
}

/// Extend the host volume (found by GUID) into free space right behind it; a no-op when
/// there is none. Prints the new size.
pub fn extend_host_script(host_guid: &str) -> String {
    format!(
        "{}$s = Get-PartitionSupportedSize -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber; \
         if ([uint64]$s.SizeMax -gt [uint64]$p.Size) {{ \
         Resize-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber -Size $s.SizeMax }}; \
         (Get-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber).Size",
        find_into_p(host_guid)
    )
}

/// What [`remove_lifeboat`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// Drive letter that got the space back, when it did.
    pub extended_host: Option<char>,
    /// A plain-English note when the space could not be given back; empty otherwise.
    pub note: String,
}

/// Delete the Lifeboat partition and give its space back to the drive in front of it.
/// Verifies the partition by GUID and its marker file first (`open_volume` does that). The
/// caller has already checked the password and confirmation word and released the volume.
pub fn remove_lifeboat(guid: &str) -> Result<RemoveOutcome> {
    let disk = disk_with(guid)?;
    check(&disk, Op::RemoveLifeboat, false)?;
    let plan = RemovePlan::build(&disk, guid)?;
    let own = (!disk.is_shared()).then_some(&disk);
    let opened = match open_volume(guid, own) {
        Ok(v) => v,
        Err(e) => {
            // Re-hide / offline the disk also when opening failed half way.
            let _ = close_volume(guid, own);
            return Err(e);
        }
    };
    if opened.is_none() {
        let _ = close_volume(guid, own);
        return Err(Error::NotFound(
            "the Lifeboat volume (or its marker file does not match)".into(),
        ));
    }
    if let Err(e) = ps::run(&remove_script(guid)) {
        let _ = close_volume(guid, own);
        return Err(e);
    }
    let mut out = RemoveOutcome {
        extended_host: None,
        note: String::new(),
    };
    if let (Some(host), Some(letter)) = (&plan.host_guid, plan.host_letter) {
        match ps::run(&extend_host_script(host)) {
            Ok(_) => out.extended_host = Some(letter),
            Err(e) => {
                out.note = format!(
                    "Lifeboat is deleted, but drive {letter}: could not take the space back ({e}). You can extend it in Disk Management."
                )
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn disk(size: u64, parts: Vec<PartitionInfo>) -> DiskInfo {
        DiskInfo {
            number: 2,
            model: "X".into(),
            size,
            bus_type: "SATA".into(),
            is_boot: false,
            is_system: false,
            partitions: parts,
        }
    }

    #[test]
    fn grows_into_space_behind() {
        let d = disk(
            500 * G,
            vec![
                part(1, MIB, 100 * G, Some('D'), HOST),
                part(2, 101 * G, 50 * G, None, LB),
            ],
        );
        let i = GrowInfo::build(&d, LB, None).unwrap();
        assert!(i.can_grow);
        assert_eq!(i.current_size, 50 * G);
        assert_eq!(i.max_size, 500 * G - 101 * G);
        let i = GrowInfo::build(&d, LB, Some(120 * G)).unwrap();
        assert_eq!(i.max_size, 120 * G);
        assert_eq!(i.check_size(100 * G + 5).unwrap(), 100 * G);
    }

    #[test]
    fn stops_at_next_partition() {
        let d = disk(
            500 * G,
            vec![
                part(1, 101 * G, 50 * G, None, LB),
                part(2, 160 * G, 10 * G, Some('R'), HOST),
            ],
        );
        let i = GrowInfo::build(&d, LB, None).unwrap();
        assert_eq!(i.max_size, 59 * G);
    }

    #[test]
    fn refuses_when_free_space_is_in_front() {
        let d = disk(
            500 * G,
            vec![
                part(1, MIB, 100 * G, Some('D'), HOST),
                part(2, 300 * G, 200 * G - 2 * MIB, None, LB),
            ],
        );
        let i = GrowInfo::build(&d, LB, None).unwrap();
        assert!(!i.can_grow);
        assert!(
            i.reason.starts_with("Lifeboat can't grow here because"),
            "{}",
            i.reason
        );
        assert!(i.reason.contains("in front of it"), "{}", i.reason);
        assert!(i.check_size(250 * G).is_err());
    }

    #[test]
    fn refuses_when_no_space_at_all() {
        let d = disk(100 * G, vec![part(1, MIB, 100 * G - 2 * MIB, None, LB)]);
        let i = GrowInfo::build(&d, LB, None).unwrap();
        assert!(!i.can_grow);
        assert!(
            i.reason.contains("no free space directly behind"),
            "{}",
            i.reason
        );
    }

    #[test]
    fn size_checks() {
        let d = disk(500 * G, vec![part(1, MIB, 50 * G, None, LB)]);
        let i = GrowInfo::build(&d, LB, None).unwrap();
        assert!(i.check_size(50 * G).is_err());
        assert!(i.check_size(40 * G).is_err());
        assert!(i.check_size(50 * G + MIB).is_err());
        assert!(i.check_size(600 * G).is_err());
        assert_eq!(i.check_size(200 * G).unwrap(), 200 * G);
    }

    #[test]
    fn unknown_guid_is_not_found() {
        let d = disk(500 * G, vec![part(1, MIB, 50 * G, None, LB)]);
        assert!(GrowInfo::build(&d, "{cccccccc-0000-0000-0000-000000000003}", None).is_err());
    }

    #[test]
    fn grow_scripts_use_guid_and_size() {
        let s = grow_script(LB, 123456789);
        assert!(
            s.contains("$_.Guid -eq '{aaaaaaaa-0000-0000-0000-000000000001}'"),
            "{s}"
        );
        assert!(s.contains("Resize-Partition"), "{s}");
        assert!(s.contains("-Size 123456789"), "{s}");
        assert!(s.contains("SizeMax -lt 123456789"), "{s}");
        let q = supported_size_script("AAAAAAAA-0000-0000-0000-000000000001");
        assert!(q.contains("Get-PartitionSupportedSize"), "{q}");
        assert!(
            q.contains("'{aaaaaaaa-0000-0000-0000-000000000001}'"),
            "{q}"
        );
        assert!(!q.contains("Resize-Partition"), "{q}");
    }

    #[test]
    fn supported_output_parsed() {
        let (a, b) = parse_supported("{\"Size\":100,\"SizeMin\":10,\"SizeMax\":300}\n").unwrap();
        assert_eq!((a, b), (100, 300));
        assert!(parse_supported("nope").is_err());
    }

    #[test]
    fn remove_picks_host_in_front() {
        let d = disk(
            500 * G,
            vec![
                part(1, MIB, 100 * G, Some('D'), HOST),
                part(2, 100 * G + MIB, 50 * G, None, LB),
            ],
        );
        let p = RemovePlan::build(&d, LB).unwrap();
        assert_eq!(p.host_guid.as_deref(), Some(HOST));
        assert_eq!(p.host_letter, Some('D'));
        assert_eq!(p.disk_number, 2);
    }

    #[test]
    fn remove_has_no_host_on_external_or_after_a_gap() {
        let ext = disk(500 * G, vec![part(1, MIB, 50 * G, None, LB)]);
        assert_eq!(RemovePlan::build(&ext, LB).unwrap().host_guid, None);
        let far = disk(
            500 * G,
            vec![
                part(1, MIB, 100 * G, Some('D'), HOST),
                part(2, 200 * G, 50 * G, None, LB),
            ],
        );
        assert_eq!(RemovePlan::build(&far, LB).unwrap().host_guid, None);
    }

    #[test]
    fn remove_never_extends_a_host_on_a_usb_disk() {
        let mut d = disk(
            500 * G,
            vec![
                part(1, MIB, 100 * G, Some('D'), HOST),
                part(2, 100 * G + MIB, 50 * G, None, LB),
            ],
        );
        assert!(RemovePlan::build(&d, LB).unwrap().host_guid.is_some());
        for bus in ["USB", "usb", "SD", "MMC", "1394"] {
            d.bus_type = bus.into();
            let p = RemovePlan::build(&d, LB).unwrap();
            assert_eq!(p.host_guid, None, "{bus}");
            assert_eq!(p.host_letter, None, "{bus}");
        }
        for bus in ["SATA", "NVMe", "RAID"] {
            d.bus_type = bus.into();
            assert!(
                RemovePlan::build(&d, LB).unwrap().host_guid.is_some(),
                "{bus}"
            );
        }
    }

    #[test]
    fn remove_script_is_by_guid_and_type_checked() {
        let s = remove_script(LB);
        assert!(
            s.contains("$_.Guid -eq '{aaaaaaaa-0000-0000-0000-000000000001}'"),
            "{s}"
        );
        assert!(s.contains("throw 'partition not found'"), "{s}");
        assert!(
            s.contains("-ieq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'"),
            "{s}"
        );
        assert!(
            s.contains("Remove-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber -Confirm:$false"),
            "{s}"
        );
    }

    #[test]
    fn extend_host_script_only_grows() {
        let s = extend_host_script(HOST);
        assert!(
            s.contains("'{bbbbbbbb-0000-0000-0000-000000000002}'"),
            "{s}"
        );
        assert!(s.contains("SizeMax -gt"), "{s}");
        assert!(s.contains("-Size $s.SizeMax"), "{s}");
    }
}

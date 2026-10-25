//! Disk operations that run PowerShell Storage cmdlets. Only ever call these on a disk the
//! guard accepted; every function here re-reads the disk and checks the guard itself.

use crate::guard::{check, Op};
use crate::ps::{self, quote};
use crate::{Error, Result};
use lifeboat_core::disk::{
    lifeboat_attributes, same_guid, DiskInfo, PartitionInfo, BASIC_DATA_TYPE_GUID, MARKER_FILE,
};
use serde::Deserialize;

const LIST_SCRIPT: &str = "$r = @(Get-Disk | ForEach-Object { $d = $_; \
[pscustomobject]@{ Number=[int]$d.Number; Model=[string]$d.FriendlyName; Size=[uint64]$d.Size; \
BusType=[string]$d.BusType; IsBoot=[bool]$d.IsBoot; IsSystem=[bool]$d.IsSystem; \
Partitions=@(Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue | ForEach-Object { \
[pscustomobject]@{ Number=[int]$_.PartitionNumber; Offset=[uint64]$_.Offset; Size=[uint64]$_.Size; \
GptType=[string]$_.GptType; DriveLetter=[string]$_.DriveLetter; \
IsHidden=[bool]$_.IsHidden; NoDefaultDriveLetter=[bool]$_.NoDefaultDriveLetter; \
Guid=[string]$_.Guid } }) } }); \
ConvertTo-Json -Depth 4 -InputObject $r";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawDisk {
    number: u32,
    #[serde(default)]
    model: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    bus_type: String,
    #[serde(default)]
    is_boot: bool,
    #[serde(default)]
    is_system: bool,
    #[serde(default)]
    partitions: Vec<RawPart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawPart {
    number: u32,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    gpt_type: String,
    #[serde(default)]
    drive_letter: String,
    #[serde(default)]
    is_hidden: bool,
    #[serde(default)]
    no_default_drive_letter: bool,
    #[serde(default)]
    guid: String,
}

/// Parse the JSON printed by [`LIST_SCRIPT`] (an array, or a lone object on old PowerShell).
pub fn parse_disks(json: &str) -> Result<Vec<DiskInfo>> {
    let json = json.trim();
    let raws: Vec<RawDisk> = if json.is_empty() {
        vec![]
    } else if json.starts_with('[') {
        serde_json::from_str(json).map_err(|e| Error::Parse(e.to_string()))?
    } else {
        vec![serde_json::from_str(json).map_err(|e| Error::Parse(e.to_string()))?]
    };
    Ok(raws
        .into_iter()
        .map(|d| DiskInfo {
            number: d.number,
            model: d.model,
            size: d.size,
            bus_type: d.bus_type,
            is_boot: d.is_boot,
            is_system: d.is_system,
            partitions: d
                .partitions
                .into_iter()
                .map(|p| PartitionInfo {
                    number: p.number,
                    offset: p.offset,
                    size: p.size,
                    gpt_type: p.gpt_type,
                    drive_letter: p.drive_letter.chars().find(|c| c.is_ascii_alphabetic()),
                    is_hidden: p.is_hidden,
                    no_default_letter: p.no_default_drive_letter,
                    guid: p.guid,
                })
                .collect(),
        })
        .collect())
}

/// All disks with their partitions. Read-only.
pub fn list_disks() -> Result<Vec<DiskInfo>> {
    parse_disks(&ps::run(LIST_SCRIPT)?)
}

pub(crate) fn disk_by_number(n: u32) -> Result<DiskInfo> {
    list_disks()?
        .into_iter()
        .find(|d| d.number == n)
        .ok_or_else(|| Error::NotFound(format!("disk {n}")))
}

/// A freshly created Lifeboat partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedVolume {
    /// Volume GUID path (`\\?\Volume{...}\`).
    pub volume_path: String,
    /// Unique partition GUID, to be saved in the config.
    pub partition_guid: String,
}

/// PowerShell that creates the hidden partition (`size` bytes, or the largest free extent) with
/// the basic data type (Windows will not mount a custom type), formats it NTFS, writes the marker
/// file `lifeboat.id` (holding the partition GUID) and hides it. `-IsHidden` and
/// `-NoDefaultDriveLetter` set GPT attribute bits 62 and 63 (0xC000000000000000); the script
/// checks they stuck. The last line it prints is the partition GUID. Pure text builder.
pub fn create_partition_script(disk: u32, size: Option<u64>) -> String {
    // Windows starts a partition on a 1 MiB boundary, so when the free space does not start on
    // one (a volume that ended off-boundary was shrunk) up to 1 MiB of it is lost. Take what is
    // there, as long as it is no more than SIZE_SLACK short of the request.
    let create = match size {
        Some(s) => format!(
            "$f = [uint64](Get-Disk -Number {disk}).LargestFreeExtent; $f = $f - ($f % 1MB); \
             $sz = [Math]::Min([uint64]{s}, $f); \
             if ($sz + {SIZE_SLACK} -lt [uint64]{s}) {{ throw \"only $f bytes free on disk {disk}, need {s}\" }}; \
             $p = New-Partition -DiskNumber {disk} -Size $sz"
        ),
        None => format!("$p = New-Partition -DiskNumber {disk} -UseMaximumSize"),
    };
    format!(
        "{create} -GptType {guid}; \
         $p | Format-Volume -FileSystem NTFS -NewFileSystemLabel 'Lifeboat' -Confirm:$false | Out-Null; \
         $p = Get-Partition -DiskNumber {disk} -PartitionNumber $p.PartitionNumber; \
         $g = [string]$p.Guid; \
         $v = $p.AccessPaths | Where-Object {{ $_ -like '\\\\?\\Volume{{*' }} | Select-Object -First 1; \
         [System.IO.File]::WriteAllText($v + {marker}, $g); \
         Set-Partition -DiskNumber {disk} -PartitionNumber $p.PartitionNumber \
         -IsHidden $true -NoDefaultDriveLetter $true; \
         if (-not (Get-Partition -DiskNumber {disk} -PartitionNumber $p.PartitionNumber).IsHidden) \
         {{ throw 'partition did not become hidden (attributes 0x{attrs:X})' }}; \
         Write-Output $g",
        guid = quote(BASIC_DATA_TYPE_GUID),
        marker = quote(MARKER_FILE),
        attrs = lifeboat_attributes(),
    )
}

/// Take the partition GUID from the last non-empty line of [`create_partition_script`]'s output.
pub fn parse_created_guid(out: &str) -> Result<String> {
    match out.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(l) if is_guid(l) => Ok(l.to_string()),
        _ => Err(Error::Parse(format!(
            "no partition GUID in output: {out:?}"
        ))),
    }
}

fn is_guid(s: &str) -> bool {
    let t = s.trim_matches(|c| c == '{' || c == '}');
    let parts: Vec<&str> = t.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Run the creation script and resolve the new partition's volume.
/// How far below the requested size a new partition may come out (alignment loss).
pub const SIZE_SLACK: u64 = 16 * 1024 * 1024;

/// Largest unallocated stretch on `disk`, in bytes. Read-only.
pub(crate) fn largest_free(disk: u32) -> Result<u64> {
    ps::run(&format!(
        "[uint64](Get-Disk -Number {disk}).LargestFreeExtent"
    ))?
    .trim()
    .parse()
    .map_err(|e: std::num::ParseIntError| Error::Parse(e.to_string()))
}

pub(crate) fn create_and_find(disk: u32, size: Option<u64>) -> Result<PreparedVolume> {
    let out = ps::run(&create_partition_script(disk, size))?;
    let guid = parse_created_guid(&out)?;
    let volume_path = find_lifeboat_volume(&guid)?
        .ok_or_else(|| Error::NotFound("new Lifeboat volume".into()))?;
    Ok(PreparedVolume {
        volume_path,
        partition_guid: guid,
    })
}

/// Initialise an empty external disk as GPT, create the hidden NTFS partition filling it and
/// return its volume GUID path and partition GUID. Refuses boot/system disks and non-empty disks.
pub fn prepare_external(disk_number: u32) -> Result<PreparedVolume> {
    let disk = disk_by_number(disk_number)?;
    if let Some(v) = adopt(&disk)? {
        return Ok(v);
    }
    check(&disk, Op::PrepareExternal, false)?;
    if disk.partitions.is_empty() {
        // A disk that has been used before may still be GPT/MBR without partitions.
        ps::run(&format!(
            "if ((Get-Disk -Number {n}).PartitionStyle -eq 'RAW') {{ Initialize-Disk -Number {n} -PartitionStyle GPT }} \
             elseif ((Get-Disk -Number {n}).PartitionStyle -ne 'GPT') {{ throw 'disk is MBR; refusing to convert' }}",
            n = disk_number
        ))?;
    }
    create_and_find(disk_number, None)
}

/// A Lifeboat partition left on `disk` by an earlier setup that failed after creating it (its
/// marker file names it): use it instead of making a second one.
pub(crate) fn adopt(disk: &DiskInfo) -> Result<Option<PreparedVolume>> {
    for p in disk.partitions.iter().filter(|p| p.is_lifeboat()) {
        if let Some(volume_path) = find_lifeboat_volume(&p.guid)? {
            return Ok(Some(PreparedVolume {
                volume_path,
                partition_guid: p.guid.clone(),
            }));
        }
    }
    Ok(None)
}

/// PowerShell that makes the partition with unique GUID `partition_guid` reachable and lists its
/// access paths. A hidden partition (GPT bit 62) has no volume path, so the script clears that
/// bit (the no-drive-letter bit stays: no letter appears in Explorer) and waits up to 10 s for
/// Windows to mount the volume. [`hide_lifeboat_volume`] sets the bit again.
pub fn find_script(partition_guid: &str) -> String {
    format!(
        "$p = Get-Partition | Where-Object {{ $_.Guid -eq {} }} | Select-Object -First 1; \
         if ($p) {{ if ($p.IsHidden) {{ $p | Set-Partition -IsHidden $false }}; \
         for ($i = 0; $i -lt 40; $i++) {{ \
         $a = (Get-Partition -DiskNumber $p.DiskNumber -PartitionNumber $p.PartitionNumber).AccessPaths; \
         if ($a -match 'Volume') {{ break }}; \
         if ($i -eq 8) {{ Update-Disk -Number $p.DiskNumber }}; \
         Start-Sleep -Milliseconds 250 }}; $a }}",
        quote(&braced(partition_guid))
    )
}

/// PowerShell that hides the partition with unique GUID `partition_guid` again (bits 62 and 63).
pub fn hide_script(partition_guid: &str) -> String {
    format!(
        "Get-Partition | Where-Object {{ $_.Guid -eq {} }} | Set-Partition -IsHidden $true -NoDefaultDriveLetter $true",
        quote(&braced(partition_guid))
    )
}

/// Hide the Lifeboat partition so no program sees its volume. Call when a backup, restore or
/// setup is done. No-op for an empty GUID or when the partition is not there.
pub fn hide_lifeboat_volume(partition_guid: &str) -> Result<()> {
    if partition_guid.trim().is_empty() {
        return Ok(());
    }
    ps::run(&hide_script(partition_guid)).map(|_| ())
}

fn braced(guid: &str) -> String {
    format!(
        "{{{}}}",
        guid.trim()
            .trim_matches(|c| c == '{' || c == '}')
            .to_ascii_lowercase()
    )
}

/// First `\\?\Volume{...}` path in the script output.
pub fn parse_volume_path(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|l| l.to_ascii_lowercase().starts_with(r"\\?\volume{"))
        .map(str::to_string)
}

/// Does the marker file's content name `partition_guid`?
pub fn marker_matches(contents: &str, partition_guid: &str) -> bool {
    same_guid(contents, partition_guid)
}

/// Find the Lifeboat partition by its unique partition GUID, confirm it with the marker file
/// `lifeboat.id` at the volume root, and return its volume GUID path (`\\?\Volume{...}\`).
/// `None` when there is no such partition, it has no mounted volume (e.g. the disk is offline)
/// or the marker does not match.
pub fn find_lifeboat_volume(partition_guid: &str) -> Result<Option<String>> {
    if partition_guid.trim().is_empty() {
        return Ok(None);
    }
    let out = ps::run(&find_script(partition_guid))?;
    let Some(vol) = parse_volume_path(&out) else {
        return Ok(None);
    };
    let marker = std::fs::read_to_string(format!("{vol}{MARKER_FILE}")).unwrap_or_default();
    Ok(marker_matches(&marker, partition_guid).then_some(vol))
}

/// One PowerShell script that does the whole "open" step: bring `disk` online (only for a
/// disk that is not shared with Windows or another drive), wait for the partition to show up,
/// then unhide it and list its access paths ([`find_script`]). One process instead of three.
pub fn open_volume_script(partition_guid: &str, disk: Option<u32>) -> String {
    let (online, wait) = match disk {
        Some(n) => (
            format!("Set-Disk -Number {n} -IsOffline $false; "),
            format!(
                "$g = {}; for ($j = 0; $j -lt 40; $j++) {{                  if (Get-Partition | Where-Object {{ $_.Guid -eq $g }}) {{ break }};                  Start-Sleep -Milliseconds 250 }}; ",
                quote(&braced(partition_guid))
            ),
        ),
        None => (String::new(), String::new()),
    };
    format!("{online}{wait}{}", find_script(partition_guid))
}

/// One PowerShell script that does the whole "close" step: hide the partition again, and take
/// `disk` offline (also when hiding failed).
pub fn close_volume_script(partition_guid: &str, disk: Option<u32>) -> String {
    match disk {
        Some(n) => format!(
            "try {{ {} }} finally {{ Set-Disk -Number {n} -IsOffline $true }}",
            hide_script(partition_guid)
        ),
        None => hide_script(partition_guid),
    }
}

/// Bring the disk online (when `disk` is given: a disk that holds only Lifeboat) and find,
/// unhide and verify the Lifeboat volume, in a single PowerShell run. The volume path is the
/// `\?\Volume{GUID}\` form: no drive letter, so nothing shows up in Explorer.
pub fn open_volume(partition_guid: &str, disk: Option<&DiskInfo>) -> Result<Option<String>> {
    if partition_guid.trim().is_empty() {
        return Ok(None);
    }
    if let Some(d) = disk {
        check(d, Op::SetOnline, true)?;
    }
    let out = ps::run(&open_volume_script(partition_guid, disk.map(|d| d.number)))?;
    let Some(vol) = parse_volume_path(&out) else {
        return Ok(None);
    };
    let marker = std::fs::read_to_string(format!("{vol}{MARKER_FILE}")).unwrap_or_default();
    Ok(marker_matches(&marker, partition_guid).then_some(vol))
}

/// Undo [`open_volume`] in a single PowerShell run: hide the partition, take `disk` offline.
pub fn close_volume(partition_guid: &str, disk: Option<&DiskInfo>) -> Result<()> {
    if partition_guid.trim().is_empty() {
        return Ok(());
    }
    if let Some(d) = disk {
        check(d, Op::SetOffline, true)?;
    }
    ps::run(&close_volume_script(partition_guid, disk.map(|d| d.number))).map(|_| ())
}

fn set_offline(disk_number: u32, offline: bool) -> Result<()> {
    let disk = disk_by_number(disk_number)?;
    check(
        &disk,
        if offline {
            Op::SetOffline
        } else {
            Op::SetOnline
        },
        true,
    )?;
    ps::run(&format!(
        "Set-Disk -Number {disk_number} -IsOffline ${}",
        if offline { "true" } else { "false" }
    ))?;
    Ok(())
}

/// Take a data disk offline (hides it from every program). Refuses boot/system disks.
pub fn set_disk_offline(disk_number: u32) -> Result<()> {
    set_offline(disk_number, true)
}

/// Bring a data disk back online. Refuses boot/system disks.
pub fn set_disk_online(disk_number: u32) -> Result<()> {
    set_offline(disk_number, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_array_and_single() {
        let arr = r#"[{"Number":1,"Model":"USB Disk","Size":1000,"BusType":"USB","IsBoot":false,"IsSystem":false,
            "Partitions":[{"Number":1,"Offset":1048576,"Size":900,"GptType":"{abc}","DriveLetter":"E"},
                          {"Number":2,"Offset":0,"Size":5,"GptType":"","DriveLetter":"\u0000"}]}]"#;
        let d = parse_disks(arr).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].bus_type, "USB");
        assert_eq!(d[0].partitions[0].drive_letter, Some('E'));
        assert_eq!(d[0].partitions[1].drive_letter, None);
        let single = r#"{"Number":0,"Model":"M","Size":1,"BusType":"NVMe","IsBoot":true,"IsSystem":true,"Partitions":[]}"#;
        assert!(parse_disks(single).unwrap()[0].is_boot);
        assert!(parse_disks("").unwrap().is_empty());
        assert!(parse_disks("garbage").is_err());
    }

    #[test]
    fn create_script_text() {
        let s = create_partition_script(7, None);
        assert!(s.contains(
            "-DiskNumber 7 -UseMaximumSize -GptType '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'"
        ));
        assert!(!s.contains("6c1f3a52"), "old custom type must be gone");
        assert!(s.contains("-IsHidden $true -NoDefaultDriveLetter $true"));
        assert!(s.contains("0xC000000000000000"), "{s}");
        assert!(s.contains("'lifeboat.id'"));
        assert!(s.contains("Format-Volume -FileSystem NTFS"));
        assert!(create_partition_script(7, Some(123)).contains("[Math]::Min([uint64]123, $f)"));
    }

    #[test]
    fn created_guid_parsed() {
        let g = "{3f2a9c10-1b2c-4d3e-8f90-0123456789ab}";
        assert_eq!(parse_created_guid(&format!("noise\r\n{g}\r\n")).unwrap(), g);
        assert!(parse_created_guid("").is_err());
        assert!(parse_created_guid("not a guid").is_err());
    }

    #[test]
    fn find_by_partition_guid() {
        let s = find_script("3F2A9C10-1B2C-4D3E-8F90-0123456789AB");
        assert!(
            s.contains("$_.Guid -eq '{3f2a9c10-1b2c-4d3e-8f90-0123456789ab}'"),
            "{s}"
        );
        assert!(!s.contains("GptType"));
        assert!(s.contains("Set-Partition -IsHidden $false"), "{s}");
        let h = hide_script("3F2A9C10-1B2C-4D3E-8F90-0123456789AB");
        assert!(h.contains("$_.Guid -eq '{3f2a9c10-1b2c-4d3e-8f90-0123456789ab}'"));
        assert!(h.contains("-IsHidden $true -NoDefaultDriveLetter $true"));
        assert!(hide_lifeboat_volume("  ").is_ok());
        let out = "E:\\\r\n\\\\?\\Volume{aaaa}\\\r\n";
        assert_eq!(
            parse_volume_path(out).as_deref(),
            Some(r"\\?\Volume{aaaa}\")
        );
        assert_eq!(parse_volume_path(""), None);
        assert_eq!(find_lifeboat_volume("  ").unwrap(), None);
    }

    #[test]
    fn merged_scripts() {
        let g = "AB12";
        let open = open_volume_script(g, Some(3));
        assert!(open.starts_with("Set-Disk -Number 3 -IsOffline $false;"));
        assert!(open.contains("-IsHidden $false"));
        let open = open_volume_script(g, None);
        assert!(!open.contains("Set-Disk"));
        let close = close_volume_script(g, Some(3));
        assert!(close.contains("-IsHidden $true") && close.contains("-IsOffline $true"));
        assert!(!close_volume_script(g, None).contains("Set-Disk"));
    }

    #[test]
    fn marker_check() {
        assert!(marker_matches("{ABC-1}\n", "abc-1"));
        assert!(!marker_matches("{abc-2}", "{abc-1}"));
        assert!(!marker_matches("", ""));
    }

    #[test]
    fn list_carries_guid_and_hidden() {
        let j = r#"[{"Number":1,"Partitions":[{"Number":1,"GptType":"{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}","IsHidden":true,"Guid":"{g1}","DriveLetter":""}]}]"#;
        let d = parse_disks(j).unwrap();
        assert!(d[0].partitions[0].is_hidden);
        assert_eq!(d[0].partitions[0].guid, "{g1}");
        assert!(d[0].partitions[0].is_lifeboat());
    }
}

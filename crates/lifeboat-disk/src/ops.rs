//! Disk operations that run PowerShell Storage cmdlets. Only ever call these on a disk the
//! guard accepted; every function here re-reads the disk and checks the guard itself.

use crate::guard::{check, Op};
use crate::ps::{self, quote};
use crate::{Error, Result};
use lifeboat_core::disk::{DiskInfo, PartitionInfo, LIFEBOAT_TYPE_GUID};
use serde::Deserialize;

const LIST_SCRIPT: &str = "$r = @(Get-Disk | ForEach-Object { $d = $_; \
[pscustomobject]@{ Number=[int]$d.Number; Model=[string]$d.FriendlyName; Size=[uint64]$d.Size; \
BusType=[string]$d.BusType; IsBoot=[bool]$d.IsBoot; IsSystem=[bool]$d.IsSystem; \
Partitions=@(Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue | ForEach-Object { \
[pscustomobject]@{ Number=[int]$_.PartitionNumber; Offset=[uint64]$_.Offset; Size=[uint64]$_.Size; \
GptType=[string]$_.GptType; DriveLetter=[string]$_.DriveLetter } }) } }); \
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

/// PowerShell that creates the hidden partition (`size` bytes, or the largest free extent),
/// formats it NTFS and hides it. Pure text builder; unit-tested.
pub fn create_partition_script(disk: u32, size: Option<u64>) -> String {
    let extent = match size {
        Some(s) => format!("-Size {s}"),
        None => "-UseMaximumSize".to_string(),
    };
    format!(
        "$p = New-Partition -DiskNumber {disk} {extent} -GptType {guid}; \
         $p | Format-Volume -FileSystem NTFS -NewFileSystemLabel 'Lifeboat' -Confirm:$false | Out-Null; \
         Set-Partition -DiskNumber {disk} -PartitionNumber $p.PartitionNumber \
         -IsHidden $true -NoDefaultDriveLetter $true",
        guid = quote(LIFEBOAT_TYPE_GUID)
    )
}

/// Initialise an empty external disk as GPT, create the hidden NTFS partition filling it and
/// return its volume GUID path. Refuses boot/system disks and non-empty disks.
pub fn prepare_external(disk_number: u32) -> Result<String> {
    let disk = disk_by_number(disk_number)?;
    check(&disk, Op::PrepareExternal, false)?;
    let init = if disk.partitions.is_empty() {
        // A disk that has been used before may still be GPT/MBR without partitions.
        format!(
            "if ((Get-Disk -Number {n}).PartitionStyle -eq 'RAW') {{ Initialize-Disk -Number {n} -PartitionStyle GPT }} \
             elseif ((Get-Disk -Number {n}).PartitionStyle -ne 'GPT') {{ throw 'disk is MBR; refusing to convert' }}; ",
            n = disk_number
        )
    } else {
        String::new()
    };
    ps::run(&format!(
        "{init}{}",
        create_partition_script(disk_number, None)
    ))?;
    find_lifeboat_volume()?.ok_or_else(|| Error::NotFound("new Lifeboat volume".into()))
}

/// Find the Lifeboat partition by type GUID and return its volume GUID path
/// (`\\?\Volume{...}\`), or `None` when there is none (or it has no mounted volume, e.g. the
/// disk is offline).
pub fn find_lifeboat_volume() -> Result<Option<String>> {
    let out = ps::run(&format!(
        "Get-Partition | Where-Object {{ $_.GptType -eq {} }} | ForEach-Object {{ $_.AccessPaths }}",
        quote(LIFEBOAT_TYPE_GUID)
    ))?;
    Ok(out
        .lines()
        .map(str::trim)
        .find(|l| l.to_ascii_lowercase().starts_with(r"\\?\volume{"))
        .map(str::to_string))
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
            "-DiskNumber 7 -UseMaximumSize -GptType '{6c1f3a52-8d4e-4b7a-9e21-5f0b8a7d3c14}'"
        ));
        assert!(s.contains("-IsHidden $true -NoDefaultDriveLetter $true"));
        assert!(s.contains("Format-Volume -FileSystem NTFS"));
        assert!(create_partition_script(7, Some(123)).contains("-Size 123 "));
    }
}

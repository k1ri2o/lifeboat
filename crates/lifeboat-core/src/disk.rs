//! Disk model shared by the service, the disk crate and the UI.

use serde::{Deserialize, Serialize};

/// GPT type GUID "basic data". The Lifeboat partition uses it because Windows does not mount
/// partitions with an unknown type as volumes; the partition is told apart by its unique
/// partition GUID (see [`Config::partition_guid`](crate::config::Config)) and a marker file.
pub const BASIC_DATA_TYPE_GUID: &str = "{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}";

/// Name of the marker file at the volume root; it holds the partition GUID.
pub const MARKER_FILE: &str = "lifeboat.id";

/// GPT partition attribute bit 62: hidden.
pub const GPT_ATTR_HIDDEN: u64 = 1 << 62;
/// GPT partition attribute bit 63: no default drive letter.
pub const GPT_ATTR_NO_DRIVE_LETTER: u64 = 1 << 63;

/// Attributes the Lifeboat partition carries: hidden and no drive letter.
pub const fn lifeboat_attributes() -> u64 {
    GPT_ATTR_HIDDEN | GPT_ATTR_NO_DRIVE_LETTER
}

/// True when both the hidden and the no-drive-letter bits are set.
pub const fn is_hidden_attrs(attrs: u64) -> bool {
    attrs & lifeboat_attributes() == lifeboat_attributes()
}

fn norm_guid(s: &str) -> String {
    s.trim()
        .trim_matches(|c| c == '{' || c == '}')
        .to_ascii_lowercase()
}

/// Is this the basic data GPT type (any case, with or without braces)?
pub fn is_basic_data_type(guid: &str) -> bool {
    norm_guid(guid) == norm_guid(BASIC_DATA_TYPE_GUID)
}

/// Do two GUID strings name the same GUID (case and braces ignored, empty never matches)?
pub fn same_guid(a: &str, b: &str) -> bool {
    let a = norm_guid(a);
    !a.is_empty() && a == norm_guid(b)
}

/// One partition of a disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    /// GPT type GUID, empty on MBR disks.
    pub gpt_type: String,
    pub drive_letter: Option<char>,
    /// Hidden flag (GPT attribute bit 62).
    #[serde(default)]
    pub is_hidden: bool,
    /// Unique partition GUID, empty when unknown.
    #[serde(default)]
    pub guid: String,
}

impl PartitionInfo {
    /// Looks like a Lifeboat partition: hidden basic-data partition without a drive letter.
    /// This is only a heuristic for the guard ("do not wipe this disk"); the real identity is
    /// [`PartitionInfo::has_guid`].
    pub fn is_lifeboat(&self) -> bool {
        is_basic_data_type(&self.gpt_type) && self.is_hidden && self.drive_letter.is_none()
    }

    /// Is this the partition with the saved unique GUID?
    pub fn has_guid(&self, guid: &str) -> bool {
        same_guid(&self.guid, guid)
    }
}

/// One physical (or virtual) disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskInfo {
    pub number: u32,
    pub model: String,
    pub size: u64,
    /// Bus type as Windows names it: "USB", "SATA", "NVMe", "File Backed Virtual", ...
    pub bus_type: String,
    pub is_boot: bool,
    pub is_system: bool,
    pub partitions: Vec<PartitionInfo>,
}

impl DiskInfo {
    pub fn has_lifeboat_partition(&self) -> bool {
        self.partitions.iter().any(PartitionInfo::is_lifeboat)
    }

    /// Does anything besides Lifeboat live on this disk (Windows, or a drive such as D:)?
    /// A shared disk must never be taken offline, or that drive would vanish.
    pub fn is_shared(&self) -> bool {
        self.is_boot
            || self.is_system
            || self.partitions.iter().any(|p| {
                !p.is_lifeboat() && (p.drive_letter.is_some() || is_basic_data_type(&p.gpt_type))
            })
    }
}

/// How the UI asks the service to prepare a disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum PrepareMode {
    /// Use an empty external disk (GPT, one hidden partition filling it).
    External,
    /// Shrink the volume `drive_letter` on `disk` by `size_bytes` and create the partition there.
    InternalShrink { drive_letter: char, size_bytes: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_bits() {
        assert_eq!(GPT_ATTR_HIDDEN, 0x4000_0000_0000_0000);
        assert_eq!(GPT_ATTR_NO_DRIVE_LETTER, 0x8000_0000_0000_0000);
        assert_eq!(lifeboat_attributes(), 0xC000_0000_0000_0000);
        assert!(is_hidden_attrs(lifeboat_attributes()));
        assert!(!is_hidden_attrs(GPT_ATTR_HIDDEN));
        assert!(!is_hidden_attrs(0));
    }

    #[test]
    fn type_guid_match() {
        assert!(is_basic_data_type("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"));
        assert!(is_basic_data_type(BASIC_DATA_TYPE_GUID));
        assert!(!is_basic_data_type(
            "{6c1f3a52-8d4e-4b7a-9e21-5f0b8a7d3c14}"
        ));
        assert!(!is_basic_data_type(""));
    }

    fn part(hidden: bool, letter: Option<char>, guid: &str) -> PartitionInfo {
        PartitionInfo {
            number: 1,
            offset: 0,
            size: 1,
            gpt_type: BASIC_DATA_TYPE_GUID.into(),
            drive_letter: letter,
            is_hidden: hidden,
            guid: guid.into(),
        }
    }

    #[test]
    fn identify_by_partition_guid() {
        let p = part(true, None, "{AAAA0000-0000-0000-0000-000000000001}");
        assert!(p.is_lifeboat());
        assert!(p.has_guid("aaaa0000-0000-0000-0000-000000000001"));
        assert!(!p.has_guid("{aaaa0000-0000-0000-0000-000000000002}"));
        assert!(!p.has_guid(""));
        assert!(!part(false, None, "x").is_lifeboat());
        assert!(!part(true, Some('E'), "x").is_lifeboat());
        assert!(!part(false, None, "").has_guid(""));
    }

    #[test]
    fn shared_disk() {
        let mut d = DiskInfo {
            number: 1,
            model: String::new(),
            size: 0,
            bus_type: "SATA".into(),
            is_boot: false,
            is_system: false,
            partitions: vec![part(true, None, "{L}")],
        };
        assert!(!d.is_shared(), "only Lifeboat on it");
        d.partitions.push(part(false, Some('D'), "{D}"));
        assert!(d.is_shared(), "D: lives here");
        d.partitions.pop();
        d.partitions.push(part(false, None, "{X}"));
        assert!(d.is_shared(), "visible data partition without a letter");
        d.partitions.pop();
        d.is_boot = true;
        assert!(d.is_shared());
    }

    #[test]
    fn mode_wire() {
        let s = serde_json::to_string(&PrepareMode::External).unwrap();
        assert_eq!(s, r#"{"mode":"External"}"#);
    }
}

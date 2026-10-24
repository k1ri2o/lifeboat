//! Disk model shared by the service, the disk crate and the UI.

use serde::{Deserialize, Serialize};

/// GPT partition type GUID of the hidden Lifeboat partition (randomly generated, ours alone).
pub const LIFEBOAT_TYPE_GUID: &str = "{6c1f3a52-8d4e-4b7a-9e21-5f0b8a7d3c14}";

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

/// Does this GPT type GUID (any case, with or without braces) belong to Lifeboat?
pub fn is_lifeboat_type(guid: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_matches(|c| c == '{' || c == '}')
            .to_ascii_lowercase()
    };
    norm(guid) == norm(LIFEBOAT_TYPE_GUID)
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
}

impl PartitionInfo {
    pub fn is_lifeboat(&self) -> bool {
        is_lifeboat_type(&self.gpt_type)
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
        assert!(is_lifeboat_type("6C1F3A52-8D4E-4B7A-9E21-5F0B8A7D3C14"));
        assert!(is_lifeboat_type(LIFEBOAT_TYPE_GUID));
        assert!(!is_lifeboat_type("{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}"));
        assert!(!is_lifeboat_type(""));
    }

    #[test]
    fn mode_wire() {
        let s = serde_json::to_string(&PrepareMode::External).unwrap();
        assert_eq!(s, r#"{"mode":"External"}"#);
    }
}

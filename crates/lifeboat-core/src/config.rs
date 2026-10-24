//! Configuration types.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// User configuration stored by the service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Folders that are backed up.
    pub folders: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let c = Config {
            folders: vec![PathBuf::from("Documents")],
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&s).unwrap(), c);
    }
}

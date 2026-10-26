//! Hidden-partition management for Lifeboat (plan section 3).
//!
//! Disk work is done by running the Windows Storage cmdlets in a PowerShell subprocess and
//! parsing their JSON output. Everything that decides *what* to do (guard, plan, command text,
//! ACL arguments) is a pure function so it is unit-tested without touching any disk. The
//! functions that run commands refuse boot/system disks (see [`guard`]); the VHDX integration
//! test in `tests/vhdx.rs` is the only place that exercises them, on a virtual disk.

pub mod acl;
pub mod guard;
pub mod ops;
pub mod plan;
mod ps;
pub mod resize;

pub use acl::{lock_down, lock_down_args};
pub use guard::{check, GuardError, Op};
pub use lifeboat_core::disk::{
    DiskInfo, PartitionInfo, PrepareMode, BASIC_DATA_TYPE_GUID, MARKER_FILE,
};
pub use ops::{
    close_volume, find_lifeboat_volume, hide_lifeboat_volume, list_disks, open_volume,
    prepare_external, set_disk_offline, set_disk_online, PreparedVolume,
};
pub use plan::{execute_shrink, plan_shrink, ShrinkPlan};
pub use resize::{grow_info, grow_lifeboat, remove_lifeboat, GrowInfo, RemoveOutcome, RemovePlan};

/// Errors from disk operations.
#[derive(Debug)]
pub enum Error {
    Guard(GuardError),
    /// The PowerShell command failed; carries its stderr.
    Command(String),
    /// Output could not be parsed.
    Parse(String),
    /// The request itself can't be done (too big, bad letter); the text is for the user.
    Invalid(String),
    NotFound(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Guard(e) => write!(f, "refused: {e}"),
            Error::Command(e) => write!(f, "command failed: {e}"),
            Error::Parse(e) => write!(f, "bad command output: {e}"),
            Error::Invalid(e) => write!(f, "{e}"),
            Error::NotFound(e) => write!(f, "not found: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<GuardError> for Error {
    fn from(e: GuardError) -> Self {
        Error::Guard(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

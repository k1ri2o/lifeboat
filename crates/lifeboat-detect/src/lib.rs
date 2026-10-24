//! Ransomware detector: pure library, no service wiring.
//!
//! Building blocks: [`entropy`], [`magic`], [`ransom`] (extensions and note names),
//! [`canary`] bait files, [`baseline`] of normal change rate, [`assess`] to combine
//! them into a [`Verdict`], and [`freeze`] to persist the frozen state.

pub mod assess;
pub mod baseline;
pub mod canary;
pub mod entropy;
pub mod freeze;
pub mod magic;
pub mod ransom;

pub use assess::{assess, ChangeKind, ChangeSet, FileChange, Verdict};
pub use baseline::Baseline;
pub use canary::{check_canaries, create_canaries, CanaryHit, CanaryManifest};
pub use entropy::{entropy, entropy_of_file};
pub use freeze::FreezeState;
pub use magic::mismatches;

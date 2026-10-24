//! Per-folder normal change rate.

use serde::{Deserialize, Serialize};

/// Weight of the newest run in the moving average.
pub const EWMA_ALPHA: f64 = 0.2;

/// Runs needed before the baseline is trusted.
pub const MIN_TRAINED_RUNS: u32 = 3;

/// What "normal" looks like for one watched folder. Keep one per folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// Exponentially weighted average of existing files changed per backup run.
    pub ewma_changed: f64,
    /// Number of runs folded in so far.
    pub runs: u32,
    /// File count at the last run.
    pub file_count: usize,
}

impl Baseline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one finished (non-attack) run into the baseline.
    pub fn update(&mut self, changed: usize, file_count: usize) {
        self.ewma_changed = if self.runs == 0 {
            changed as f64
        } else {
            EWMA_ALPHA * changed as f64 + (1.0 - EWMA_ALPHA) * self.ewma_changed
        };
        self.runs = self.runs.saturating_add(1);
        self.file_count = file_count;
    }

    /// True once there are enough runs to judge a change count against it.
    pub fn is_trained(&self) -> bool {
        self.runs >= MIN_TRAINED_RUNS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewma_moves_slowly() {
        let mut b = Baseline::new();
        b.update(10, 100);
        assert_eq!(b.ewma_changed, 10.0);
        b.update(20, 100);
        assert!((b.ewma_changed - 12.0).abs() < 1e-9);
        assert!(!b.is_trained());
        b.update(10, 120);
        assert!(b.is_trained());
        assert_eq!(b.file_count, 120);
    }
}

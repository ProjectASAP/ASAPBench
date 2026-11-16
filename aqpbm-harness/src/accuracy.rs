//! Accuracy curves: the nearest-rank `(1 - δ)` quantile of per-value errors,
//! and per key the worst of it over merge counts.
//!
//! Example: 1,000 trials of a KLL sketch with errors 0.0015, 0.0018 and
//! 0.0017 at δ = 0.01 after 1, 6 and 360 merges record 0.0018 at δ = 0.01.

use serde::{Deserialize, Serialize};

/// The nearest-rank `(1 - failure_probability)` quantile of `errors`: the
/// `ceil((1 - δ) n)`-th smallest, with the rank computed exactly.
pub fn error_quantile(errors: &[f64], failure_probability: f64) -> Result<f64, AccuracyError> {
    let _ = (errors, failure_probability);
    todo!("implemented after test approval")
}

/// The error quantiles of one key after `merge_count` merges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeCountErrors {
    /// Opaque to the harness, such as `{"sketch": "kll", "k": 547, "metric": "rank"}`.
    pub key: serde_json::Value,
    pub merge_count: u32,
    pub trials: u32,
    /// `(δ, error quantile)` pairs.
    pub error_at: Vec<(f64, f64)>,
}

/// Per key, the worst error over merge counts at each δ.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracyCurve {
    pub key: serde_json::Value,
    pub trials: u32,
    /// `(δ, error)` pairs, δ ascending.
    pub error_at: Vec<(f64, f64)>,
}

/// Per key, the worst error over merge counts at each δ >= 3 / trials; a
/// smaller δ has too few failures to measure and is dropped.
pub fn worst_over_merge_counts(results: &[MergeCountErrors]) -> Vec<AccuracyCurve> {
    let _ = results;
    todo!("implemented after test approval")
}

/// Errors that cannot give a quantile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccuracyError {
    #[error("no errors to take a quantile of")]
    NoErrors,
    #[error("error {index} is NaN")]
    NotANumber { index: usize },
    #[error("failure probability must be in (0, 1)")]
    FailureProbabilityOutOfRange,
}

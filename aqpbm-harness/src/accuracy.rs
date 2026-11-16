//! Accuracy curves: the nearest-rank `(1 - δ)` quantile of per-value errors,
//! and per key the worst of it over merge counts.
//!
//! Example: 1,000 trials of a KLL sketch with errors 0.0015, 0.0018 and
//! 0.0017 at δ = 0.01 after 1, 6 and 360 merges record 0.0018 at δ = 0.01.

use serde::{Deserialize, Serialize};

/// The nearest-rank `(1 - failure_probability)` quantile of `errors`: the
/// `ceil((1 - δ) n)`-th smallest, with the rank computed exactly.
///
/// δ is a decimal such as 0.57, which f64 holds only approximately, so the
/// rank is `n - floor(δ n)` with `δ n` snapped to the nearest integer when
/// it is within 1e-9 of it. Example: 100 errors at δ = 0.57 take the 43rd
/// smallest, where `ceil((1 - 0.57) * 100)` in f64 gives 44.
pub fn error_quantile(errors: &[f64], failure_probability: f64) -> Result<f64, AccuracyError> {
    if errors.is_empty() {
        return Err(AccuracyError::NoErrors);
    }
    if let Some(index) = errors.iter().position(|error| error.is_nan()) {
        return Err(AccuracyError::NotANumber { index });
    }
    if !(failure_probability > 0.0 && failure_probability < 1.0) {
        return Err(AccuracyError::FailureProbabilityOutOfRange);
    }
    let n = errors.len();
    let tolerated = failure_probability * n as f64;
    let nearest = tolerated.round();
    let tolerated = if (tolerated - nearest).abs() <= 1e-9 * nearest.max(1.0) {
        nearest
    } else {
        tolerated.floor()
    };
    let rank = (n - tolerated as usize).max(1);
    let mut sorted = errors.to_vec();
    sorted.sort_by(f64::total_cmp);
    Ok(sorted[rank - 1])
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
/// Curves come in first-appearance order of their keys; a key measured with
/// different trial counts takes the smallest.
pub fn worst_over_merge_counts(results: &[MergeCountErrors]) -> Vec<AccuracyCurve> {
    let mut curves: Vec<AccuracyCurve> = Vec::new();
    for result in results {
        let index = match curves.iter().position(|curve| curve.key == result.key) {
            Some(index) => index,
            None => {
                curves.push(AccuracyCurve {
                    key: result.key.clone(),
                    trials: result.trials,
                    error_at: Vec::new(),
                });
                curves.len() - 1
            }
        };
        let curve = &mut curves[index];
        curve.trials = curve.trials.min(result.trials);
        for &(delta, error) in &result.error_at {
            match curve.error_at.iter_mut().find(|(d, _)| *d == delta) {
                Some((_, worst)) => *worst = worst.max(error),
                None => curve.error_at.push((delta, error)),
            }
        }
    }
    for curve in &mut curves {
        let floor = 3.0 / f64::from(curve.trials);
        curve.error_at.retain(|&(delta, _)| delta >= floor);
        curve.error_at.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    curves
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

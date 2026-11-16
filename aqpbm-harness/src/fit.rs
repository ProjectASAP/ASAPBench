//! Model fitting: relative-weighted least squares over named constants, and
//! leave-one-setting-out validation on top of it.
//!
//! Example: an operator's cost `cpu_ns = units * ns_per_unit +
//! ns_per_invocation` is the two-constant case, with terms `[units, 1]` per
//! point; the points (1e3, 60,000), (1e4, 510,000), (1e5, 5,010,000) fit
//! 50 ns per unit and 10,000 ns per invocation.

use serde::{Deserialize, Serialize};

use crate::protocol::{CpuClock, Protocol};

/// One measurement: the units charged per constant (`terms`, in the order of
/// the constant names) and the measured value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FitPoint {
    /// The setting the point was measured in; leave-one-setting-out holds out
    /// every point of one setting together.
    pub setting: String,
    pub terms: Vec<f64>,
    pub measured: f64,
}

/// Fitted constants, by name, and what the fit flagged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConstantsFit {
    pub constants: Vec<(String, f64)>,
    pub flags: Vec<FitFlag>,
}

/// Something about a fit a reader must know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FitFlag {
    /// The unconstrained fit gave this constant a negative value; it is set
    /// to 0 and the others refitted without it.
    NegativeConstantClamped { constant: String, raw_value: f64 },
    /// A point misses the fit by more than 10% of its measured value.
    ResidualAboveTolerance {
        setting: String,
        relative_residual: f64,
    },
    /// No point has a non-zero term for this constant, so it is not fitted.
    ConstantNotIdentified { constant: String },
}

/// Relative-weighted least squares (weights `1 / measured^2`) of `measured`
/// on `terms`, one constant per name. A negative constant is set to 0 and the
/// rest refitted without it.
pub fn fit_constants(names: &[&str], points: &[FitPoint]) -> Result<ConstantsFit, FitError> {
    let _ = (names, points);
    todo!("implemented after test approval")
}

/// One fold: the fit without one setting, and its relative error on that
/// setting's points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldOutSetting {
    pub setting: String,
    pub fit: ConstantsFit,
    /// `(predicted - measured) / measured` summed over the held-out points;
    /// `None` when a constant the setting needs is not identified without it.
    pub relative_error: Option<f64>,
}

/// Leave-one-setting-out over every setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaveOneSettingOut {
    /// One fold per setting, in first-appearance order.
    pub held_out: Vec<HeldOutSetting>,
    /// `sqrt(mean(ln(predicted / measured)^2))` over the scored folds.
    pub rms_log_error: f64,
    /// Folds with a `relative_error`.
    pub settings_scored: usize,
}

/// Fits without each setting in turn and scores the fit on it.
pub fn leave_one_setting_out(
    names: &[&str],
    points: &[FitPoint],
) -> Result<LeaveOneSettingOut, FitError> {
    let _ = (names, points);
    todo!("implemented after test approval")
}

/// Times `run(size)` at each size on `clock` under `protocol` (warm-up, then
/// the median of `measured_runs` thread-CPU times). `run` returns the units
/// it processed. One point per size: setting = the size, terms =
/// `[units, 1]`.
pub fn measure_sizes(
    protocol: &Protocol,
    clock: &dyn CpuClock,
    sizes: &[u64],
    run: &mut dyn FnMut(u64) -> u64,
) -> Vec<FitPoint> {
    let _ = (protocol, clock, sizes, run);
    todo!("implemented after test approval")
}

/// A fit that cannot be computed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FitError {
    /// No points.
    #[error("no points to fit")]
    NoPoints,
    /// A point's term count differs from the constant count.
    #[error("setting {setting}: {got} terms for {expected} constants")]
    TermCountMismatch {
        setting: String,
        expected: usize,
        got: usize,
    },
    /// A measured value of 0, a negative value or NaN: the weight `1 / y^2`
    /// is undefined.
    #[error("setting {setting}: measured {measured} cannot be weighted by 1 / measured^2")]
    BadMeasurement { setting: String, measured: f64 },
    /// Leave-one-setting-out needs two settings.
    #[error("leave-one-setting-out needs at least 2 settings, got {settings}")]
    TooFewSettings { settings: usize },
}

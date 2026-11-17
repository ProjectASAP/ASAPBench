//! Model fitting: relative-weighted least squares over named constants, and
//! leave-one-setting-out validation on top of it.
//!
//! Example: an operator's cost `cpu_ns = units * ns_per_unit +
//! ns_per_invocation` is the two-constant case, with terms `[units, 1]` per
//! point; the points (1e3, 60,000), (1e4, 510,000), (1e5, 5,010,000) fit
//! 50 ns per unit and 10,000 ns per invocation.

use serde::{Deserialize, Serialize};

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
    /// The points cannot determine this constant, so it is set to 0 and not
    /// fitted: no point has a non-zero term for it, its column is a linear
    /// combination of the other constants' columns (collinear), or there are
    /// fewer points than constants.
    ConstantNotIdentified { constant: String },
}

/// Relative-weighted least squares (weights `1 / measured^2`) of `measured`
/// on `terms`, one constant per name. A negative constant is set to 0 and the
/// rest refitted without it.
///
/// The points are fitted in a canonical order (by setting, then terms, then
/// measured value), so the constants do not depend on the order given. A
/// constant no point exercises (all its terms 0, or collinear with the
/// others) is `ConstantNotIdentified` and set to 0. Every point the fit
/// misses by more than [`RESIDUAL_TOLERANCE`] of its measured value is
/// flagged.
pub fn fit_constants(names: &[&str], points: &[FitPoint]) -> Result<ConstantsFit, FitError> {
    if points.is_empty() {
        return Err(FitError::NoPoints);
    }
    for point in points {
        if point.terms.len() != names.len() {
            return Err(FitError::TermCountMismatch {
                setting: point.setting.clone(),
                expected: names.len(),
                got: point.terms.len(),
            });
        }
        if !(point.measured.is_finite() && point.measured > 0.0) {
            return Err(FitError::BadMeasurement {
                setting: point.setting.clone(),
                measured: point.measured,
            });
        }
    }
    let mut ordered: Vec<&FitPoint> = points.iter().collect();
    ordered.sort_by(|a, b| {
        a.setting
            .cmp(&b.setting)
            .then_with(|| {
                a.terms
                    .iter()
                    .zip(&b.terms)
                    .map(|(x, y)| x.total_cmp(y))
                    .find(|order| order.is_ne())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.measured.total_cmp(&b.measured))
    });

    let mut flags = Vec::new();
    let mut values = vec![0.0; names.len()];
    let mut active: Vec<usize> = (0..names.len()).collect();
    loop {
        match weighted_least_squares(&ordered, &active) {
            Err(Unidentified(constant)) => {
                flags.push(FitFlag::ConstantNotIdentified {
                    constant: names[constant].to_string(),
                });
                active.retain(|&c| c != constant);
            }
            Ok(solution) => {
                let negative: Vec<(usize, f64)> = active
                    .iter()
                    .zip(&solution)
                    .filter(|(_, &value)| value < 0.0)
                    .map(|(&c, &value)| (c, value))
                    .collect();
                if negative.is_empty() {
                    for (&c, value) in active.iter().zip(solution) {
                        values[c] = value;
                    }
                    break;
                }
                for (constant, raw_value) in negative {
                    flags.push(FitFlag::NegativeConstantClamped {
                        constant: names[constant].to_string(),
                        raw_value,
                    });
                    active.retain(|&c| c != constant);
                }
            }
        }
    }
    for point in &ordered {
        let relative_residual = (predict(&values, &point.terms) - point.measured) / point.measured;
        if relative_residual.abs() > RESIDUAL_TOLERANCE {
            flags.push(FitFlag::ResidualAboveTolerance {
                setting: point.setting.clone(),
                relative_residual,
            });
        }
    }
    Ok(ConstantsFit {
        constants: names
            .iter()
            .zip(values)
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
        flags,
    })
}

/// A point the fit misses by more than this share of its measured value is
/// flagged: 0.10 is 10%.
pub const RESIDUAL_TOLERANCE: f64 = 0.10;

/// `terms · constants`.
fn predict(constants: &[f64], terms: &[f64]) -> f64 {
    constants.iter().zip(terms).map(|(c, t)| c * t).sum()
}

/// The constant (by index) the points cannot determine.
struct Unidentified(usize);

/// Minimizes `Σ ((terms · c - measured) / measured)^2` over the `active`
/// constants by Householder QR of the weighted, column-normalized design
/// matrix (better conditioned than the normal equations). Returns the
/// constants in `active` order, or the first active constant whose column is
/// zero or a combination of the earlier ones.
fn weighted_least_squares(
    points: &[&FitPoint],
    active: &[usize],
) -> Result<Vec<f64>, Unidentified> {
    let (rows, columns) = (points.len(), active.len());
    if columns == 0 {
        return Ok(Vec::new());
    }
    // a[i][j] = term / measured, columns scaled to unit norm; b[i] = 1.
    let mut a: Vec<Vec<f64>> = points
        .iter()
        .map(|point| {
            active
                .iter()
                .map(|&c| point.terms[c] / point.measured)
                .collect()
        })
        .collect();
    let mut b = vec![1.0; rows];
    let mut scale = vec![0.0; columns];
    for (j, column_scale) in scale.iter_mut().enumerate() {
        let norm = a.iter().map(|row| row[j] * row[j]).sum::<f64>().sqrt();
        if norm == 0.0 {
            return Err(Unidentified(active[j]));
        }
        *column_scale = norm;
        for row in &mut a {
            row[j] /= norm;
        }
    }
    for j in 0..columns {
        if j >= rows {
            return Err(Unidentified(active[j]));
        }
        // Householder reflection zeroing column j below the diagonal.
        let norm = (j..rows).map(|i| a[i][j] * a[i][j]).sum::<f64>().sqrt();
        if norm < IDENTIFIED_COLUMN_NORM {
            return Err(Unidentified(active[j]));
        }
        let alpha = if a[j][j] > 0.0 { -norm } else { norm };
        let mut v: Vec<f64> = (j..rows).map(|i| a[i][j]).collect();
        v[0] -= alpha;
        let v_norm_sq: f64 = v.iter().map(|x| x * x).sum();
        for k in j..columns {
            let mut column: Vec<f64> = a[j..].iter().map(|row| row[k]).collect();
            reflect(&v, v_norm_sq, &mut column);
            for (row, value) in a[j..].iter_mut().zip(column) {
                row[k] = value;
            }
        }
        reflect(&v, v_norm_sq, &mut b[j..]);
    }
    let mut solution = vec![0.0; columns];
    for j in (0..columns).rev() {
        let rest: f64 = (j + 1..columns).map(|k| a[j][k] * solution[k]).sum();
        solution[j] = (b[j] - rest) / a[j][j];
    }
    Ok(solution
        .into_iter()
        .zip(scale)
        .map(|(value, norm)| value / norm)
        .collect())
}

/// The Householder reflection `x -> x - 2 v (v · x) / (v · v)`.
fn reflect(v: &[f64], v_norm_sq: f64, x: &mut [f64]) {
    let dot: f64 = v.iter().zip(x.iter()).map(|(vi, xi)| vi * xi).sum();
    let factor = 2.0 * dot / v_norm_sq;
    for (xi, vi) in x.iter_mut().zip(v) {
        *xi -= factor * vi;
    }
}

/// Below this norm, a unit-norm column's part outside the earlier columns is
/// rounding noise: the constant is not identified.
const IDENTIFIED_COLUMN_NORM: f64 = 1e-10;

/// One fold: the fit without one setting, and its relative error on each of
/// that setting's points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldOutSetting {
    pub setting: String,
    pub fit: ConstantsFit,
    /// `(predicted - measured) / measured` of each held-out point, in the
    /// order given, signed: errors of opposite sign never cancel. `None` when
    /// a constant the setting needs is not identified without it. Example:
    /// points measured 100 and 200, predicted 110 and 180: `[0.1, -0.1]`.
    pub point_relative_errors: Option<Vec<f64>>,
}

/// Leave-one-setting-out over every setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaveOneSettingOut {
    /// One fold per setting, in first-appearance order.
    pub held_out: Vec<HeldOutSetting>,
    /// `sqrt(mean(ln(predicted / measured)^2))` over the points of the scored
    /// folds; `None` when no fold is scored.
    pub rms_log_error: Option<f64>,
    /// Folds with `point_relative_errors`.
    pub settings_scored: usize,
}

/// Fits without each setting in turn and scores the fit on it.
pub fn leave_one_setting_out(
    names: &[&str],
    points: &[FitPoint],
) -> Result<LeaveOneSettingOut, FitError> {
    let mut settings: Vec<&str> = Vec::new();
    for point in points {
        if !settings.contains(&point.setting.as_str()) {
            settings.push(&point.setting);
        }
    }
    if settings.len() < 2 {
        return Err(FitError::TooFewSettings {
            settings: settings.len(),
        });
    }
    let mut held_out = Vec::with_capacity(settings.len());
    let mut log_errors = Vec::new();
    for setting in settings {
        let (test, train): (Vec<FitPoint>, Vec<FitPoint>) = points
            .iter()
            .cloned()
            .partition(|point| point.setting == setting);
        let fit = fit_constants(names, &train)?;
        let unidentified: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, name)| {
                fit.flags.iter().any(|flag| {
                    matches!(flag, FitFlag::ConstantNotIdentified { constant } if constant == *name)
                })
            })
            .map(|(index, _)| index)
            .collect();
        let needs_unidentified = test
            .iter()
            .any(|point| unidentified.iter().any(|&c| point.terms[c] != 0.0));
        let point_relative_errors = if needs_unidentified {
            None
        } else {
            let constants: Vec<f64> = fit.constants.iter().map(|(_, value)| *value).collect();
            let errors = test
                .iter()
                .map(|point| {
                    let predicted = predict(&constants, &point.terms);
                    log_errors.push((predicted / point.measured).ln());
                    (predicted - point.measured) / point.measured
                })
                .collect();
            Some(errors)
        };
        held_out.push(HeldOutSetting {
            setting: setting.to_string(),
            fit,
            point_relative_errors,
        });
    }
    let rms_log_error = (!log_errors.is_empty())
        .then(|| (log_errors.iter().map(|e| e * e).sum::<f64>() / log_errors.len() as f64).sqrt());
    let settings_scored = held_out
        .iter()
        .filter(|fold| fold.point_relative_errors.is_some())
        .count();
    Ok(LeaveOneSettingOut {
        held_out,
        rms_log_error,
        settings_scored,
    })
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

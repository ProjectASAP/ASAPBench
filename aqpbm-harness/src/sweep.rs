//! Sweep arithmetic over measured arms: regret of the selected arm, and how
//! often the engine's predicted ranking agrees with the measured one.

use crate::protocol::{compare, Comparison};

/// `measured(selected) / measured(best arm)`.
pub fn regret(arms: &[(String, f64)], selected: &str) -> Result<f64, SweepError> {
    let best = arms
        .iter()
        .map(|(_, measured)| *measured)
        .min_by(f64::total_cmp)
        .ok_or(SweepError::NoArms)?;
    let (_, measured) = arms
        .iter()
        .find(|(arm, _)| arm == selected)
        .ok_or_else(|| SweepError::UnknownArm {
            arm: selected.to_string(),
        })?;
    Ok(measured / best)
}

/// The engine's predicted and the harness's measured cost per second of
/// data of the selected arm and the exact arm in one setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectedAndExactCosts {
    pub predicted_selected: f64,
    pub predicted_exact: f64,
    pub measured_selected: f64,
    pub measured_exact: f64,
}

/// The share of rows where predicted and measured order the selected arm
/// against exact the same way; a measured tie (inside `band`) counts as
/// agreement. `NaN` for no rows.
pub fn ranking_agreement(rows: &[SelectedAndExactCosts], band: f64) -> f64 {
    let agreeing = rows
        .iter()
        .filter(|row| {
            let measured = compare(row.measured_selected, row.measured_exact, band);
            measured == Comparison::Tie
                || measured == compare(row.predicted_selected, row.predicted_exact, 0.0)
        })
        .count();
    agreeing as f64 / rows.len() as f64
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SweepError {
    #[error("selected arm '{arm}' is not among the measured arms")]
    UnknownArm { arm: String },
    #[error("no arms measured")]
    NoArms,
}

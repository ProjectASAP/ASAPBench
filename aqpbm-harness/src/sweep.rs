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

/// The share of rows where predicted and measured order the selected arm
/// against exact the same way; a measured tie (inside `band`) counts as
/// agreement. Each row is (predicted selected, predicted exact, measured
/// selected, measured exact). `NaN` for no rows.
pub fn ranking_agreement(rows: &[(f64, f64, f64, f64)], band: f64) -> f64 {
    let agreeing = rows
        .iter()
        .filter(
            |&&(predicted_selected, predicted_exact, measured_selected, measured_exact)| {
                let measured = compare(measured_selected, measured_exact, band);
                measured == Comparison::Tie
                    || measured == compare(predicted_selected, predicted_exact, 0.0)
            },
        )
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

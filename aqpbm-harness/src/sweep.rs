//! Sweep arithmetic over measured arms: regret of the selected arm, and how
//! often the engine's predicted ranking agrees with the measured one.

/// `measured(selected) / measured(best arm)`.
pub fn regret(arms: &[(String, f64)], selected: &str) -> Result<f64, SweepError> {
    let _ = (arms, selected);
    todo!("implemented after test approval")
}

/// The share of rows where predicted and measured order the selected arm
/// against exact the same way; a measured tie (inside `band`) counts as
/// agreement. Each row is (predicted selected, predicted exact, measured
/// selected, measured exact).
pub fn ranking_agreement(rows: &[(f64, f64, f64, f64)], band: f64) -> f64 {
    let _ = (rows, band);
    todo!("implemented after test approval")
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SweepError {
    #[error("selected arm '{arm}' is not among the measured arms")]
    UnknownArm { arm: String },
    #[error("no arms measured")]
    NoArms,
}

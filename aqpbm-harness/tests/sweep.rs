//! Sweep arithmetic: regret and ranking agreement.

use aqpbm_harness::sweep::{ranking_agreement, regret, SelectedAndExactCosts, SweepError};

/// Arms exact 100, planned 120 and noraw 80 with `planned` selected: regret
/// 120 / 80 = 1.5. An unknown selected arm `stored` is an error naming it.
#[test]
fn regret_is_measured_selected_over_measured_best() {
    let arms = [
        ("exact".to_string(), 100.0),
        ("planned".to_string(), 120.0),
        ("noraw".to_string(), 80.0),
    ];
    assert_eq!(regret(&arms, "planned"), Ok(1.5));

    let error = regret(&arms, "stored").unwrap_err();
    assert_eq!(
        error,
        SweepError::UnknownArm {
            arm: "stored".into()
        }
    );
    assert!(error.to_string().contains("stored"), "{error}");
}

/// 10 rows at band 0.11: 8 where predicted and measured agree, 1 where the
/// prediction says cheaper but the selected arm measures 30% costlier, and 1
/// where it measures 104 against exact 100, a tie inside the band that counts
/// as agreement: 9 / 10 = 0.9.
#[test]
fn ranking_agreement_counts_measured_ties_as_agreement() {
    let row = |predicted_selected, predicted_exact, measured_selected, measured_exact| {
        SelectedAndExactCosts {
            predicted_selected,
            predicted_exact,
            measured_selected,
            measured_exact,
        }
    };
    let mut rows = Vec::new();
    for _ in 0..4 {
        rows.push(row(50.0, 100.0, 60.0, 100.0)); // both cheaper
        rows.push(row(150.0, 100.0, 140.0, 100.0)); // both costlier
    }
    rows.push(row(50.0, 100.0, 130.0, 100.0)); // disagrees by 30%
    rows.push(row(50.0, 100.0, 104.0, 100.0)); // a measured tie inside 0.11
    assert_eq!(rows.len(), 10);
    let agreement = ranking_agreement(&rows, 0.11);
    assert!((agreement - 0.9).abs() < 1e-12, "{agreement}");
}

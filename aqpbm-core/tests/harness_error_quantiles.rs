//! Accuracy curves: nearest-rank error quantiles, and per key the worst of
//! them over merge counts.

use aqpbm_core::harness::error_quantiles::{
    error_quantile, worst_over_merge_counts, AccuracyError, MergeCountErrors,
};
use rand::{Rng, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use serde_json::json;

/// Errors i / 1e5 for i = 1..1000, given in descending order: the
/// nearest-rank 0.99 quantile is the ⌈0.99 · 1000⌉ = 990th smallest,
/// 0.0099.
/// Moved from the ASAPPlanner test `quantile_of_errors_nearest_rank`.
#[test]
fn quantile_of_errors_nearest_rank() {
    let errors: Vec<f64> = (1..=1000).rev().map(|i| i as f64 / 1e5).collect();
    assert_eq!(error_quantile(&errors, 0.01), Ok(990.0 / 1e5));
}

/// Errors 1..=100 at δ = 0.57: the nearest rank is ⌈0.43 · 100⌉ = 43, so
/// the quantile is 43. In f64, `(1 - 0.57) * 100` is 43.00000000000001,
/// whose `ceil` would pick 44.
#[test]
fn nearest_rank_is_exact_where_a_float_ceil_is_not() {
    let errors: Vec<f64> = (1..=100).map(f64::from).collect();
    assert_eq!(error_quantile(&errors, 0.57), Ok(43.0));
}

/// Rank errors of KLL k = 547 at δ = 0.01 after 1, 6 and 360 merges:
/// 0.0015, 0.0018, 0.0017, 1,000 trials each. The curve records the worst,
/// 0.0018, not the mean (0.001667) or the last (0.0017), under the opaque
/// key it was given.
/// Moved from the ASAPPlanner test `worst_over_merge_counts_is_recorded`.
#[test]
fn worst_over_merge_counts_is_recorded() {
    let key = json!({"sketch": "kll", "k": 547, "metric": "rank"});
    let after = |merge_count, error| MergeCountErrors {
        key: key.clone(),
        merge_count,
        trials: 1_000,
        error_at: vec![(0.01, error)],
    };
    let curves = worst_over_merge_counts(&[after(1, 0.0015), after(6, 0.0018), after(360, 0.0017)]);
    assert_eq!(curves.len(), 1, "{curves:?}");
    assert_eq!(curves[0].key, key);
    assert_eq!(curves[0].error_at, vec![(0.01, 0.0018)]);
    assert_eq!(curves[0].trials, 1_000);
}

/// With 3,000 trials, δ below 3 / 3,000 = 0.001 has too few failures to
/// measure: δ = 0.0005 is dropped, 0.001 and 0.01 are kept.
#[test]
fn failure_probabilities_below_three_over_trials_are_dropped() {
    let results = [MergeCountErrors {
        key: json!({"sketch": "cms"}),
        merge_count: 1,
        trials: 3_000,
        error_at: vec![(0.0005, 0.05), (0.001, 0.03), (0.01, 0.01)],
    }];
    let curves = worst_over_merge_counts(&results);
    assert_eq!(curves.len(), 1, "{curves:?}");
    assert_eq!(curves[0].error_at, vec![(0.001, 0.03), (0.01, 0.01)]);
}

/// 3 keys × merge counts 1, 6, 36 and 360, each with 3,000 errors drawn from
/// Uniform(0, 0.01) (`Xoshiro256PlusPlus` seed 7) and their quantiles at
/// δ = 0.001 … 0.5. Every curve has δ ascending in (0, 1] and ≥ 3 / trials,
/// errors finite, ≥ 0 and non-increasing in δ, and each error equal to the
/// max over that key's merge counts at that δ.
/// Adapted from the ASAPPlanner test `accuracy_table_is_monotone`.
#[test]
fn accuracy_curve_is_monotone_and_the_worst_of_its_merge_counts() {
    const TRIALS: u32 = 3_000;
    let deltas = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5];
    let keys = [
        json!({"sketch": "kll", "k": 200}),
        json!({"sketch": "kll", "k": 547}),
        json!({"sketch": "cms", "width": 2048, "depth": 5}),
    ];
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(7);
    let mut results = Vec::new();
    for key in &keys {
        for merge_count in [1, 6, 36, 360] {
            let errors: Vec<f64> = (0..TRIALS).map(|_| rng.random_range(0.0..0.01)).collect();
            let error_at = deltas
                .iter()
                .map(|&delta| (delta, error_quantile(&errors, delta).unwrap()))
                .collect();
            results.push(MergeCountErrors {
                key: key.clone(),
                merge_count,
                trials: TRIALS,
                error_at,
            });
        }
    }
    let curves = worst_over_merge_counts(&results);
    assert_eq!(curves.len(), keys.len(), "{curves:?}");
    for key in &keys {
        assert!(curves.iter().any(|curve| &curve.key == key), "{curves:?}");
    }
    for curve in &curves {
        assert_eq!(curve.error_at.len(), deltas.len(), "{curve:?}");
        let floor = 3.0 / curve.trials as f64;
        for pair in curve.error_at.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{curve:?}");
            assert!(pair[1].1 <= pair[0].1, "{curve:?}");
        }
        for &(delta, error) in &curve.error_at {
            assert!(delta >= floor && delta <= 1.0, "{curve:?}");
            assert!(error.is_finite() && error >= 0.0, "{curve:?}");
            let worst = results
                .iter()
                .filter(|raw| raw.key == curve.key)
                .flat_map(|raw| raw.error_at.iter())
                .filter(|raw| raw.0 == delta)
                .map(|raw| raw.1)
                .fold(f64::NEG_INFINITY, f64::max);
            assert_eq!(error, worst, "{curve:?}");
        }
    }
}

/// No errors give `NoErrors`; `[0.1, NaN]` gives `NotANumber { index: 1 }`
/// rather than a quantile that depends on where NaN sorts.
#[test]
fn empty_or_nan_errors_are_refused() {
    assert_eq!(error_quantile(&[], 0.01), Err(AccuracyError::NoErrors));
    assert_eq!(
        error_quantile(&[0.1, f64::NAN], 0.01),
        Err(AccuracyError::NotANumber { index: 1 })
    );
}

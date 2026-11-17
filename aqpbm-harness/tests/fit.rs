//! Model fitting: relative-weighted least squares over named constants,
//! leave-one-setting-out on top of it, and the size sweep that measures the
//! points.

use std::time::{Duration, Instant};

use aqpbm_harness::fake::FakeClock;
use aqpbm_harness::fit::{
    fit_constants, leave_one_setting_out, ConstantsFit, FitError, FitFlag, FitPoint,
    LeaveOneSettingOut,
};
use aqpbm_harness::protocol::{
    measure_sizes, EngineClock, LoadProbe, OsCpuClock, OsLoadProbe, Protocol, ProtocolError,
};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;

/// The two constants of an operator's cost `units * ns_per_unit + ns_per_invocation`.
/// A machine that is always idle.
struct IdleProbe;

impl LoadProbe for IdleProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        0.0
    }
}

const OPERATOR: [&str; 2] = ["ns_per_unit", "ns_per_invocation"];

/// The three constants of the leave-one-setting-out model.
const MODEL: [&str; 3] = ["ns_per_sample", "ns_per_series", "ns_per_call"];

/// One point per `(units, measured)`, terms `[units, 1]`, setting = the units.
fn operator_points(points: &[(f64, f64)]) -> Vec<FitPoint> {
    points
        .iter()
        .map(|&(units, measured)| FitPoint {
            setting: format!("{units}"),
            terms: vec![units, 1.0],
            measured,
        })
        .collect()
}

/// One point per `terms`, settings `s1`, `s2`, ..., measured `measured[i]`.
fn model_points(terms: &[[f64; 3]], measured: &[f64]) -> Vec<FitPoint> {
    terms
        .iter()
        .zip(measured)
        .enumerate()
        .map(|(index, (terms, &measured))| FitPoint {
            setting: format!("s{}", index + 1),
            terms: terms.to_vec(),
            measured,
        })
        .collect()
}

/// The five settings of F8: (samples, series, calls).
const MODEL_TERMS: [[f64; 3]; 5] = [
    [1e3, 10.0, 1.0],
    [1e4, 10.0, 1.0],
    [1e3, 100.0, 1.0],
    [1e5, 100.0, 1.0],
    [1e4, 1000.0, 1.0],
];

/// The exact dot products of [`MODEL_TERMS`] with (2, 150, 10,000).
const MODEL_MEASURED: [f64; 5] = [13_500.0, 31_500.0, 27_000.0, 225_000.0, 180_000.0];

/// [`MODEL_MEASURED`] with setting 4 measured ×1.5.
const OUTLIER_MEASURED: [f64; 5] = [13_500.0, 31_500.0, 27_000.0, 337_500.0, 180_000.0];

/// The fitted value of `name`.
fn constant(fit: &ConstantsFit, name: &str) -> f64 {
    fit.constants
        .iter()
        .find(|(constant, _)| constant == name)
        .map(|&(_, value)| value)
        .unwrap_or_else(|| panic!("no constant {name}: {fit:?}"))
}

fn relative_difference(a: f64, b: f64) -> f64 {
    (a - b).abs() / b.abs()
}

/// Every fold's relative error, in fold order.
fn held_out_errors(loso: &LeaveOneSettingOut) -> Vec<Option<f64>> {
    loso.held_out
        .iter()
        .map(|fold| {
            fold.point_relative_errors.as_ref().map(|errors| {
                assert_eq!(errors.len(), 1, "one point per setting: {fold:?}");
                errors[0]
            })
        })
        .collect()
}

/// The line through (1e3, 60,000), (1e4, 510,000), (1e5, 5,010,000) is
/// 50 ns per unit + 10,000 ns per invocation, within 1e-9, with no flags.
/// Moved from the ASAPPlanner test `fit_recovers_exact_line`.
#[test]
fn fit_recovers_exact_line() {
    let fit = fit_constants(
        &OPERATOR,
        &operator_points(&[(1e3, 60_000.0), (1e4, 510_000.0), (1e5, 5_010_000.0)]),
    )
    .unwrap();
    assert!(
        (constant(&fit, "ns_per_unit") - 50.0).abs() < 1e-9,
        "{fit:?}"
    );
    assert!(
        (constant(&fit, "ns_per_invocation") - 10_000.0).abs() < 1e-9 * 10_000.0,
        "{fit:?}"
    );
    assert!(fit.flags.is_empty(), "{fit:?}");
}

/// The same points × (1.02, 0.98, 1.01). Relative-weighted least squares
/// gives 49.69 / 11,391 (by hand from the normal equations with weights
/// 1/y²); plain least squares gives an intercept of 2,783, which fails the
/// 8,000..=12,500 bound.
/// Moved from the ASAPPlanner test `fit_is_relative_weighted`.
#[test]
fn fit_is_relative_weighted() {
    let fit = fit_constants(
        &OPERATOR,
        &operator_points(&[
            (1e3, 60_000.0 * 1.02),
            (1e4, 510_000.0 * 0.98),
            (1e5, 5_010_000.0 * 1.01),
        ]),
    )
    .unwrap();
    assert!(
        (48.5..=51.5).contains(&constant(&fit, "ns_per_unit")),
        "{fit:?}"
    );
    assert!(
        (8_000.0..=12_500.0).contains(&constant(&fit, "ns_per_invocation")),
        "{fit:?}"
    );
}

/// Five sizes from 10 to 1e5 units, noise +2%, −2%, +2%, −2%, +2%: the
/// weighted fit is 50.12 / 10,045 by hand, within 0.5 / 100 of 50 / 10,000.
/// Moved from the ASAPPlanner test `fit_five_sizes_tightens`.
#[test]
fn fit_five_sizes_tightens() {
    let noise = [1.02, 0.98, 1.02, 0.98, 1.02];
    let points: Vec<(f64, f64)> = [10.0, 100.0, 1e3, 1e4, 1e5]
        .iter()
        .zip(noise)
        .map(|(&units, noise)| (units, (50.0 * units + 10_000.0) * noise))
        .collect();
    let fit = fit_constants(&OPERATOR, &operator_points(&points)).unwrap();
    assert!(
        (constant(&fit, "ns_per_unit") - 50.0).abs() <= 0.5,
        "{fit:?}"
    );
    assert!(
        (constant(&fit, "ns_per_invocation") - 10_000.0).abs() <= 100.0,
        "{fit:?}"
    );
}

/// (100, 900), (1000, 10,900) have intercept −211.111. The fit sets
/// `ns_per_invocation` to 0, refits through the origin (weighted:
/// Σ(x/y) / Σ(x²/y²) = 9.770231720134127) and flags the clamp with the raw
/// value.
/// Moved from the ASAPPlanner test `fit_negative_intercept_is_clamped`.
#[test]
fn fit_negative_intercept_is_clamped() {
    let fit = fit_constants(
        &OPERATOR,
        &operator_points(&[(100.0, 900.0), (1_000.0, 10_900.0)]),
    )
    .unwrap();
    assert_eq!(constant(&fit, "ns_per_invocation"), 0.0, "{fit:?}");
    assert!(
        (constant(&fit, "ns_per_unit") - 9.770_231_720_134_127).abs() < 1e-9,
        "{fit:?}"
    );
    assert!(
        fit.flags.iter().any(|flag| matches!(
            flag,
            FitFlag::NegativeConstantClamped { constant, raw_value }
                if constant == "ns_per_invocation" && (raw_value + 211.111).abs() < 0.01
        )),
        "{fit:?}"
    );
}

/// A quadratic kernel (1e3, 1e4), (1e4, 1e5), (1e5, 1e7) misses a line by
/// far more than 10% at some point (−89% at 1e5 for the weighted line):
/// flagged, not silently accepted.
/// Moved from the ASAPPlanner test `fit_flags_nonlinear`.
#[test]
fn fit_flags_nonlinear() {
    let fit = fit_constants(
        &OPERATOR,
        &operator_points(&[(1e3, 1e4), (1e4, 1e5), (1e5, 1e7)]),
    )
    .unwrap();
    assert!(
        fit.flags
            .iter()
            .any(|flag| matches!(flag, FitFlag::ResidualAboveTolerance { .. })),
        "{fit:?}"
    );
}

/// `measure_sizes` over a fake clock, warm-up 1 and 5 measured runs, on a
/// kernel charging 50 ns per unit + 10 µs per call: the points are exactly
/// (1e3, 60,000), (1e4, 510,000), (1e5, 5,010,000) with setting = the size
/// and terms `[units, 1]` (one call per run, not a batch), and they fit
/// 50 / 10,000.
/// Moved from the ASAPPlanner test `fit_recovers_fake_clock_kernel`.
#[test]
fn fit_recovers_fake_clock_kernel() {
    let clock = FakeClock::default();
    let kernel_clock = clock.clone();
    let protocol = Protocol {
        warmup_runs: 1,
        measured_runs: 5,
        max_core_busy_fraction: 0.10,
    };
    let measured = measure_sizes(
        &protocol,
        0,
        &mut IdleProbe,
        &EngineClock::CallingThread(&clock),
        &[1_000, 10_000, 100_000],
        &mut |units| {
            kernel_clock.charge_thread(50 * units + 10_000);
            Ok(units)
        },
    )
    .unwrap();
    assert_eq!(
        measured,
        operator_points(&[(1e3, 60_000.0), (1e4, 510_000.0), (1e5, 5_010_000.0)])
    );
    let fit = fit_constants(&OPERATOR, &measured).unwrap();
    assert!(
        (constant(&fit, "ns_per_unit") - 50.0).abs() < 1e-9,
        "{fit:?}"
    );
    assert!(
        (constant(&fit, "ns_per_invocation") - 10_000.0).abs() < 1e-6,
        "{fit:?}"
    );
}

/// On the OS thread CPU clock, a kernel that busy-spins 50 ns per unit +
/// 20 µs per call comes back within ±5 ns per unit and ±5 µs per call
/// (9 measured runs on an idle core).
/// Moved from the ASAPPlanner test `fit_recovers_spin_kernel`.
#[test]
#[ignore = "timing: run on a dedicated host"]
fn fit_recovers_spin_kernel() {
    let protocol = Protocol {
        warmup_runs: 1,
        measured_runs: 9,
        max_core_busy_fraction: 0.10,
    };
    let measured = measure_sizes(
        &protocol,
        0,
        &mut OsLoadProbe::default(),
        &EngineClock::CallingThread(&OsCpuClock),
        &[1_000, 10_000, 100_000],
        &mut |units| {
            let until = Instant::now() + Duration::from_nanos(50 * units + 20_000);
            while Instant::now() < until {
                std::hint::spin_loop();
            }
            Ok(units)
        },
    )
    .unwrap();
    let fit = fit_constants(&OPERATOR, &measured).unwrap();
    assert!(
        (constant(&fit, "ns_per_unit") - 50.0).abs() <= 5.0,
        "{fit:?}"
    );
    assert!(
        (constant(&fit, "ns_per_invocation") - 20_000.0).abs() <= 5_000.0,
        "{fit:?}"
    );
}

/// Five settings measured exactly as 2 ns per sample + 150 ns per series +
/// 10,000 ns per call (13,500, 31,500, 27,000, 225,000, 180,000): every
/// fold recovers (2, 150, 10,000) within 1e-9 relative and predicts its
/// held-out setting with error < 1e-12; the RMS log error is < 1e-12.
#[test]
fn leave_one_setting_out_recovers_a_known_linear_model() {
    let loso = leave_one_setting_out(&MODEL, &model_points(&MODEL_TERMS, &MODEL_MEASURED)).unwrap();
    assert_eq!(loso.held_out.len(), 5, "{loso:?}");
    assert_eq!(loso.settings_scored, 5, "{loso:?}");
    for (fold, setting) in loso.held_out.iter().zip(["s1", "s2", "s3", "s4", "s5"]) {
        assert_eq!(fold.setting, setting, "{loso:?}");
        for (name, want) in MODEL.iter().zip([2.0, 150.0, 10_000.0]) {
            assert!(
                relative_difference(constant(&fold.fit, name), want) < 1e-9,
                "{fold:?}"
            );
        }
        for error in fold.point_relative_errors.as_ref().unwrap() {
            assert!(error.abs() < 1e-12, "{fold:?}");
        }
    }
    assert!(loso.rms_log_error.unwrap() < 1e-12, "{loso:?}");
}

/// F8 with setting 4 measured ×1.5 (337,500): the fold without it recovers
/// (2, 150, 10,000) and predicts 225,000, held-out error −0.3333. The other
/// folds score −0.2443, +0.3284, −0.0422 and +0.0998 (±1e-3), and the RMS
/// log error is 0.2586 (±1e-3).
#[test]
fn leave_one_setting_out_exposes_an_outlier_setting() {
    let loso =
        leave_one_setting_out(&MODEL, &model_points(&MODEL_TERMS, &OUTLIER_MEASURED)).unwrap();
    assert_eq!(loso.settings_scored, 5, "{loso:?}");
    let without_outlier = &loso.held_out[3];
    assert_eq!(without_outlier.setting, "s4");
    for (name, want) in MODEL.iter().zip([2.0, 150.0, 10_000.0]) {
        assert!(
            relative_difference(constant(&without_outlier.fit, name), want) < 1e-9,
            "{without_outlier:?}"
        );
    }
    let want = [-0.2443, 0.3284, -0.0422, -0.3333, 0.0998];
    for (got, want) in held_out_errors(&loso).into_iter().zip(want) {
        let got = got.unwrap();
        assert!((got - want).abs() < 1e-3, "{got} vs {want}: {loso:?}");
    }
    assert!(
        (loso.rms_log_error.unwrap() - 0.2586).abs() < 1e-3,
        "{loso:?}"
    );
}

/// F8 with the series term non-zero only in setting 5: the fold without
/// setting 5 cannot fit `ns_per_series`, so it is flagged
/// `ConstantNotIdentified` and unscored (`relative_error: None`), and only
/// 4 settings are scored. With exact data the RMS is < 1e-12; with
/// setting 4 measured ×1.5 the scored errors are −0.0566, +0.3182, −0.0566,
/// −0.3333 and the RMS log error is 0.2488 over those 4 (0.2225 if the
/// unscored fold counted as 0).
#[test]
fn a_constant_only_one_setting_exercises_is_not_identified_when_it_is_held_out() {
    let terms = [
        [1e3, 0.0, 1.0],
        [1e4, 0.0, 1.0],
        [1e3, 0.0, 1.0],
        [1e5, 0.0, 1.0],
        [1e4, 1000.0, 1.0],
    ];
    let exact = [12_000.0, 30_000.0, 12_000.0, 210_000.0, 180_000.0];
    let loso = leave_one_setting_out(&MODEL, &model_points(&terms, &exact)).unwrap();
    let fold = &loso.held_out[4];
    assert_eq!(fold.setting, "s5");
    assert!(
        fold.fit.flags.iter().any(|flag| matches!(
            flag,
            FitFlag::ConstantNotIdentified { constant } if constant == "ns_per_series"
        )),
        "{fold:?}"
    );
    assert_eq!(fold.point_relative_errors, None, "{fold:?}");
    assert_eq!(loso.settings_scored, 4, "{loso:?}");
    assert!(loso.rms_log_error.unwrap() < 1e-12, "{loso:?}");

    let mut outlier = exact;
    outlier[3] *= 1.5;
    let loso = leave_one_setting_out(&MODEL, &model_points(&terms, &outlier)).unwrap();
    assert_eq!(loso.settings_scored, 4, "{loso:?}");
    let want = [
        Some(-0.0566),
        Some(0.3182),
        Some(-0.0566),
        Some(-0.3333),
        None,
    ];
    for (got, want) in held_out_errors(&loso).into_iter().zip(want) {
        match (got, want) {
            (Some(got), Some(want)) => {
                assert!((got - want).abs() < 1e-3, "{got} vs {want}: {loso:?}")
            }
            (None, None) => {}
            _ => panic!("{got:?} vs {want:?}: {loso:?}"),
        }
    }
    assert!(
        (loso.rms_log_error.unwrap() - 0.2488).abs() < 1e-3,
        "{loso:?}"
    );
}

/// Leave-one-setting-out over a single setting (two points of `only`) has
/// nothing to fit on: `TooFewSettings { settings: 1 }`.
#[test]
fn leave_one_setting_out_needs_two_settings() {
    let points = vec![
        FitPoint {
            setting: "only".into(),
            terms: vec![1e3, 1.0],
            measured: 60_000.0,
        },
        FitPoint {
            setting: "only".into(),
            terms: vec![1e4, 1.0],
            measured: 510_000.0,
        },
    ];
    assert_eq!(
        leave_one_setting_out(&OPERATOR, &points),
        Err(FitError::TooFewSettings { settings: 1 })
    );
}

/// A point measured 0, and then one measured NaN, in setting `s3`: the
/// weight 1/y² is undefined, so the fit refuses with `BadMeasurement`
/// naming `s3`.
#[test]
fn a_zero_or_nan_measurement_is_refused_by_setting() {
    for bad in [0.0, f64::NAN] {
        let points = vec![
            FitPoint {
                setting: "s1".into(),
                terms: vec![1e3, 1.0],
                measured: 60_000.0,
            },
            FitPoint {
                setting: "s2".into(),
                terms: vec![1e4, 1.0],
                measured: 510_000.0,
            },
            FitPoint {
                setting: "s3".into(),
                terms: vec![1e5, 1.0],
                measured: bad,
            },
        ];
        let error = fit_constants(&OPERATOR, &points).unwrap_err();
        assert!(
            matches!(
                &error,
                FitError::BadMeasurement { setting, measured }
                    if setting == "s3" && (*measured == bad || (bad.is_nan() && measured.is_nan()))
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("s3"), "{error}");
    }
}

/// F9's five points shuffled by `Xoshiro256PlusPlus` seeds 1, 2 and 3 fit
/// the same constants (2.6154, 149.36, 8,733.3) as in order, within 1e-12
/// relative.
#[test]
fn fit_is_independent_of_point_order() {
    let points = model_points(&MODEL_TERMS, &OUTLIER_MEASURED);
    let in_order = fit_constants(&MODEL, &points).unwrap();
    for seed in [1, 2, 3] {
        let mut shuffled = points.clone();
        shuffled.shuffle(&mut Xoshiro256PlusPlus::seed_from_u64(seed));
        let fit = fit_constants(&MODEL, &shuffled).unwrap();
        for name in MODEL {
            assert!(
                relative_difference(constant(&fit, name), constant(&in_order, name)) < 1e-12,
                "seed {seed}: {fit:?} vs {in_order:?}"
            );
        }
    }
}

/// Two held-out points of one setting, one predicted 10% high and one 10%
/// low, report both errors, `[+0.1, -0.1]`: they do not cancel to 0. The
/// model is 1 ns per unit with no other constant; setting `a` (units 100 and
/// 200) fits it exactly, setting `b` measures 1,000 units at 900 ns and
/// 2,000 units at 2,222.2 ns.
#[test]
fn held_out_errors_of_opposite_sign_do_not_cancel() {
    let point = |setting: &str, units: f64, measured: f64| FitPoint {
        setting: setting.into(),
        terms: vec![units],
        measured,
    };
    let points = [
        point("a", 100.0, 100.0),
        point("a", 200.0, 200.0),
        point("b", 1_000.0, 1_000.0 / 1.1),
        point("b", 2_000.0, 2_000.0 / 0.9),
    ];
    let loso = leave_one_setting_out(&["ns_per_unit"], &points).unwrap();
    let errors = loso.held_out[1].point_relative_errors.clone().unwrap();
    assert_eq!(loso.held_out[1].setting, "b");
    assert_eq!(errors.len(), 2, "{loso:?}");
    assert!((errors[0] - 0.1).abs() < 1e-9, "{errors:?}");
    assert!((errors[1] + 0.1).abs() < 1e-9, "{errors:?}");
}

/// With every setting needing a constant only it exercises, no fold is
/// scored and the RMS log error is `None`, never `NaN` (which JSON writes as
/// `null` and cannot read back as a number).
#[test]
fn no_scored_fold_has_no_rms_log_error() {
    let point = |setting: &str, terms: Vec<f64>| FitPoint {
        setting: setting.into(),
        terms,
        measured: 1_000.0,
    };
    let loso = leave_one_setting_out(
        &["x", "y"],
        &[point("a", vec![1.0, 0.0]), point("b", vec![0.0, 1.0])],
    )
    .unwrap();
    assert_eq!(loso.settings_scored, 0, "{loso:?}");
    assert_eq!(loso.rms_log_error, None);
    let text = serde_json::to_string(&loso).unwrap();
    assert_eq!(
        serde_json::from_str::<LeaveOneSettingOut>(&text).unwrap(),
        loso
    );
}

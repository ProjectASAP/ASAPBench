//! The gate: the selected arm against the exact arm, on the engine's own cost
//! and the measured error.

use aqpbm_harness::gate::{gate_verdict, GateFailure, GateRow, GateVerdict};

const SETTING: &str = "1s-10s-raw";

fn row(
    exact: Option<f64>,
    selected: Result<Option<f64>, String>,
    error: f64,
    target: f64,
) -> GateRow {
    GateRow {
        workload: "w".into(),
        setting: SETTING.into(),
        exact_engine_cost_per_second: exact,
        selected_engine_cost_per_second: selected,
        exact_query_cpu_ns: None,
        selected_query_cpu_ns: None,
        measured_error: error,
        target_error: target,
    }
}

fn expected(workload: &str) -> (String, String) {
    (workload.into(), SETTING.into())
}

fn verdict_of(row: GateRow) -> GateVerdict {
    gate_verdict(&[expected("w")], &[row], true)
}

fn fails_with(verdict: &GateVerdict, failure: fn(&GateFailure) -> bool) -> bool {
    matches!(verdict, GateVerdict::Fail(failures) if failures.iter().any(failure))
}

fn fails(verdict: &GateVerdict) -> bool {
    matches!(verdict, GateVerdict::Fail(failures) if !failures.is_empty())
}

/// The threshold arithmetic: engine cost 105 against exact 100 passes
/// (≤ 1.05 ×), 105.01 fails; error 0.0100 against target 0.01 passes, 0.0101
/// fails; a missing exact arm, a failed selected arm, a NaN error, and a
/// missing or extra row each fail by name; without the cost gate, cost 500
/// with a met target passes.
/// Adapted from ASAPPlanner #1332 `gate_rule_pure`.
#[test]
fn gate_rule_pure() {
    assert_eq!(
        verdict_of(row(Some(100.0), Ok(Some(105.0)), 0.0, 0.01)),
        GateVerdict::Pass
    );
    assert!(fails_with(
        &verdict_of(row(Some(100.0), Ok(Some(105.01)), 0.0, 0.01)),
        |f| matches!(f, GateFailure::CostAboveExact { .. })
    ));
    assert_eq!(
        verdict_of(row(Some(100.0), Ok(Some(50.0)), 0.0100, 0.01)),
        GateVerdict::Pass
    );
    assert!(fails_with(
        &verdict_of(row(Some(100.0), Ok(Some(50.0)), 0.0101, 0.01)),
        |f| matches!(f, GateFailure::ErrorAboveTarget { .. })
    ));
    assert!(fails_with(
        &verdict_of(row(None, Ok(Some(50.0)), 0.0, 0.01)),
        |f| matches!(f, GateFailure::ExactArmMissing { .. })
    ));
    assert!(fails_with(
        &verdict_of(row(Some(100.0), Err("panicked".into()), 0.0, 0.01)),
        |f| matches!(f, GateFailure::SelectedArmFailed { .. })
    ));
    assert!(fails_with(
        &verdict_of(row(Some(100.0), Ok(Some(50.0)), f64::NAN, 0.01)),
        |f| matches!(f, GateFailure::ErrorNotANumber { .. })
    ));
    let missing = gate_verdict(
        &[expected("w"), expected("v")],
        &[row(Some(100.0), Ok(Some(50.0)), 0.0, 0.01)],
        true,
    );
    assert!(fails_with(&missing, |f| matches!(
        f,
        GateFailure::MissingRow { .. }
    )));
    let extra = gate_verdict(&[], &[row(Some(100.0), Ok(Some(50.0)), 0.0, 0.01)], true);
    assert!(fails_with(&extra, |f| matches!(
        f,
        GateFailure::ExtraRow { .. }
    )));
    assert!(fails(&extra));
    // Without the cost gate, a costly plan with a met target passes.
    assert_eq!(
        gate_verdict(
            &[expected("w")],
            &[row(Some(100.0), Ok(Some(500.0)), 0.0, 0.01)],
            false
        ),
        GateVerdict::Pass
    );
}

/// Engine cost 104 against exact 100 passes even though the selected arm's
/// query CPU is 4,222 ms against the exact arm's 204 ms; the row reports the
/// raw-CPU ratio 20.7 beside it.
#[test]
fn raw_cpu_is_reported_but_does_not_decide_the_gate() {
    let row = GateRow {
        exact_query_cpu_ns: Some(204_000_000),
        selected_query_cpu_ns: Some(4_222_000_000),
        ..row(Some(100.0), Ok(Some(104.0)), 0.0, 0.01)
    };
    assert_eq!(verdict_of(row.clone()), GateVerdict::Pass);
    let ratio = row.raw_cpu_ratio().expect("both CPUs are reported");
    assert!((ratio - 4_222.0 / 204.0).abs() < 1e-9, "{ratio}");
    assert_eq!(format!("{ratio:.1}"), "20.7");
}

/// A selected arm without an engine cost (`Ok(None)`, an external engine)
/// fails with exactly `EngineCostMissing` for workload `w`; it is never
/// compared on raw CPU instead.
#[test]
fn a_row_without_an_engine_cost_fails_by_name() {
    let row = GateRow {
        exact_query_cpu_ns: Some(200_000_000),
        selected_query_cpu_ns: Some(100_000_000),
        ..row(Some(100.0), Ok(None), 0.0, 0.01)
    };
    assert_eq!(
        verdict_of(row),
        GateVerdict::Fail(vec![GateFailure::EngineCostMissing {
            workload: "w".into(),
            setting: SETTING.into(),
        }])
    );
}

/// A selected cost of NaN (or an infinite exact cost) fails with
/// `EngineCostNotANumber`, not a pass: `NaN > 1.05 * exact` is false, so the
/// ratio check alone would let it through.
#[test]
fn a_cost_that_is_not_a_number_fails_the_gate() {
    for (exact, selected) in [(10.0, f64::NAN), (f64::INFINITY, 10.0)] {
        let verdict = verdict_of(row(Some(exact), Ok(Some(selected)), 0.0, 0.01));
        assert!(
            fails_with(&verdict, |f| matches!(
                f,
                GateFailure::EngineCostNotANumber { .. }
            )),
            "{verdict:?}"
        );
    }
}

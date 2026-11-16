//! The conformance kit against the fake engine and its faults.

use aqpbm_harness::conformance::{check, ConformanceCheck, ConformanceFailure};
use aqpbm_harness::fake::{FakeEngine, FakeFault};

/// Runs the kit on a fake with `faults`, reading the fake's own clock.
fn check_fake(faults: &[FakeFault]) -> Result<Vec<ConformanceCheck>, Vec<ConformanceFailure>> {
    let mut engine = FakeEngine {
        faults: faults.to_vec(),
        ..FakeEngine::default()
    };
    let clock = engine.clock.clone();
    check(&mut engine, &clock).map(|report| report.passed)
}

/// The default fake (50 ns per sample, 150 ns per series, 10 µs per call,
/// no faults) passes all 4 checks, listed in check order.
#[test]
fn the_fake_engine_passes_the_conformance_kit() {
    assert_eq!(
        check_fake(&[]),
        Ok(vec![
            ConformanceCheck::DeterministicAnswers,
            ConformanceCheck::LeftOpenWindow,
            ConformanceCheck::CalibrationUnits,
            ConformanceCheck::WorkOnCallingThread,
        ])
    );
}

/// A fake that adds 0.5 to every answer from its second install on fails
/// the determinism check: the failure names seed 7, the time, and both
/// answers, the second 0.5 above the first.
#[test]
fn a_fake_with_nondeterministic_answers_fails_and_shows_the_differing_answer() {
    let failures = check_fake(&[FakeFault::NondeterministicAnswers]).unwrap_err();
    let failure = failures
        .iter()
        .find(|f| matches!(f, ConformanceFailure::NondeterministicAnswers { .. }))
        .unwrap_or_else(|| panic!("no NondeterministicAnswers in {failures:?}"));
    let ConformanceFailure::NondeterministicAnswers {
        seed,
        at_ms,
        series,
        first,
        second,
    } = failure
    else {
        unreachable!()
    };
    assert_eq!(*seed, 7);
    assert_eq!(*second, *first + 0.5);
    let message = failure.to_string();
    assert!(
        message.starts_with(&format!(
            "NondeterministicAnswers: answers differ between two installs with seed 7 at t={at_ms} ms: {series} {first} vs {second}"
        )),
        "{message}"
    );
    assert!(series.contains("probe"), "{series}");
}

/// A fake that counts the sample on the window start answers 2 for
/// `count_over_time(probe[10s])` at t = 20,000 ms over the probe samples at
/// 10,000 and 20,000 ms; the left-open window (10000, 20000] holds 1. It is
/// the only failure.
#[test]
fn a_fake_that_counts_the_window_start_fails_the_boundary_check() {
    let failures = check_fake(&[FakeFault::CountsWindowStart]).unwrap_err();
    let expected = ConformanceFailure::WindowNotLeftOpen {
        query: "count_over_time(probe[10s])".into(),
        at_ms: 20_000,
        returned: 2.0,
        expected: 1.0,
        window_start_ms: 10_000,
    };
    assert_eq!(failures, vec![expected.clone()]);
    assert_eq!(
        expected.to_string(),
        "WindowNotLeftOpen: count_over_time(probe[10s]) at t=20000 ms returned 2, expected 1: the window is (10000, 20000]"
    );
}

/// A fake whose calibration closure processes 1,000 units at size 1,000
/// while `charged_units` reports 2,000 fails the units check, naming the
/// target `fake/ingest`, the size and both unit counts. Replaces ASAPPlanner
/// #1332 `units_match_cost_model`, which parsed the cost model's text.
#[test]
fn a_fake_whose_calibration_units_differ_from_its_charged_units_fails() {
    let failures = check_fake(&[FakeFault::CalibrationUnitsDouble]).unwrap_err();
    let expected = ConformanceFailure::UnitsMismatch {
        target: "fake/ingest".into(),
        size: 1_000,
        closure_units: 1_000,
        charged_units: 2_000.0,
    };
    assert!(failures.contains(&expected), "{failures:?}");
    let message = expected.to_string();
    for needle in ["fake/ingest", "1000", "2000"] {
        assert!(message.contains(needle), "{needle} not in {message}");
    }
    assert!(
        failures
            .iter()
            .all(|f| matches!(f, ConformanceFailure::UnitsMismatch { .. })),
        "{failures:?}"
    );
}

/// A `CallingThread` fake whose ingest charges 1 ms of thread CPU and 9 ms of
/// process CPU fails the calling-thread check: the message says ingest used
/// 9.0 ms of process CPU but 1.0 ms on the calling thread, and how to fix it.
#[test]
fn a_fake_that_works_on_another_thread_fails_the_calling_thread_check() {
    let failures = check_fake(&[FakeFault::WorksOnAnotherThread]).unwrap_err();
    let failure = failures
        .iter()
        .find(|f| matches!(f, ConformanceFailure::WorkOffCallingThread { .. }))
        .unwrap_or_else(|| panic!("no WorkOffCallingThread in {failures:?}"));
    let ConformanceFailure::WorkOffCallingThread {
        step,
        process_cpu_ns,
        thread_cpu_ns,
    } = failure
    else {
        unreachable!()
    };
    assert_eq!(step, "ingest");
    assert!(process_cpu_ns > thread_cpu_ns);
    assert_eq!(
        failure.to_string(),
        "WorkOffCallingThread: ingest used 9.0 ms of process CPU but 1.0 ms on the calling thread: report CpuAccounting::Processes or do the work on the calling thread"
    );
}

/// With the nondeterministic-answers and window-start faults together, both
/// failures are listed, in check order: every `NondeterministicAnswers`
/// before every `WindowNotLeftOpen`.
#[test]
fn every_conformance_failure_is_listed() {
    let failures = check_fake(&[
        FakeFault::NondeterministicAnswers,
        FakeFault::CountsWindowStart,
    ])
    .unwrap_err();
    let first_window = failures
        .iter()
        .position(|f| matches!(f, ConformanceFailure::WindowNotLeftOpen { .. }))
        .unwrap_or_else(|| panic!("no WindowNotLeftOpen in {failures:?}"));
    let last_nondeterministic = failures
        .iter()
        .rposition(|f| matches!(f, ConformanceFailure::NondeterministicAnswers { .. }))
        .unwrap_or_else(|| panic!("no NondeterministicAnswers in {failures:?}"));
    assert!(
        last_nondeterministic < first_window,
        "not in check order: {failures:?}"
    );
}

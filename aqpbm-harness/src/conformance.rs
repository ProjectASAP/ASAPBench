//! The conformance kit every engine adapter runs: answers are deterministic
//! under one seed, windows are left-open, calibration closures process the
//! units the engine's cost model charges, and a `CallingThread` engine does
//! its work on the calling thread.

use crate::engine::{EngineUnderTest, WorkloadSpec};
use crate::protocol::CpuClock;

/// One check of the kit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceCheck {
    DeterministicAnswers,
    LeftOpenWindow,
    CalibrationUnits,
    WorkOnCallingThread,
}

/// The checks an engine passed, in check order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    pub passed: Vec<ConformanceCheck>,
}

/// A failed check, with what was seen.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConformanceFailure {
    /// Two installs with one seed answered differently.
    #[error("NondeterministicAnswers: answers differ between two installs with seed {seed} at t={at_ms} ms: {series} {first} vs {second}")]
    NondeterministicAnswers {
        seed: u64,
        at_ms: i64,
        series: String,
        first: f64,
        second: f64,
    },
    /// A sample on the window start was counted.
    #[error("WindowNotLeftOpen: {query} at t={at_ms} ms returned {returned}, expected {expected}: the window is ({window_start_ms}, {at_ms}]")]
    WindowNotLeftOpen {
        query: String,
        at_ms: i64,
        returned: f64,
        expected: f64,
        window_start_ms: i64,
    },
    /// The calibration closure processed other units than the cost model charges.
    #[error("UnitsMismatch: target {target} at size {size}: the calibration closure processed {closure_units} units but the cost model charges {charged_units}")]
    UnitsMismatch {
        target: String,
        size: u64,
        closure_units: u64,
        charged_units: f64,
    },
    /// A `CallingThread` engine used CPU off the calling thread.
    #[error("WorkOffCallingThread: {step} used {:.1} ms of process CPU but {:.1} ms on the calling thread: report CpuAccounting::Processes or do the work on the calling thread", *process_cpu_ns as f64 / 1e6, *thread_cpu_ns as f64 / 1e6)]
    WorkOffCallingThread {
        step: String,
        process_cpu_ns: u64,
        thread_cpu_ns: u64,
    },
}

/// Runs the probe workloads twice with one seed, checks the window boundary,
/// the calibration units and (for `CallingThread` engines) that no step's
/// work ran on another thread. Every failure is listed, in check order.
pub fn check(
    engine: &mut dyn EngineUnderTest,
    clock: &dyn CpuClock,
) -> Result<ConformanceReport, Vec<ConformanceFailure>> {
    let _ = (engine, clock);
    todo!("implemented after test approval")
}

/// The probe workloads, each with PromQL and SQL texts: `probe` sampled every
/// 10 s from 0 (first sample at 10,000 ms), with `sum_over_time(probe[10s])`
/// and `count_over_time(probe[10s])` evaluated every 10 s; seed 7.
pub fn probe_workloads() -> Vec<WorkloadSpec> {
    todo!("implemented after test approval")
}

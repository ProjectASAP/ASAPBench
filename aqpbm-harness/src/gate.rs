//! The gate: per (workload, setting) row, the selected arm passes when its
//! engine cost is at most 1.05 times the exact arm's and its measured error is
//! at most the target. Raw CPU is reported beside the engine cost and never
//! decides.

use serde::{Deserialize, Serialize};

/// The selected arm may cost at most this many times the exact arm.
pub const GATE_MAX_COST_RATIO: f64 = 1.05;

/// One row of the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRow {
    pub workload: String,
    pub setting: String,
    /// The exact arm's engine cost per second; `None` when the exact arm is missing.
    pub exact_engine_cost_per_second: Option<f64>,
    /// `Err` with the reason when the selected arm failed; `Ok(None)` when the
    /// engine has no cost model.
    pub selected_engine_cost_per_second: Result<Option<f64>, String>,
    /// Reported, never deciding.
    pub exact_query_cpu_ns: Option<u64>,
    /// Reported, never deciding.
    pub selected_query_cpu_ns: Option<u64>,
    pub measured_error: f64,
    pub target_error: f64,
}

impl GateRow {
    /// Selected over exact query CPU, for the summary; `None` when either is missing.
    pub fn raw_cpu_ratio(&self) -> Option<f64> {
        todo!("implemented after test approval")
    }
}

/// Why a row fails.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateFailure {
    CostAboveExact {
        workload: String,
        setting: String,
        selected: f64,
        exact: f64,
    },
    ErrorAboveTarget {
        workload: String,
        setting: String,
        measured_error: f64,
        target_error: f64,
    },
    ExactArmMissing {
        workload: String,
        setting: String,
    },
    SelectedArmFailed {
        workload: String,
        setting: String,
        reason: String,
    },
    EngineCostMissing {
        workload: String,
        setting: String,
    },
    ErrorNotANumber {
        workload: String,
        setting: String,
    },
    /// An expected (workload, setting) has no row.
    MissingRow {
        workload: String,
        setting: String,
    },
    /// A row nobody expected.
    ExtraRow {
        workload: String,
        setting: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateVerdict {
    Pass,
    Fail(Vec<GateFailure>),
}

/// The verdict over `rows` against the `(workload, setting)` pairs expected.
/// With `cost_gate` false only errors and row presence decide.
pub fn gate_verdict(
    expected: &[(String, String)],
    rows: &[GateRow],
    cost_gate: bool,
) -> GateVerdict {
    let _ = (expected, rows, cost_gate);
    todo!("implemented after test approval")
}

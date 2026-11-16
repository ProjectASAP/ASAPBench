//! The JSON records of a run: `run.json`, one `results/<engine>/<workload>__<arm>.json`
//! per arm, and `calibrate-run.json`.

use serde::{Deserialize, Serialize};

use crate::engine::{
    Answers, EngineDescription, Footprint, NodeProfile, PlanSummary, PredictedCost,
};
use crate::fit::{ConstantsFit, FitPoint};
use crate::machine::MachineDescription;
use crate::protocol::{JobRecord, Protocol};

/// `run.json`: how the run was started and under which protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    /// The command line.
    pub args: Vec<String>,
    /// `CARGO_PKG_VERSION`, plus the build-time git commit when known.
    pub harness_version: String,
    pub machine: MachineDescription,
    pub engines: Vec<EngineDescription>,
    pub protocol: Protocol,
}

/// One arm's results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmResult {
    pub engine: String,
    pub workload: String,
    pub arm: String,
    pub plan: PlanSummary,
    pub predicted: Option<PredictedCost>,
    /// One job per refresh chunk.
    pub ingest: Vec<JobRecord>,
    /// One job per evaluation time.
    pub evaluate: Vec<JobRecord>,
    /// The answers at each evaluation time.
    pub answers: Vec<(i64, Answers)>,
    pub footprint: Footprint,
    /// Measured usage at the engine's own prices; `None` for an engine
    /// without a cost model.
    pub engine_cost_per_second: Option<f64>,
    pub node_profiles: Option<Vec<NodeProfile>>,
}

/// `calibrate-run.json`: the raw points and fits of one calibration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationRun {
    pub engine: EngineDescription,
    pub machine: MachineDescription,
    pub protocol: Protocol,
    pub targets: Vec<CalibratedTarget>,
}

/// One calibration target's points and fit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibratedTarget {
    /// The engine's opaque target key.
    pub key: serde_json::Value,
    pub points: Vec<FitPoint>,
    pub fit: ConstantsFit,
}

/// A warning when `current` differs from the engine a calibration was
/// measured on, or is dirty, naming both versions and `calibrate`.
pub fn calibration_staleness(
    recorded: &EngineDescription,
    current: &EngineDescription,
) -> Option<String> {
    if recorded.id != current.id || recorded.version != current.version {
        return Some(format!(
            "the calibration was measured on {} {} but the engine is {} {}: rerun calibrate",
            recorded.id, recorded.version, current.id, current.version
        ));
    }
    if recorded.version_dirty || current.version_dirty {
        return Some(format!(
            "the engine {} {} is dirty (uncommitted changes), so it may not be the build \
             the calibration was measured on: commit and rerun calibrate",
            current.id, current.version
        ));
    }
    None
}

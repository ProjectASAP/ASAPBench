//! The JSON records of a run: `run.json`, one `results/<engine>/<workload>__<arm>.json`
//! per arm, and `calibrate-run.json`.

use serde::{Deserialize, Serialize};

use crate::harness::engine::{
    Answers, EngineDescription, Footprint, NodeProfile, PlanSummary, PredictedCost,
};
use crate::harness::cgroup::CgroupMemory;
use crate::harness::fit::{ConstantsFit, FitPoint};
use crate::harness::machine::MachineDescription;
use crate::harness::protocol::{JobRecord, Protocol};

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
    /// Samples ingested by one install: the sum over refresh chunks.
    pub ingested_samples: u64,
    /// For a [`crate::harness::engine::CpuAccounting::Cgroup`] engine, the server's
    /// memory before the installs.
    pub server_memory_before: Option<CgroupMemory>,
    /// The same after the last step. The server then holds every install of
    /// the arm at once (`installs_held` of them), so this is not one
    /// install's memory.
    pub server_memory_after: Option<CgroupMemory>,
    /// For a `Cgroup` engine, the server's CPU from before the installs to
    /// after the footprint, in ns: the steps plus work between them, such
    /// as background merges, over every install.
    pub server_cpu_ns_whole_arm: Option<u64>,
    /// Installs held at once while the arm ran: warm-up plus measured runs.
    pub installs_held: u32,
    /// The usage the harness measured, priced at the engine's own prices
    /// ([`crate::harness::engine::InstalledArm::price`]); `None` for an engine without
    /// a cost model. Compare with the engine's own prediction,
    /// [`PredictedCost::engine_cost_per_second`].
    pub measured_engine_cost_per_second: Option<f64>,
    pub node_profiles: Option<Vec<NodeProfile>>,
}

/// `calibrate-run.json`: the raw points and fits of one calibration. Written
/// by the `calibrate` subcommand of the harness CLI; defined here because the
/// design names it as the harness-owned calibration record beside
/// [`RunRecord`] and [`ArmResult`].
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
    let dirty = match (recorded.version_dirty, current.version_dirty) {
        (false, false) => return None,
        (true, false) => "the calibration was measured on a dirty build",
        (false, true) => "the engine is a dirty build",
        (true, true) => "the calibration was measured on a dirty build, and the engine is one",
    };
    Some(format!(
        "{dirty} of {} {} (uncommitted changes), so the two may differ: commit and rerun calibrate",
        current.id, current.version
    ))
}

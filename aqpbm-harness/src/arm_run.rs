//! One arm of one workload, run under the protocol: install, ingest every
//! refresh chunk, evaluate at every time, and record the costs the harness
//! measured.

use crate::data::DataError;
use crate::engine::{ArmSpec, EngineError, EngineUnderTest, WorkloadSpec};
use crate::protocol::{CpuClock, LoadProbe, Protocol, ProtocolError};
use crate::records::ArmResult;

/// When to ingest and when to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmSchedule {
    /// Ingest in chunks `(end - refresh_ms, end]`.
    pub refresh_ms: u64,
    /// Evaluate at these times, ascending; each after the chunk that ends there.
    pub evaluate_at_ms: Vec<i64>,
}

/// Runs `arm` of `workload` on `engine` pinned to `core`. Each measured run
/// installs the arm afresh, so ingest stays a first ingest; the answers come
/// from the last measured run.
#[allow(clippy::too_many_arguments)]
pub fn run_arm(
    engine: &dyn EngineUnderTest,
    workload: &WorkloadSpec,
    arm: &ArmSpec,
    schedule: &ArmSchedule,
    protocol: &Protocol,
    core: usize,
    probe: &mut dyn LoadProbe,
    clock: &dyn CpuClock,
) -> Result<ArmResult, ArmRunError> {
    let _ = (
        engine, workload, arm, schedule, protocol, core, probe, clock,
    );
    todo!("implemented after test approval")
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ArmRunError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Data(#[from] DataError),
}

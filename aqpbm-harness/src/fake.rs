//! A deterministic in-process engine on a shared fake clock, for the
//! harness's own tests.
//!
//! It answers `sum_over_time(<metric>[<w>])` and `count_over_time(<metric>[<w>])`
//! per series over `(t - w, t]`. Ingesting a chunk charges `ns_per_call +
//! ns_per_sample` per sample to the clock; evaluating charges `ns_per_call +
//! ns_per_series` per answered series. Example: with the defaults (50, 150
//! and 10,000 ns), ingesting 1,000 samples costs 60,000 ns and evaluating
//! 4 series 10,600 ns.
//!
//! Its plan has one `scan` node per metric, shared by every query over that
//! metric, and one node per query, named after the query id; the operator
//! timing hook times each node once per evaluation.

use std::cell::Cell;
use std::rc::Rc;

use crate::engine::{
    ArmSpec, Calibratable, CostTarget, CpuAccounting, EngineDescription, EngineError,
    EngineUnderTest, InstalledArm, WorkloadSpec,
};
use crate::protocol::CpuClock;

/// The ticks of a [`FakeClock`], in ns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FakeTicks {
    pub thread_ns: u64,
    pub process_ns: u64,
    pub wall_ns: u64,
}

/// A clock only the fake engine (or a test) advances. Clones share the ticks.
#[derive(Debug, Clone, Default)]
pub struct FakeClock(pub Rc<Cell<FakeTicks>>);

impl FakeClock {
    /// Work on the calling thread: advances thread, process and wall time by `ns`.
    pub fn charge_thread(&self, ns: u64) {
        let _ = ns;
        todo!("implemented after test approval")
    }

    /// Work on another thread of this process: advances process and wall
    /// time by `ns`, thread time not at all.
    pub fn charge_other_thread(&self, ns: u64) {
        let _ = ns;
        todo!("implemented after test approval")
    }

    pub fn ticks(&self) -> FakeTicks {
        self.0.get()
    }
}

impl CpuClock for FakeClock {
    fn thread_cpu_ns(&self) -> u64 {
        self.0.get().thread_ns
    }
    fn process_cpu_ns(&self) -> u64 {
        self.0.get().process_ns
    }
    fn wall_ns(&self) -> u64 {
        self.0.get().wall_ns
    }
}

/// A broken behavior the conformance kit must catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeFault {
    /// Adds 0.5 to every answer from the second install on.
    NondeterministicAnswers,
    /// Counts a sample at the window start, `[t - w, t]`.
    CountsWindowStart,
    /// `charged_units` reports twice the units the calibration closure returns.
    CalibrationUnitsDouble,
    /// Charges 1 ms of thread CPU and 9 ms more on another thread per ingest.
    WorksOnAnotherThread,
}

/// The fake engine.
#[derive(Debug, Clone)]
pub struct FakeEngine {
    pub clock: FakeClock,
    pub ns_per_sample: u64,
    pub ns_per_series: u64,
    pub ns_per_call: u64,
    /// Reported in every `StepReport`, like an external engine's own counter.
    pub engine_reported_cpu_ns: Option<u64>,
    pub cpu_accounting: CpuAccounting,
    pub faults: Vec<FakeFault>,
}

impl Default for FakeEngine {
    /// 50 ns per sample, 150 ns per series, 10 µs per call, on a new clock,
    /// `CallingThread`, no faults.
    fn default() -> Self {
        FakeEngine {
            clock: FakeClock::default(),
            ns_per_sample: 50,
            ns_per_series: 150,
            ns_per_call: 10_000,
            engine_reported_cpu_ns: None,
            cpu_accounting: CpuAccounting::CallingThread,
            faults: Vec::new(),
        }
    }
}

impl EngineUnderTest for FakeEngine {
    fn description(&self) -> EngineDescription {
        todo!("implemented after test approval")
    }
    fn cpu_accounting(&self) -> CpuAccounting {
        todo!("implemented after test approval")
    }
    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        let _ = (workload, arm);
        todo!("implemented after test approval")
    }
    fn calibration(&mut self) -> Option<&mut dyn Calibratable> {
        todo!("implemented after test approval")
    }
}

/// One calibration target, `fake/ingest`: `ns_per_sample` over `samples`.
impl Calibratable for FakeEngine {
    fn cost_targets(&self) -> Vec<CostTarget> {
        todo!("implemented after test approval")
    }
    fn prepare_cost_target(
        &mut self,
        target: &CostTarget,
        size: u64,
    ) -> Result<Box<dyn FnMut() -> Result<u64, EngineError>>, EngineError> {
        let _ = (target, size);
        todo!("implemented after test approval")
    }
    fn charged_units(&self, target: &CostTarget, size: u64) -> f64 {
        let _ = (target, size);
        todo!("implemented after test approval")
    }
}

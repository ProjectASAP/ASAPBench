//! `run_arm` frees the answers of every run outside the timed window: an
//! evaluation step is timed from the clock reading before the engine call to
//! the reading after it, and freeing the previous run's answers in between
//! would charge their deallocation to the step.
//!
//! Its own test binary: it installs a global allocator that counts frees of
//! the one allocation size only the answers have.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;

use aqpbm_core::harness::arm_run::{run_arm, ArmSchedule};
use aqpbm_core::harness::data::{DataRecord, SampleChunk, SeriesLabels};
use aqpbm_core::harness::engine::{
    Answers, ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, Footprint,
    InstalledArm, PlanSummary, StepReport, WorkloadSpec,
};
use aqpbm_core::harness::protocol::{
    CpuClock, LoadProbe, MeasurementClocks, Protocol, ProtocolError,
};
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};

/// The byte length of the one label value every answer carries. Nothing
/// else in a run allocates this size, so a free of it is an answer's.
const MARKER_BYTES: usize = 7_919;

thread_local! {
    /// Between the clock reading before a timed engine call and the one after it.
    static IN_TIMED_WINDOW: Cell<bool> = const { Cell::new(false) };
    /// Frees of a `MARKER_BYTES` allocation inside a timed window.
    static ANSWER_FREES_IN_WINDOW: Cell<u32> = const { Cell::new(0) };
}

struct CountingAllocator;

// SAFETY: every call forwards to `System` unchanged; the bookkeeping touches
// only const-initialized thread locals, which never allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == MARKER_BYTES && IN_TIMED_WINDOW.try_with(Cell::get).unwrap_or(false) {
            let _ = ANSWER_FREES_IN_WINDOW.try_with(|frees| frees.set(frees.get() + 1));
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// A clock that reads 0 everywhere and opens or closes the timed window on
/// each wall-clock reading: the protocol reads the wall clock once before
/// and once after every timed call.
struct WindowClock;

impl CpuClock for WindowClock {
    fn thread_cpu_ns(&self) -> u64 {
        0
    }
    fn process_cpu_ns(&self) -> u64 {
        0
    }
    fn wall_ns(&self) -> u64 {
        IN_TIMED_WINDOW.with(|window| window.set(!window.get()));
        0
    }
}

struct IdleProbe;

impl LoadProbe for IdleProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        0.0
    }
}

/// An engine whose every evaluation answers query `q` with one series
/// `{m="xxx..."}` (`MARKER_BYTES` long), freshly allocated.
struct MarkerEngine;

impl EngineUnderTest for MarkerEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "marker".into(),
            version: "0".into(),
            version_dirty: false,
            build_profile: "test".into(),
        }
    }
    fn cpu_accounting(&self) -> CpuAccounting {
        CpuAccounting::CallingThread
    }
    fn install(
        &self,
        _workload: &WorkloadSpec,
        _arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        Ok(Box::new(MarkerArm))
    }
}

struct MarkerArm;

impl InstalledArm for MarkerArm {
    fn plan_summary(&self) -> PlanSummary {
        PlanSummary::default()
    }
    fn ingest(&mut self, _chunk: &SampleChunk) -> Result<StepReport, EngineError> {
        Ok(StepReport::default())
    }
    fn evaluate(&mut self, _at_ms: i64) -> Result<(Answers, StepReport), EngineError> {
        let series: SeriesLabels = [("m".to_string(), "x".repeat(MARKER_BYTES))]
            .into_iter()
            .collect();
        let answers = BTreeMap::from([("q".to_string(), BTreeMap::from([(series, 1.0)]))]);
        Ok((Answers(answers), StepReport::default()))
    }
    fn footprint(&self) -> Result<Footprint, EngineError> {
        Ok(Footprint::default())
    }
}

/// One series of `m`, one sample per second for 20 s.
fn workload() -> WorkloadSpec {
    WorkloadSpec {
        name: "w".into(),
        queries: Vec::new(),
        data: vec![DataRecord {
            metric: "m".into(),
            description: TimeSeriesDescription {
                labels: vec![SeriesLabelSpec {
                    name: "s".into(),
                    cardinality: 1,
                    skewness: 0.0,
                }],
                series_num: 1,
                start_ms: 0,
                sample_interval_ms: 1_000,
                duration_ms: 20_000,
                value_distribution: DataDistribution::Uniform(UniformParameter {
                    lower_bound: 0.0,
                    upper_bound: 1.0,
                    seed: 0,
                }),
                counter: false,
                seed: 1,
            },
        }],
        tables: Vec::new(),
        one_shot_queries: Vec::new(),
    }
}

/// Warm-up 1 and 5 runs, evaluations at 10 s and 20 s: each evaluation
/// step makes 6 engine calls with one answer each. None of the 12 answers
/// is freed inside a timed window, and the last run's answers are kept.
#[test]
fn answers_are_freed_outside_the_timed_window() {
    let workload = workload();
    let arm = ArmSpec {
        name: "exact".into(),
        raw_data_retained: true,
        engine_options: serde_json::Value::Null,
    };
    let schedule = ArmSchedule {
        refresh_ms: 10_000,
        evaluate_at_ms: vec![10_000, 20_000],
    };
    let result = run_arm(
        &MarkerEngine,
        &workload,
        &arm,
        &schedule,
        &Protocol::default(),
        0,
        &mut IdleProbe,
        &MeasurementClocks::with_calling_thread(&WindowClock),
    )
    .unwrap();
    assert_eq!(result.answers.len(), 2);
    for (_, answers) in &result.answers {
        assert_eq!(answers.iter().count(), 1);
    }
    assert_eq!(
        ANSWER_FREES_IN_WINDOW.with(Cell::get),
        0,
        "answers freed inside a timed window"
    );
}

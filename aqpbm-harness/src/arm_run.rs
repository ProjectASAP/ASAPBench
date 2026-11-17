//! One arm of one workload, run under the protocol: install, ingest every
//! refresh chunk, evaluate at every time, and record the costs the harness
//! measured.

use crate::data::{workload_chunks, DataError};
use crate::engine::{
    Answers, ArmSpec, EngineError, EngineUnderTest, InstalledArm, MeasuredUsage, WorkloadSpec,
};
use crate::protocol::{
    CoreLoad, EngineClock, JobRecord, LoadProbe, MeasurementClocks, Protocol, ProtocolError,
};
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
///
/// The engine is measured on the clock its [`EngineUnderTest::cpu_accounting`]
/// names: `clocks.calling_thread` for `CallingThread`, and for
/// `Processes(pids)` a clock over the server pids under `clocks.process_cpu_files` that
/// `run_arm` builds itself, never the harness process.
///
/// Every run gets its own install up front, and each step (one refresh
/// chunk, or one evaluation) is one protocol job whose k-th call goes to
/// install k. Example: warm-up 1 and 5 runs install 6 arms; the job of chunk
/// 3 ingests chunk 3 into each of them in turn and records the 5 measured
/// calls. An evaluation time runs after the chunk that ends at or after it.
///
/// Memory: all `warmup_runs + measured_runs` installs are held at once, so a
/// run needs that many times the memory of one arm (6× with the default
/// protocol). Installing them one after another instead would make a
/// protocol job span whole runs rather than one step, so the idle check and
/// the median would no longer be per step. Because consecutive calls go to
/// different installs, each call starts with the previous install's data in
/// the caches, not its own: steps are measured with cold caches, as a
/// refresh that runs after other work.
///
/// Answers: each call's answers are moved out of the timed call and freed
/// after the job, so freeing them is never charged to a step; the last
/// run's answers are kept.
///
/// Idle checks: consecutive jobs share a sample, the after-sample of one job
/// is the before-sample of the next, so each step costs one sampling interval
/// of the probe (100 ms with the OS probe).
#[allow(clippy::too_many_arguments)]
pub fn run_arm(
    engine: &dyn EngineUnderTest,
    workload: &WorkloadSpec,
    arm: &ArmSpec,
    schedule: &ArmSchedule,
    protocol: &Protocol,
    core: usize,
    probe: &mut dyn LoadProbe,
    clocks: &MeasurementClocks,
) -> Result<ArmResult, ArmRunError> {
    protocol.check_run_count()?;
    let data_ms = workload
        .data
        .iter()
        .map(|record| record.description.duration_ms)
        .max()
        .unwrap_or(0);
    if data_ms == 0 {
        return Err(ArmRunError::NoData {
            workload: workload.name.clone(),
        });
    }
    let clock = clocks.clock_for(&engine.cpu_accounting())?;
    let runs = (protocol.warmup_runs + protocol.measured_runs) as usize;
    let mut installs: Vec<Box<dyn InstalledArm>> = (0..runs)
        .map(|_| engine.install(workload, arm))
        .collect::<Result<_, _>>()?;

    let mut ingest = Vec::new();
    let mut evaluate = Vec::new();
    let mut answers = Vec::new();
    // The after-sample of the last job, the before-sample of the next.
    let mut load: Option<CoreLoad> = None;
    let mut pending_evaluations = schedule.evaluate_at_ms.iter().copied().peekable();
    let mut evaluate_at = |at_ms: i64,
                           installs: &mut Vec<Box<dyn InstalledArm>>,
                           probe: &mut dyn LoadProbe,
                           load: &mut Option<CoreLoad>|
     -> Result<(), ArmRunError> {
        // Every call's answers, reserved up front so the timed calls only
        // move into it; freed after the job, outside every timed window.
        let mut answers_per_call: Vec<Answers> = Vec::with_capacity(runs);
        let job = protocol.run_next_job(core, probe, *load, &clock, &mut || {
            let (step_answers, report) = installs[answers_per_call.len()].evaluate(at_ms)?;
            answers_per_call.push(step_answers);
            Ok(report)
        })?;
        *load = Some(job.load_after());
        evaluate.push(job);
        let last_answers = answers_per_call.pop().expect("at least one measured run");
        drop(answers_per_call);
        answers.push((at_ms, last_answers));
        Ok(())
    };
    for chunk in workload_chunks(&workload.data, schedule.refresh_ms)? {
        let chunk = chunk?;
        let mut call = 0;
        let job = protocol.run_next_job(core, probe, load, &clock, &mut || {
            let report = installs[call].ingest(&chunk);
            call += 1;
            report
        })?;
        load = Some(job.load_after());
        ingest.push(job);
        while let Some(at_ms) = pending_evaluations.next_if(|&at_ms| at_ms <= chunk.end_ms) {
            evaluate_at(at_ms, &mut installs, probe, &mut load)?;
        }
    }
    for at_ms in pending_evaluations {
        evaluate_at(at_ms, &mut installs, probe, &mut load)?;
    }

    let last = installs.last_mut().expect("at least one measured run");
    let footprint = last.footprint()?;
    let data_seconds = data_ms as f64 / 1_000.0;
    let usage = MeasuredUsage {
        ingest_cpu_ns_per_second: median_cpu_ns(&ingest, &clock) / data_seconds,
        query_cpu_ns_per_second: median_cpu_ns(&evaluate, &clock) / data_seconds,
        held_bytes: footprint.view_bytes + footprint.raw_retained_bytes,
    };
    Ok(ArmResult {
        engine: engine.description().id,
        workload: workload.name.clone(),
        arm: arm.name.clone(),
        plan: last.plan_summary(),
        predicted: last.predicted(),
        measured_engine_cost_per_second: last.price(&usage),
        node_profiles: last.node_profiles(),
        ingest,
        evaluate,
        answers,
        footprint,
    })
}

/// The summed median CPU of `jobs`, on the clock the medians were taken on.
fn median_cpu_ns(jobs: &[JobRecord], clock: &EngineClock) -> f64 {
    jobs.iter()
        .map(|job| clock.engine_cpu_ns(&job.median) as f64)
        .sum()
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ArmRunError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Data(#[from] DataError),
    /// A workload whose data last 0 ms: its costs per second of data would be
    /// infinite.
    #[error("workload {workload} has no data (duration 0 ms), so its cost per second of data is undefined")]
    NoData { workload: String },
}

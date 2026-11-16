//! One arm of one workload, run under the protocol: install, ingest every
//! refresh chunk, evaluate at every time, and record the costs the harness
//! measured.

use crate::data::{workload_chunks, DataError};
use crate::engine::{
    Answers, ArmSpec, CpuAccounting, EngineError, EngineUnderTest, InstalledArm, MeasuredUsage,
    WorkloadSpec,
};
use crate::protocol::{CpuClock, JobRecord, LoadProbe, MedianClock, Protocol, ProtocolError};
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
/// Every run gets its own install up front, and each step (one refresh
/// chunk, or one evaluation) is one protocol job whose k-th call goes to
/// install k. Example: warm-up 1 and 5 runs install 6 arms; the job of chunk
/// 3 ingests chunk 3 into each of them in turn and records the 5 measured
/// calls. An evaluation time runs after the chunk that ends at or after it.
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
    protocol.check_run_count()?;
    let median_clock = match engine.cpu_accounting() {
        CpuAccounting::CallingThread => MedianClock::Thread,
        CpuAccounting::Processes(_) => MedianClock::Process,
    };
    let runs = (protocol.warmup_runs + protocol.measured_runs) as usize;
    let mut installs: Vec<Box<dyn InstalledArm>> = (0..runs)
        .map(|_| engine.install(workload, arm))
        .collect::<Result<_, _>>()?;

    let mut ingest = Vec::new();
    let mut evaluate = Vec::new();
    let mut answers = Vec::new();
    let mut pending_evaluations = schedule.evaluate_at_ms.iter().copied().peekable();
    let mut evaluate_at = |at_ms: i64,
                           installs: &mut Vec<Box<dyn InstalledArm>>,
                           probe: &mut dyn LoadProbe|
     -> Result<(), ArmRunError> {
        let mut call = 0;
        let mut last_answers = Answers::default();
        let job = protocol.run_job_accounted(core, probe, clock, median_clock, &mut || {
            let (step_answers, report) = installs[call].evaluate(at_ms)?;
            call += 1;
            if call == runs {
                last_answers = step_answers;
            }
            Ok(report)
        })?;
        evaluate.push(job);
        answers.push((at_ms, last_answers));
        Ok(())
    };
    for chunk in workload_chunks(&workload.data, schedule.refresh_ms)? {
        let chunk = chunk?;
        let mut call = 0;
        let job = protocol.run_job_accounted(core, probe, clock, median_clock, &mut || {
            let report = installs[call].ingest(&chunk);
            call += 1;
            report
        })?;
        ingest.push(job);
        while let Some(at_ms) = pending_evaluations.next_if(|&at_ms| at_ms <= chunk.end_ms) {
            evaluate_at(at_ms, &mut installs, probe)?;
        }
    }
    for at_ms in pending_evaluations {
        evaluate_at(at_ms, &mut installs, probe)?;
    }

    let last = installs.last_mut().expect("at least one measured run");
    let footprint = last.footprint()?;
    let data_seconds = workload
        .data
        .iter()
        .map(|record| record.description.duration_ms)
        .max()
        .unwrap_or(0) as f64
        / 1_000.0;
    let usage = MeasuredUsage {
        ingest_cpu_ns_per_second: median_cpu_ns(&ingest, median_clock) / data_seconds,
        query_cpu_ns_per_second: median_cpu_ns(&evaluate, median_clock) / data_seconds,
        memory_bytes: footprint.view_bytes + footprint.raw_retained_bytes,
    };
    Ok(ArmResult {
        engine: engine.description().id,
        workload: workload.name.clone(),
        arm: arm.name.clone(),
        plan: last.plan_summary(),
        predicted: last.predicted(),
        engine_cost_per_second: last.price(&usage),
        node_profiles: last.node_profiles(),
        ingest,
        evaluate,
        answers,
        footprint,
    })
}

/// The summed median CPU of `jobs`, on the clock the medians were taken on.
fn median_cpu_ns(jobs: &[JobRecord], median_clock: MedianClock) -> f64 {
    jobs.iter()
        .map(|job| match median_clock {
            MedianClock::Thread => job.median.thread_cpu_ns.unwrap_or(0),
            MedianClock::Process => job.median.process_cpu_ns,
        } as f64)
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
}

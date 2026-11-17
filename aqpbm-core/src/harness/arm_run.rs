//! One arm of one workload, run under the protocol: install, ingest every
//! refresh chunk, evaluate at every time, and record the costs the harness
//! measured.

use crate::harness::data::{workload_chunks, DataError};
use crate::harness::engine::{
    Answers, ArmSpec, CpuAccounting, EngineError, EngineUnderTest, InstalledArm, MeasuredUsage,
    WorkloadSpec,
};
use crate::harness::protocol::{
    CoreLoad, EngineClock, JobRecord, LoadProbe, MeasurementClocks, Protocol, ProtocolError,
};
use crate::harness::records::ArmResult;

/// When to ingest and when to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmSchedule {
    /// Ingest in chunks `(end - refresh_ms, end]`.
    pub refresh_ms: u64,
    /// Evaluate at these times, ascending; each after the chunk that ends there.
    pub evaluate_at_ms: Vec<i64>,
}

impl ArmSchedule {
    /// The schedule every lane uses for `workload`: refresh every evaluation
    /// interval, and evaluate at every multiple of the interval
    /// (`t mod interval = 0`) from the data start plus the longest window
    /// through the data end.
    /// Example: windows 30 s and 20 s, every 10 s, data from 1,000 ms for
    /// 120 s: refresh 10 s, evaluations at 40,000, 50,000, ..., 120,000 ms.
    pub fn for_workload(workload: &WorkloadSpec) -> Result<ArmSchedule, ScheduleError> {
        let first = workload.queries.first().ok_or(ScheduleError::NoQueries {
            workload: workload.name.clone(),
        })?;
        let interval_ms = first.evaluation_interval_ms;
        if interval_ms == 0 {
            return Err(ScheduleError::ZeroInterval {
                query: first.id.clone(),
            });
        }
        if let Some(other) = workload
            .queries
            .iter()
            .find(|query| query.evaluation_interval_ms != interval_ms)
        {
            return Err(ScheduleError::MixedEvaluationIntervals {
                workload: workload.name.clone(),
                first_ms: interval_ms,
                other_ms: other.evaluation_interval_ms,
                other_query: other.id.clone(),
            });
        }
        let longest_window_ms = workload
            .queries
            .iter()
            .map(|q| q.window_ms)
            .max()
            .unwrap_or(0);
        let start_ms = workload
            .data
            .iter()
            .map(|record| record.description.start_ms)
            .min()
            .ok_or(ScheduleError::NoData {
                workload: workload.name.clone(),
            })?;
        let end_ms = workload
            .data
            .iter()
            .map(|record| record.description.start_ms + record.description.duration_ms as i64)
            .max()
            .unwrap_or(start_ms);
        let interval = interval_ms as i64;
        let earliest_ms = start_ms + longest_window_ms as i64;
        let first_ms = earliest_ms.div_euclid(interval) * interval
            + if earliest_ms.rem_euclid(interval) == 0 {
                0
            } else {
                interval
            };
        let evaluate_at_ms: Vec<i64> = (0..)
            .map(|k| first_ms + k * interval)
            .take_while(|&at_ms| at_ms <= end_ms)
            .collect();
        if evaluate_at_ms.is_empty() {
            return Err(ScheduleError::NoEvaluationTime {
                workload: workload.name.clone(),
                longest_window_ms,
                data_ms: (end_ms - start_ms) as u64,
            });
        }
        Ok(ArmSchedule {
            refresh_ms: interval_ms,
            evaluate_at_ms,
        })
    }
}

/// A workload no schedule fits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("workload {workload} has no queries")]
    NoQueries { workload: String },
    #[error("workload {workload} has no data")]
    NoData { workload: String },
    #[error("query {query} has an evaluation interval of 0 ms")]
    ZeroInterval { query: String },
    /// Every query of a workload is evaluated at every evaluation time, so
    /// one interval per workload; split the workload instead.
    #[error("workload {workload}: query {other_query} runs every {other_ms} ms but the first query every {first_ms} ms; one interval per workload")]
    MixedEvaluationIntervals {
        workload: String,
        first_ms: u64,
        other_ms: u64,
        other_query: String,
    },
    #[error("workload {workload}: the longest window ({longest_window_ms} ms) leaves no evaluation time inside {data_ms} ms of data")]
    NoEvaluationTime {
        workload: String,
        longest_window_ms: u64,
        data_ms: u64,
    },
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
    let server_cgroup = match engine.cpu_accounting() {
        CpuAccounting::Cgroup(source) => Some(source),
        _ => None,
    };
    let server_before = server_cgroup
        .as_ref()
        .map(|source| Ok::<_, ProtocolError>((source.cpu()?.usage_ns, source.memory()?)))
        .transpose()?;
    let mut installs: Vec<Box<dyn InstalledArm>> = (0..runs)
        .map(|_| engine.install(workload, arm))
        .collect::<Result<_, _>>()?;

    let mut ingest = Vec::new();
    let mut ingested_samples: u64 = 0;
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
        ingested_samples += chunk.samples.len() as u64;
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
    let server_after = server_cgroup
        .as_ref()
        .map(|source| Ok::<_, ProtocolError>((source.cpu()?.usage_ns, source.memory()?)))
        .transpose()?;
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
        ingested_samples,
        server_memory_before: server_before.map(|(_, memory)| memory),
        server_memory_after: server_after.map(|(_, memory)| memory),
        server_cpu_ns_whole_arm: server_before
            .zip(server_after)
            .map(|((before_ns, _), (after_ns, _))| after_ns.saturating_sub(before_ns)),
        cpu_source: clock.cpu_source(),
        installs_held: runs as u32,
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

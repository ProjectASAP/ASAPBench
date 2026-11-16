//! The conformance kit every engine adapter runs: answers are deterministic
//! under one seed, windows are left-open, calibration closures process the
//! units the engine's cost model charges, and a `CallingThread` engine does
//! its work on the calling thread.

use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};

use crate::data::{workload_chunks, DataRecord};
use crate::engine::{
    format_series, Answers, ArmSpec, CpuAccounting, EngineError, EngineUnderTest, InstalledArm,
    QueryLanguage, QueryText, WorkloadQuery, WorkloadSpec,
};
use crate::protocol::{CpuClock, MeasurementClocks};

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
    /// The engine refused or failed a step of a check.
    #[error("EngineFailed: {check:?}: {step}: {message}")]
    EngineFailed {
        check: ConformanceCheck,
        step: String,
        message: String,
    },
    /// A `CallingThread` engine used CPU off the calling thread. The CPU is
    /// per call, averaged over the calls the check repeated the step for.
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
///
/// The engine is timed on the clock its `cpu_accounting()` names, picked by
/// [`MeasurementClocks::clock_for`]; for a `Processes` engine, its pids must
/// be readable under `clocks.proc_fs`, or the check fails with
/// `EngineFailed { step: "cpu accounting" }`.
pub fn check(
    engine: &mut dyn EngineUnderTest,
    clocks: &MeasurementClocks,
) -> Result<ConformanceReport, Vec<ConformanceFailure>> {
    let workloads = probe_workloads();
    let mut passed = Vec::new();
    let mut failures = Vec::new();
    let mut record = |check, found: Vec<ConformanceFailure>| {
        if found.is_empty() {
            passed.push(check);
        }
        failures.extend(found);
    };
    record(
        ConformanceCheck::DeterministicAnswers,
        check_deterministic_answers(engine, &workloads),
    );
    record(
        ConformanceCheck::LeftOpenWindow,
        check_left_open_window(engine, &workloads[1]),
    );
    if let Some(found) = check_calibration_units(engine) {
        record(ConformanceCheck::CalibrationUnits, found);
    }
    let accounting = engine.cpu_accounting();
    match clocks.clock_for(&accounting) {
        Err(error) => failures.push(ConformanceFailure::EngineFailed {
            check: ConformanceCheck::WorkOnCallingThread,
            step: "cpu accounting".into(),
            message: error.to_string(),
        }),
        Ok(engine_clock) => {
            if accounting == CpuAccounting::CallingThread {
                record(
                    ConformanceCheck::WorkOnCallingThread,
                    check_work_on_calling_thread(engine, engine_clock.as_clock()),
                );
            }
        }
    }
    if failures.is_empty() {
        Ok(ConformanceReport { passed })
    } else {
        Err(failures)
    }
}

/// A `CallingThread` step may use this much more process than thread CPU
/// before it fails: 10% of its thread CPU, for other threads of the process
/// (the test runner's, the allocator's) and clock granularity.
const OFF_THREAD_TOLERANCE_FRACTION: f64 = 0.10;
/// Each step of the calling-thread check is repeated until it has used this
/// much CPU, so the 10% margin is 2 ms: other threads of a test process
/// using a millisecond during a step of a few microseconds no longer fail it.
const OFF_THREAD_MIN_MEASURED_NS: u64 = 20_000_000;
/// At most this many calls per step, for an engine whose step uses no CPU
/// the clock can see.
const OFF_THREAD_MAX_CALLS: u64 = 100_000;
/// A closure's units and the cost model's charged units match when they
/// differ by at most this share: fractional units are allowed.
const UNITS_RELATIVE_TOLERANCE: f64 = 1e-9;

/// The seed of the probe data.
const PROBE_SEED: u64 = 7;
/// The probe's sample interval, window and evaluation interval.
const PROBE_STEP_MS: u64 = 10_000;

fn probe_arm() -> ArmSpec {
    ArmSpec {
        name: "conformance".into(),
        raw_data_retained: true,
        engine_options: serde_json::Value::Null,
    }
}

fn engine_failed(check: ConformanceCheck, step: &str, error: EngineError) -> ConformanceFailure {
    ConformanceFailure::EngineFailed {
        check,
        step: step.into(),
        message: error.0,
    }
}

/// Installs `workload` and runs it to `until_ms`: ingest each refresh chunk,
/// then evaluate at its end. Returns the answers at each end.
fn run_probe(
    engine: &dyn EngineUnderTest,
    workload: &WorkloadSpec,
    until_ms: i64,
    check: ConformanceCheck,
) -> Result<Vec<(i64, Answers)>, ConformanceFailure> {
    let mut arm = engine
        .install(workload, &probe_arm())
        .map_err(|error| engine_failed(check, "install", error))?;
    let chunks = workload_chunks(&workload.data, PROBE_STEP_MS)
        .map_err(|error| engine_failed(check, "data", EngineError(error.to_string())))?;
    let mut answers = Vec::new();
    for chunk in chunks {
        let chunk =
            chunk.map_err(|error| engine_failed(check, "data", EngineError(error.to_string())))?;
        if chunk.end_ms > until_ms {
            break;
        }
        arm.ingest(&chunk)
            .map_err(|error| engine_failed(check, "ingest", error))?;
        let (at_end, _) = arm
            .evaluate(chunk.end_ms)
            .map_err(|error| engine_failed(check, "evaluate", error))?;
        answers.push((chunk.end_ms, at_end));
    }
    Ok(answers)
}

/// Two installs of each probe with one seed answer alike; per workload, the
/// first differing answer is reported.
fn check_deterministic_answers(
    engine: &dyn EngineUnderTest,
    workloads: &[WorkloadSpec],
) -> Vec<ConformanceFailure> {
    let check = ConformanceCheck::DeterministicAnswers;
    let mut failures = Vec::new();
    for workload in workloads {
        let runs = (
            run_probe(engine, workload, i64::MAX, check),
            run_probe(engine, workload, i64::MAX, check),
        );
        let (first, second) = match runs {
            (Ok(first), Ok(second)) => (first, second),
            (Err(failure), _) | (_, Err(failure)) => {
                failures.push(failure);
                continue;
            }
        };
        let differing = first.iter().zip(&second).find_map(|((at_ms, a), (_, b))| {
            let (query, series) = a
                .iter()
                .chain(b.iter())
                .map(|(query, series, _)| (query, series))
                .find(|(query, series)| {
                    a.get(query, series).map(f64::to_bits) != b.get(query, series).map(f64::to_bits)
                })?;
            Some(ConformanceFailure::NondeterministicAnswers {
                seed: workload.data[0].description.seed,
                at_ms: *at_ms,
                series: format!("{query}: {}", format_series(series)),
                first: a.get(query, series).unwrap_or(f64::NAN),
                second: b.get(query, series).unwrap_or(f64::NAN),
            })
        });
        failures.extend(differing);
    }
    failures
}

/// `count_over_time(probe[10s])` at t = 20,000 ms, over samples at 10,000 and
/// 20,000 ms, counts 1 per series: the window `(10000, 20000]` excludes its
/// start.
fn check_left_open_window(
    engine: &dyn EngineUnderTest,
    count_workload: &WorkloadSpec,
) -> Vec<ConformanceFailure> {
    let at_ms = 2 * PROBE_STEP_MS as i64;
    let query = &count_workload.queries[0];
    let answers = match run_probe(
        engine,
        count_workload,
        at_ms,
        ConformanceCheck::LeftOpenWindow,
    ) {
        Ok(answers) => answers,
        Err(failure) => return vec![failure],
    };
    let at_end = answers
        .into_iter()
        .find(|(t, _)| *t == at_ms)
        .map(|(_, answers)| answers)
        .unwrap_or_default();
    let window_failure = |returned: f64| ConformanceFailure::WindowNotLeftOpen {
        query: query.texts[0].text.clone(),
        at_ms,
        returned,
        expected: 1.0,
        window_start_ms: at_ms - query.window_ms as i64,
    };
    let counts: Vec<f64> = at_end.iter().map(|(_, _, count)| count).collect();
    if counts.is_empty() {
        return vec![window_failure(0.0)];
    }
    counts
        .into_iter()
        .find(|&count| count != 1.0)
        .map(|count| vec![window_failure(count)])
        .unwrap_or_default()
}

/// Every calibration closure processes the units the cost model charges.
/// `None` for an engine without a calibration.
fn check_calibration_units(engine: &mut dyn EngineUnderTest) -> Option<Vec<ConformanceFailure>> {
    let check = ConformanceCheck::CalibrationUnits;
    let calibration = engine.calibration()?;
    let mut failures = Vec::new();
    for target in calibration.cost_targets() {
        let name = target
            .key
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| target.key.to_string());
        for &size in &target.sizes {
            let units = calibration
                .prepare_cost_target(&target, size)
                .and_then(|mut run| run());
            match units {
                Err(error) => failures.push(engine_failed(check, &name, error)),
                Ok(closure_units) => {
                    let charged_units = calibration.charged_units(&target, size);
                    let closure = closure_units as f64;
                    if (closure - charged_units).abs()
                        > UNITS_RELATIVE_TOLERANCE * closure.abs().max(charged_units.abs())
                    {
                        failures.push(ConformanceFailure::UnitsMismatch {
                            target: name.clone(),
                            size,
                            closure_units,
                            charged_units,
                        });
                    }
                }
            }
        }
    }
    Some(failures)
}

/// Ingest and evaluate of the sum probe, each with no more process than
/// thread CPU (within 10%). Each step is repeated until it has used
/// [`OFF_THREAD_MIN_MEASURED_NS`] (ingest takes the probe's chunks in order,
/// from a probe that runs long enough; evaluate repeats at the last ingested
/// chunk's end); the failure reports the CPU per call. The first offending
/// step of each kind is reported.
fn check_work_on_calling_thread(
    engine: &dyn EngineUnderTest,
    clock: &dyn CpuClock,
) -> Vec<ConformanceFailure> {
    let check = ConformanceCheck::WorkOnCallingThread;
    let mut workload = probe_workloads().remove(0);
    for record in &mut workload.data {
        record.description.duration_ms = OFF_THREAD_MAX_CALLS * PROBE_STEP_MS;
    }
    let mut arm: Box<dyn InstalledArm> = match engine.install(&workload, &probe_arm()) {
        Ok(arm) => arm,
        Err(error) => return vec![engine_failed(check, "install", error)],
    };
    let mut chunks = match workload_chunks(&workload.data, PROBE_STEP_MS) {
        Ok(chunks) => chunks,
        Err(error) => return vec![engine_failed(check, "data", EngineError(error.to_string()))],
    };
    let mut failures = Vec::new();
    let mut last_end_ms = None;
    let ingest = repeat_until_measured(clock, "ingest", &mut || match chunks.next() {
        None => Ok(false),
        Some(Err(error)) => Err(EngineError(error.to_string())),
        Some(Ok(chunk)) => {
            last_end_ms = Some(chunk.end_ms);
            arm.ingest(&chunk).map(|_| true)
        }
    });
    match ingest {
        Err(error) => return vec![engine_failed(check, "ingest", error)],
        Ok(found) => failures.extend(found),
    }
    let Some(end_ms) = last_end_ms else {
        return vec![engine_failed(
            check,
            "data",
            EngineError("the probe has no data".into()),
        )];
    };
    match repeat_until_measured(clock, "evaluate", &mut || {
        arm.evaluate(end_ms).map(|_| true)
    }) {
        Err(error) => failures.push(engine_failed(check, "evaluate", error)),
        Ok(found) => failures.extend(found),
    }
    failures
}

/// Calls `call` until the calls have used [`OFF_THREAD_MIN_MEASURED_NS`] of
/// thread or process CPU, [`OFF_THREAD_MAX_CALLS`] calls ran, or `call`
/// returns `false` (nothing left to call it with). A `WorkOffCallingThread`
/// with the CPU per call when process CPU exceeds thread CPU by more than
/// 10%. Example: 20 ingests of 1 ms on the thread and 8 ms on another thread
/// report 9.0 ms of process CPU but 1.0 ms on the calling thread.
fn repeat_until_measured(
    clock: &dyn CpuClock,
    step: &str,
    call: &mut dyn FnMut() -> Result<bool, EngineError>,
) -> Result<Option<ConformanceFailure>, EngineError> {
    let (mut thread_cpu_ns, mut process_cpu_ns, mut calls) = (0_u64, 0_u64, 0_u64);
    while calls < OFF_THREAD_MAX_CALLS
        && thread_cpu_ns.max(process_cpu_ns) < OFF_THREAD_MIN_MEASURED_NS
    {
        let (thread_before, process_before) = (clock.thread_cpu_ns(), clock.process_cpu_ns());
        if !call()? {
            break;
        }
        thread_cpu_ns += clock.thread_cpu_ns().saturating_sub(thread_before);
        process_cpu_ns += clock.process_cpu_ns().saturating_sub(process_before);
        calls += 1;
    }
    let allowed_ns = thread_cpu_ns + (thread_cpu_ns as f64 * OFF_THREAD_TOLERANCE_FRACTION) as u64;
    Ok((calls > 0 && process_cpu_ns > allowed_ns).then(|| {
        ConformanceFailure::WorkOffCallingThread {
            step: step.into(),
            process_cpu_ns: process_cpu_ns / calls,
            thread_cpu_ns: thread_cpu_ns / calls,
        }
    }))
}

/// The probe workloads, each with PromQL and SQL texts (`$t` is the
/// evaluation time in ms): `probe` sampled every
/// 10 s from 0 (first sample at 10,000 ms), with `sum_over_time(probe[10s])`
/// and `count_over_time(probe[10s])` evaluated every 10 s; seed 7.
///
/// The first workload holds the sum query (id `sum`), the second the count
/// query (id `count`), so each workload's answers are one value per series.
pub fn probe_workloads() -> Vec<WorkloadSpec> {
    let data = DataRecord {
        metric: "probe".into(),
        description: TimeSeriesDescription {
            labels: vec![SeriesLabelSpec {
                name: "instance".into(),
                cardinality: 1,
                skewness: 0.0,
            }],
            series_num: 1,
            start_ms: 0,
            sample_interval_ms: PROBE_STEP_MS,
            duration_ms: 6 * PROBE_STEP_MS,
            value_distribution: DataDistribution::Uniform(UniformParameter {
                lower_bound: 0.0,
                upper_bound: 10.0,
                seed: 0,
            }),
            counter: false,
            seed: PROBE_SEED,
        },
    };
    [
        ("sum", "sum_over_time", "sum(value)"),
        ("count", "count_over_time", "count(*)"),
    ]
    .into_iter()
    .map(|(id, promql_function, sql_aggregate)| WorkloadSpec {
        name: format!("probe-{id}"),
        queries: vec![WorkloadQuery {
            id: id.into(),
            texts: vec![
                QueryText {
                    language: QueryLanguage::PromQl,
                    text: format!("{promql_function}(probe[10s])"),
                },
                QueryText {
                    language: QueryLanguage::Sql,
                    text: format!(
                        "SELECT instance, {sql_aggregate} FROM probe \
                             WHERE ts_ms > $t - 10000 AND ts_ms <= $t GROUP BY instance"
                    ),
                },
            ],
            window_ms: PROBE_STEP_MS,
            evaluation_interval_ms: PROBE_STEP_MS,
        }],
        data: vec![data.clone()],
    })
    .collect()
}

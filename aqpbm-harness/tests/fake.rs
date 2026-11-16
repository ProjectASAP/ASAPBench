//! The fake engine: step costs from the harness clock, a whole harness run,
//! node profiles from the operator timing hook, and engine-reported CPU.

use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use aqpbm_harness::arm_run::{run_arm, ArmSchedule};
use aqpbm_harness::conformance::probe_workloads;
use aqpbm_harness::data::{DataRecord, Sample, SampleChunk, SeriesLabels};
use aqpbm_harness::engine::{
    ArmSpec, CpuAccounting, EngineUnderTest, ProfilingMethod, QueryLanguage, QueryText, StepReport,
    WorkloadQuery, WorkloadSpec,
};
use aqpbm_harness::fake::FakeEngine;
use aqpbm_harness::protocol::{CpuClock, LoadProbe, Protocol, ProtocolError};
use aqpbm_harness::records::ArmResult;

/// A machine that is always idle.
struct IdleProbe;

impl LoadProbe for IdleProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        0.0
    }
}

fn arm() -> ArmSpec {
    ArmSpec {
        name: "exact".into(),
        raw_data_retained: true,
        engine_options: serde_json::Value::Null,
    }
}

/// `metric` with `series` series `s_0`..., one sample per second for
/// `duration_ms`.
fn data(metric: &str, series: u64, duration_ms: u64) -> DataRecord {
    DataRecord {
        metric: metric.into(),
        description: TimeSeriesDescription {
            labels: vec![SeriesLabelSpec {
                name: "s".into(),
                cardinality: series,
                skewness: 0.0,
            }],
            series_num: series,
            start_ms: 0,
            sample_interval_ms: 1_000,
            duration_ms,
            value_distribution: DataDistribution::Uniform(UniformParameter {
                lower_bound: 0.0,
                upper_bound: 10.0,
                seed: 0,
            }),
            counter: false,
            seed: 1,
        },
    }
}

/// One query `id` reading `sum_over_time(<metric>[<window>s])`, every 10 s.
fn sum_query(id: &str, metric: &str, window_s: u64) -> WorkloadQuery {
    WorkloadQuery {
        id: id.into(),
        texts: vec![QueryText {
            language: QueryLanguage::PromQl,
            text: format!("sum_over_time({metric}[{window_s}s])"),
        }],
        window_ms: window_s * 1_000,
        evaluation_interval_ms: 10_000,
    }
}

/// The samples of `series` series of `metric` at the given times, time-major.
fn chunk(metric: &str, series: u64, times_ms: impl Iterator<Item = i64>) -> SampleChunk {
    let mut samples = Vec::new();
    let mut end_ms = 0;
    for ts_ms in times_ms {
        for s in 0..series {
            let labels: SeriesLabels = [
                ("__name__".to_string(), metric.to_string()),
                ("s".to_string(), format!("s_{s}")),
            ]
            .into_iter()
            .collect();
            samples.push(Sample {
                labels,
                ts_ms,
                value: (s + 1) as f64,
            });
        }
        end_ms = ts_ms;
    }
    SampleChunk { end_ms, samples }
}

/// The probe workload of the conformance kit with its data seeded `seed`.
fn probe_workload(seed: u64) -> WorkloadSpec {
    let mut workload = probe_workloads().remove(0);
    for record in &mut workload.data {
        record.description.seed = seed;
    }
    workload
}

/// Every 10 s on the probe's sample grid, to its end.
fn probe_schedule(workload: &WorkloadSpec) -> ArmSchedule {
    let description = &workload.data[0].description;
    let evaluations = description.duration_ms / 10_000;
    ArmSchedule {
        refresh_ms: 10_000,
        evaluate_at_ms: (1..=evaluations as i64)
            .map(|k| description.start_ms + k * 10_000)
            .collect(),
    }
}

/// A harness run of `engine` on the probe workload seeded `seed`.
fn run_probe(engine: &FakeEngine, seed: u64) -> ArmResult {
    let workload = probe_workload(seed);
    let clock = engine.clock.clone();
    run_arm(
        engine,
        &workload,
        &arm(),
        &probe_schedule(&workload),
        &Protocol::default(),
        0,
        &mut IdleProbe,
        &clock,
    )
    .unwrap()
}

/// The middle of three values.
fn median_of_three(values: &[u64]) -> u64 {
    assert_eq!(values.len(), 3, "{values:?}");
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[1]
}

/// Through the protocol on the fake clock (50 ns per sample, 150 ns per
/// series, 10 µs per call): ingesting 1,000 samples (4 series × 250 s) costs
/// 60,000 ns and evaluating 4 series 10,600 ns, on the thread, process and
/// wall clocks alike. The engine's `StepReport` holds only scanned bytes and
/// its own CPU counter (none here): no time.
#[test]
fn step_costs_come_from_the_harness_clock() {
    let engine = FakeEngine::default();
    let clock = engine.clock.clone();
    let workload = WorkloadSpec {
        name: "w".into(),
        queries: vec![sum_query("q", "m", 300)],
        data: vec![data("m", 4, 250_000)],
    };
    let mut installed = engine.install(&workload, &arm()).unwrap();
    let samples = chunk("m", 4, (1..=250).map(|k| k * 1_000));
    assert_eq!(samples.samples.len(), 1_000);
    let protocol = Protocol {
        warmup_runs: 0,
        measured_runs: 1,
        max_core_busy_fraction: 0.10,
    };

    let ingest = protocol
        .run_job(0, &mut IdleProbe, &clock, &mut || {
            installed.ingest(&samples)
        })
        .unwrap();
    assert_eq!(ingest.median.thread_cpu_ns, Some(60_000));
    assert_eq!(ingest.median.process_cpu_ns, 60_000);
    assert_eq!(ingest.median.wall_ns, 60_000);

    let mut answered_series = 0;
    let evaluate = protocol
        .run_job(0, &mut IdleProbe, &clock, &mut || {
            let (answers, report) = installed.evaluate(250_000)?;
            answered_series = answers.0.len();
            Ok(report)
        })
        .unwrap();
    assert_eq!(answered_series, 4);
    assert_eq!(evaluate.median.thread_cpu_ns, Some(10_600));
    assert_eq!(evaluate.median.process_cpu_ns, 10_600);
    assert_eq!(evaluate.median.wall_ns, 10_600);

    // Exhaustive: a time field added to `StepReport` breaks this pattern.
    let StepReport {
        scanned_bytes: _,
        engine_reported_cpu_ns,
    } = &evaluate.median.report;
    assert_eq!(*engine_reported_cpu_ns, None);
}

/// Two harness runs of the probe workload with seed 7 serialize to the same
/// `ArmResult` JSON, byte for byte. Seed 8 changes the answers but not the
/// ingest and evaluate job costs.
#[test]
fn a_harness_run_of_the_fake_is_byte_identical_under_one_seed() {
    let first = run_probe(&FakeEngine::default(), 7);
    let second = run_probe(&FakeEngine::default(), 7);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );

    let reseeded = run_probe(&FakeEngine::default(), 8);
    assert_ne!(reseeded.answers, first.answers);
    assert_eq!(reseeded.ingest, first.ingest);
    assert_eq!(reseeded.evaluate, first.evaluate);
}

/// Two queries `sum_a` and `sum_b` over one metric `m` share one `scan` node.
/// After 3 evaluations each of `scan`, `sum_a` and `sum_b` appears once with
/// 3 per-batch samples, measured by the operator timing hook, and the node
/// medians add up to the evaluate median exactly on the fake clock.
#[test]
fn the_operator_timing_hook_counts_a_shared_node_once() {
    let engine = FakeEngine::default();
    let clock = engine.clock.clone();
    let workload = WorkloadSpec {
        name: "w".into(),
        queries: vec![sum_query("sum_a", "m", 10), sum_query("sum_b", "m", 10)],
        data: vec![data("m", 2, 30_000)],
    };
    let mut installed = engine.install(&workload, &arm()).unwrap();

    let mut evaluate_ns = Vec::new();
    for batch in 0..3_i64 {
        let start_ms = batch * 10_000;
        let samples = chunk("m", 2, (1..=10).map(|k| start_ms + k * 1_000));
        installed.ingest(&samples).unwrap();
        let before = clock.thread_cpu_ns();
        installed.evaluate(start_ms + 10_000).unwrap();
        evaluate_ns.push(clock.thread_cpu_ns() - before);
    }

    let profiles = installed
        .node_profiles()
        .expect("the fake has a timing hook");
    let mut nodes: Vec<&str> = profiles.iter().map(|p| p.node.as_str()).collect();
    nodes.sort_unstable();
    assert_eq!(nodes, ["scan", "sum_a", "sum_b"]);
    for profile in &profiles {
        assert_eq!(profile.method, ProfilingMethod::OperatorTimingHook);
        assert_eq!(profile.cpu_ns_per_batch.len(), 3, "{profile:?}");
    }
    let node_medians: u64 = profiles
        .iter()
        .map(|p| median_of_three(&p.cpu_ns_per_batch))
        .sum();
    assert_eq!(node_medians, median_of_three(&evaluate_ns));
}

/// A fake that keeps its own CPU counter (90 ns per step) and is measured as
/// processes (this test's pid): every evaluate sample of the harness run
/// carries the harness-measured process CPU (at least the 10 µs call cost on
/// the fake clock), no thread CPU, and the engine's own 90 ns beside them.
#[test]
fn engine_reported_cpu_is_recorded_beside_process_cpu() {
    let engine = FakeEngine {
        engine_reported_cpu_ns: Some(90),
        cpu_accounting: CpuAccounting::Processes(vec![std::process::id()]),
        ..FakeEngine::default()
    };
    let result = run_probe(&engine, 7);
    assert!(!result.evaluate.is_empty());
    for job in &result.evaluate {
        assert!(!job.samples.is_empty());
        for sample in &job.samples {
            assert!(sample.process_cpu_ns >= 10_000, "{sample:?}");
            assert_eq!(sample.thread_cpu_ns, None, "{sample:?}");
            assert_eq!(sample.report.engine_reported_cpu_ns, Some(90), "{sample:?}");
        }
    }
}

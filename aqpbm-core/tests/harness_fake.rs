//! The fake engine: step costs from the harness clock, a whole harness run,
//! node profiles from the operator timing hook, and engine-reported CPU.

use aqpbm_core::harness::arm_run::ArmRunError;
use aqpbm_core::harness::arm_run::{run_arm, ArmSchedule};
use aqpbm_core::harness::conformance::probe_workloads;
use aqpbm_core::harness::data::{DataRecord, Sample, SampleChunk, SeriesLabels};
use aqpbm_core::harness::engine::{
    ArmSpec, CpuAccounting, EngineUnderTest, QueryLanguage, QueryText, StepReport, WorkloadQuery,
    WorkloadSpec,
};
use aqpbm_core::harness::fake::{FakeEngine, FakeServer};
use aqpbm_core::harness::protocol::{
    CpuClock, EngineClock, LoadProbe, MeasurementClocks, Protocol, ProtocolError,
};
use aqpbm_core::harness::records::ArmResult;
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};

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
        truth: None,
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
                labels: labels.into(),
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
    let clock = engine.clock.clone();
    run_probe_on(
        engine,
        seed,
        &MeasurementClocks::with_calling_thread(&clock),
    )
    .unwrap()
}

/// [`run_probe`] reading `clocks`.
fn run_probe_on(
    engine: &FakeEngine,
    seed: u64,
    clocks: &MeasurementClocks,
) -> Result<ArmResult, ArmRunError> {
    let workload = probe_workload(seed);
    run_arm(
        engine,
        &workload,
        &arm(),
        &probe_schedule(&workload),
        &Protocol::default(),
        0,
        &mut IdleProbe,
        clocks,
    )
}

/// A fresh directory under the system temp dir for a fake `/proc`.
fn fake_proc_root(test: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "aqpbm-core-harness-fake-{}-{test}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    root
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
        .run_job(
            0,
            &mut IdleProbe,
            &EngineClock::CallingThread(&clock),
            &mut || installed.ingest(&samples),
        )
        .unwrap();
    assert_eq!(ingest.median.thread_cpu_ns, Some(60_000));
    assert_eq!(ingest.median.process_cpu_ns, 60_000);
    assert_eq!(ingest.median.wall_ns, 60_000);

    let mut answered_series = 0;
    let evaluate = protocol
        .run_job(
            0,
            &mut IdleProbe,
            &EngineClock::CallingThread(&clock),
            &mut || {
                let (answers, report) = installed.evaluate(250_000)?;
                answered_series = answers.iter().count();
                Ok(report)
            },
        )
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
        assert_eq!(profile.cpu_ns_per_batch.len(), 3, "{profile:?}");
    }
    let node_medians: u64 = profiles
        .iter()
        .map(|p| median_of_three(&p.cpu_ns_per_batch))
        .sum();
    assert_eq!(node_medians, median_of_three(&evaluate_ns));
}

/// A fake external server (pid 4242 under a fake `/proc` whose thread
/// `schedstat` advances with the server's work) that keeps its own CPU
/// counter (90 ns per step): every evaluate sample of the harness run
/// carries the server's process CPU read from that file, exactly the 10 µs call + 150 ns for
/// the probe's one series, no thread CPU, and the engine's own 90 ns beside
/// them. The harness's own clock (the fake clock passed as the calling-thread
/// clock) never moves, so a run that read it instead would record 0.
#[test]
fn engine_reported_cpu_is_recorded_beside_process_cpu() {
    let root = fake_proc_root("server");
    let server = FakeServer::start(&root, 4242).unwrap();
    let engine = FakeEngine {
        engine_reported_cpu_ns: Some(90),
        server: Some(server.clone()),
        ..FakeEngine::default()
    };
    assert_eq!(
        engine.cpu_accounting(),
        CpuAccounting::Processes(vec![4242])
    );
    let clock = engine.clock.clone();
    let clocks = MeasurementClocks {
        calling_thread: &clock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let result = run_probe_on(&engine, 7, &clocks).unwrap();
    assert!(!result.evaluate.is_empty());
    for job in &result.evaluate {
        assert!(!job.samples.is_empty());
        for sample in &job.samples {
            assert_eq!(sample.process_cpu_ns, 10_150, "{sample:?}");
            assert_eq!(sample.thread_cpu_ns, None, "{sample:?}");
            assert_eq!(sample.report.engine_reported_cpu_ns, Some(90), "{sample:?}");
        }
    }
    for job in &result.ingest {
        // 10 µs per call + 50 ns for the chunk's one sample.
        assert_eq!(job.median.process_cpu_ns, 10_050, "{job:?}");
    }
    assert!(server.cpu_ns() > 0);
    assert_eq!(clock.process_cpu_ns(), 0, "the harness clock moved");

    // A server pid that has no stat file under the `/proc` read is refused
    // before any install.
    std::fs::remove_file(root.join("4242/stat")).unwrap();
    let installs_before = engine.installs.get();
    let error = run_probe_on(&engine, 7, &clocks).unwrap_err();
    assert!(
        matches!(
            error,
            ArmRunError::Protocol(ProtocolError::MissingProcess { pid: 4242, .. })
        ),
        "{error:?}"
    );
    assert_eq!(engine.installs.get(), installs_before);
    let _ = std::fs::remove_dir_all(&root);
}

/// A load probe that is always idle and counts its busy-fraction samples.
struct CountingProbe(usize);

impl LoadProbe for CountingProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        self.0 += 1;
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        f64::NAN
    }
}

/// A run of the probe workload (6 chunks, 6 evaluations: 12 jobs) samples the
/// core 13 times, not 24: each job's after-sample is the next job's
/// before-sample. An unreadable load average (`NaN`) is recorded as `None`,
/// and the record reads back from its JSON.
#[test]
fn consecutive_jobs_share_one_idle_sample() {
    let engine = FakeEngine::default();
    let clock = engine.clock.clone();
    let workload = probe_workload(7);
    let mut probe = CountingProbe(0);
    let result = run_arm(
        &engine,
        &workload,
        &arm(),
        &probe_schedule(&workload),
        &Protocol::default(),
        0,
        &mut probe,
        &MeasurementClocks::with_calling_thread(&clock),
    )
    .unwrap();
    let jobs = result.ingest.len() + result.evaluate.len();
    assert_eq!(jobs, 12);
    assert_eq!(probe.0, jobs + 1);
    assert_eq!(result.ingest[0].loadavg, [None, None]);
    let text = serde_json::to_string(&result).unwrap();
    assert_eq!(serde_json::from_str::<ArmResult>(&text).unwrap(), result);
}

/// A series that already carries a `query` label keeps it: answers are keyed
/// by query id outside the labels, so no label name is reserved. Query `q`
/// over `m{query="x", s="s_0"}` answers under `"q" -> {query="x",
/// s="s_0"}` (a range function drops `__name__`).
#[test]
fn a_series_label_named_query_is_kept() {
    let engine = FakeEngine::default();
    let workload = WorkloadSpec {
        name: "w".into(),
        queries: vec![sum_query("q", "m", 10)],
        data: vec![data("m", 1, 10_000)],
    };
    let mut installed = engine.install(&workload, &arm()).unwrap();
    let labels: SeriesLabels = [("__name__", "m"), ("query", "x"), ("s", "s_0")]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let samples = SampleChunk {
        end_ms: 10_000,
        samples: vec![Sample {
            labels: labels.clone().into(),
            ts_ms: 10_000,
            value: 2.5,
        }],
    };
    installed.ingest(&samples).unwrap();
    let (answers, _) = installed.evaluate(10_000).unwrap();
    let mut output_labels = labels.clone();
    output_labels.remove("__name__");
    assert_eq!(answers.get("q", &output_labels), Some(2.5));
    assert_eq!(answers.iter().count(), 1);
}

/// A workload whose data last 0 ms is refused before any install: its cost
/// per second of data would be infinite.
#[test]
fn a_workload_without_data_is_refused() {
    let engine = FakeEngine::default();
    let clock = engine.clock.clone();
    let workload = WorkloadSpec {
        name: "empty".into(),
        queries: vec![sum_query("q", "m", 10)],
        data: Vec::new(),
    };
    let error = run_arm(
        &engine,
        &workload,
        &arm(),
        &ArmSchedule {
            refresh_ms: 10_000,
            evaluate_at_ms: Vec::new(),
        },
        &Protocol::default(),
        0,
        &mut IdleProbe,
        &MeasurementClocks::with_calling_thread(&clock),
    )
    .unwrap_err();
    assert_eq!(
        error,
        ArmRunError::NoData {
            workload: "empty".into()
        }
    );
    assert_eq!(engine.installs.get(), 0);
}

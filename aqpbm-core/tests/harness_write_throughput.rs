use std::sync::{Arc, Mutex};
use std::time::Duration;

use aqpbm_core::harness::data::{DataRecord, Sample};
use aqpbm_core::harness::engine::{
    Answers, ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, Footprint,
    InstalledArm, PlanSummary, SampleChunk, SampleWriter, StepReport, WorkloadSpec,
};
use aqpbm_core::harness::fake::{FakeEngine, FakeServer};
use aqpbm_core::harness::protocol::{MeasurementClocks, OsCpuClock, ProcessesMemory};
use aqpbm_core::harness::run_directory::RunDirectory;
use aqpbm_core::harness::write_throughput::{
    run_write_throughput, WriteLoad, WriteThroughputError, WriteThroughputResult,
};
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};

struct TempRoot(std::path::PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn proc_root(test: &str) -> TempRoot {
    let root = std::env::temp_dir().join(format!(
        "aqpbm-core-harness-write-throughput-{}-{test}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    TempRoot(root)
}

fn workload(series: u64, duration_ms: u64) -> WorkloadSpec {
    WorkloadSpec {
        name: "writes".into(),
        queries: Vec::new(),
        data: vec![DataRecord {
            metric: "bench_metric".into(),
            description: TimeSeriesDescription {
                labels: vec![
                    SeriesLabelSpec {
                        name: "l0".into(),
                        cardinality: 10,
                        skewness: 0.0,
                    },
                    SeriesLabelSpec {
                        name: "l1".into(),
                        cardinality: series.div_ceil(10),
                        skewness: 0.0,
                    },
                ],
                series_num: series,
                start_ms: 0,
                sample_interval_ms: 1_000,
                duration_ms,
                value_distribution: DataDistribution::Uniform(UniformParameter {
                    lower_bound: 0.0,
                    upper_bound: 10_000.0,
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

fn arm() -> ArmSpec {
    ArmSpec {
        name: "write-only".into(),
        raw_data_retained: true,
        engine_options: serde_json::json!({}),
    }
}

fn load(writers: usize, target: Option<f64>) -> WriteLoad {
    WriteLoad {
        writers,
        samples_per_request: 10,
        target_samples_per_second: target,
        max_event_time_lead_ms: 2_000,
        warmup_ms: 100,
        measure_ms: 500,
        stop_grace_ms: 5_000,
    }
}

fn fake_engine(test: &str) -> (FakeEngine, FakeServer, TempRoot) {
    let root = proc_root(test);
    let server = FakeServer::start(&root.0, 4242).unwrap();
    let engine = FakeEngine {
        server: Some(server.clone()),
        ..FakeEngine::default()
    };
    (engine, server, root)
}

#[test]
fn an_open_loop_run_reports_the_acknowledged_rate_cpu_and_process_memory() {
    let (engine, server, _root) = fake_engine("open-loop");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let result = run_write_throughput(
        &engine,
        &workload(40, 1_000_000_000),
        &arm(),
        &load(4, None),
        &clocks,
    )
    .unwrap();
    assert_eq!(result.series, 40);
    assert_eq!(result.workload, "writes");
    assert_eq!(result.arm, "write-only");
    assert!(result.acknowledged_samples_per_second > 0.0, "{result:?}");
    assert!(result.measured_ms >= 500.0, "{result:?}");
    assert_eq!(result.engine_cpu_cores, 0.0);
    assert_eq!(result.cpu_source, "schedstat");
    assert!(
        (0..=2_000).contains(&result.widest_event_time_lead_ms),
        "{result:?}"
    );
    assert_eq!(
        result.server_process_memory_after,
        Some(ProcessesMemory {
            proportional_set_size_bytes: (2048 + 1024) * 1024,
            peak_resident_set_size_bytes: Some((2048 + 1024) * 1024),
        })
    );
    assert_eq!(result.server_memory_after, None);

    let run_root = proc_root("open-loop-run");
    let run = RunDirectory::create_at(&run_root.0.join("run")).unwrap();
    let path = run.append_write_throughput(&result).unwrap();
    run.append_write_throughput(&result).unwrap();
    assert_eq!(path, run.path().join("results/fake/write-throughput.jsonl"));
    let line = serde_json::to_string(&result).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{line}\n{line}\n")
    );
    let read: WriteThroughputResult = serde_json::from_str(&line).unwrap();
    assert_eq!(read.engine, result.engine);
    assert_eq!(read.load, result.load);
}

#[test]
fn a_rate_limited_run_holds_its_target() {
    let (engine, server, _root) = fake_engine("rate-limited");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let target = 20_000.0;
    let mut load = load(4, Some(target));
    load.measure_ms = 1_000;
    let result = run_write_throughput(
        &engine,
        &workload(40, 1_000_000_000),
        &arm(),
        &load,
        &clocks,
    )
    .unwrap();
    let achieved = result.acknowledged_samples_per_second;
    assert!(
        (achieved - target).abs() < 0.1 * target,
        "achieved {achieved} for target {target}"
    );
}

#[test]
fn an_engine_measured_on_the_calling_thread_is_refused() {
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    let error = run_write_throughput(
        &FakeEngine::default(),
        &workload(40, 1_000_000),
        &arm(),
        &load(4, None),
        &clocks,
    )
    .unwrap_err();
    assert!(matches!(error, WriteThroughputError::Load(_)), "{error:?}");
    assert!(error.to_string().contains("harness process"), "{error}");
}

#[test]
fn a_load_with_no_writers_is_refused() {
    let (engine, server, _root) = fake_engine("no-writers");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let error = run_write_throughput(
        &engine,
        &workload(40, 1_000_000),
        &arm(),
        &load(0, None),
        &clocks,
    )
    .unwrap_err();
    assert!(error.to_string().contains("writers"), "{error}");
}

#[test]
fn a_workload_that_runs_out_of_samples_is_an_error() {
    let (engine, server, _root) = fake_engine("runs-out");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let error = run_write_throughput(
        &engine,
        &workload(40, 10_000),
        &arm(),
        &load(4, None),
        &clocks,
    )
    .unwrap_err();
    assert!(error.to_string().contains("ran out of samples"), "{error}");
}

struct RecordingEngine {
    server: FakeServer,
    _root: TempRoot,
    writes: Arc<Mutex<Writes>>,
    slow_writer: usize,
    failing_writer: Option<usize>,
    panicking_writer: Option<usize>,
    hanging_writer: Option<usize>,
    gives_writers: bool,
}

#[derive(Default)]
struct Writes {
    latest_ms: Vec<i64>,
    widest_spread_ms: i64,
    samples: u64,
    batches_with_several_timestamps: u64,
    writers_of_series: std::collections::BTreeMap<String, std::collections::BTreeSet<usize>>,
}

struct RecordingArm {
    engine_writes: Arc<Mutex<Writes>>,
    slow_writer: usize,
    failing_writer: Option<usize>,
    panicking_writer: Option<usize>,
    hanging_writer: Option<usize>,
    gives_writers: bool,
}

struct RecordingWriter {
    index: usize,
    writes: Arc<Mutex<Writes>>,
    slow: bool,
    failing: bool,
    panicking: bool,
    hanging: bool,
}

impl SampleWriter for RecordingWriter {
    fn write(&mut self, samples: &[Sample]) -> Result<(), EngineError> {
        if self.failing {
            return Err(EngineError("writer refused the samples".into()));
        }
        if self.panicking {
            panic!("writer {} panics", self.index);
        }
        if self.hanging {
            std::thread::sleep(Duration::from_secs(60));
        }
        {
            let mut writes = self.writes.lock().unwrap();
            writes.latest_ms[self.index] = samples[0].ts_ms;
            let spread =
                writes.latest_ms.iter().max().unwrap() - writes.latest_ms.iter().min().unwrap();
            writes.widest_spread_ms = writes.widest_spread_ms.max(spread);
            writes.samples += samples.len() as u64;
            for sample in samples {
                writes
                    .writers_of_series
                    .entry(format!("{:?}", sample.labels))
                    .or_default()
                    .insert(self.index);
            }
            if samples
                .iter()
                .any(|sample| sample.ts_ms != samples[0].ts_ms)
            {
                writes.batches_with_several_timestamps += 1;
            }
        }
        if self.slow {
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
}

impl EngineUnderTest for RecordingEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "recording".into(),
            version: "test".into(),
            version_dirty: false,
            build_profile: "test".into(),
        }
    }

    fn cpu_accounting(&self) -> CpuAccounting {
        CpuAccounting::Processes(vec![self.server.pid])
    }

    fn install(
        &self,
        _workload: &WorkloadSpec,
        _arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        Ok(Box::new(RecordingArm {
            engine_writes: Arc::clone(&self.writes),
            slow_writer: self.slow_writer,
            failing_writer: self.failing_writer,
            panicking_writer: self.panicking_writer,
            hanging_writer: self.hanging_writer,
            gives_writers: self.gives_writers,
        }))
    }
}

impl InstalledArm for RecordingArm {
    fn plan_summary(&self) -> PlanSummary {
        PlanSummary::default()
    }

    fn ingest(&mut self, _chunk: &SampleChunk) -> Result<StepReport, EngineError> {
        Err(EngineError("writes only through sample writers".into()))
    }

    fn evaluate(&mut self, _at_ms: i64) -> Result<(Answers, StepReport), EngineError> {
        Err(EngineError("answers no queries".into()))
    }

    fn footprint(&self) -> Result<Footprint, EngineError> {
        Ok(Footprint::default())
    }

    fn sample_writers(&mut self, count: usize) -> Option<Vec<Box<dyn SampleWriter>>> {
        if !self.gives_writers {
            return None;
        }
        self.engine_writes.lock().unwrap().latest_ms = vec![0; count];
        Some(
            (0..count)
                .map(|index| {
                    Box::new(RecordingWriter {
                        index,
                        writes: Arc::clone(&self.engine_writes),
                        slow: index == self.slow_writer,
                        failing: Some(index) == self.failing_writer,
                        panicking: Some(index) == self.panicking_writer,
                        hanging: Some(index) == self.hanging_writer,
                    }) as Box<dyn SampleWriter>
                })
                .collect(),
        )
    }
}

fn recording_engine(test: &str) -> RecordingEngine {
    let root = proc_root(test);
    RecordingEngine {
        server: FakeServer::start(&root.0, 4343).unwrap(),
        _root: root,
        writes: Arc::default(),
        slow_writer: usize::MAX,
        failing_writer: None,
        panicking_writer: None,
        hanging_writer: None,
        gives_writers: true,
    }
}

#[test]
fn a_slow_writer_holds_the_others_within_the_event_time_lead() {
    let mut engine = recording_engine("lead");
    engine.slow_writer = 0;
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: engine.server.cpu_files.clone(),
    };
    let mut load = load(4, None);
    load.samples_per_request = 1_000;
    run_write_throughput(
        &engine,
        &workload(40, 1_000_000_000),
        &arm(),
        &load,
        &clocks,
    )
    .unwrap();
    let writes = engine.writes.lock().unwrap();
    assert!(writes.samples > 0);
    assert_eq!(writes.batches_with_several_timestamps, 0);
    assert!(
        writes.widest_spread_ms <= 2_000 + 1_000,
        "widest spread {} ms",
        writes.widest_spread_ms
    );
}

#[test]
fn a_failing_writer_fails_the_run_with_its_error() {
    let mut engine = recording_engine("failing");
    engine.failing_writer = Some(2);
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: engine.server.cpu_files.clone(),
    };
    let error = run_write_throughput(
        &engine,
        &workload(40, 1_000_000_000),
        &arm(),
        &load(4, None),
        &clocks,
    )
    .unwrap_err();
    assert_eq!(
        error,
        WriteThroughputError::Engine(EngineError("writer refused the samples".into()))
    );
}

#[test]
fn an_engine_without_sample_writers_is_refused() {
    let mut engine = recording_engine("no-sample-writers");
    engine.gives_writers = false;
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: engine.server.cpu_files.clone(),
    };
    let error = run_write_throughput(
        &engine,
        &workload(40, 1_000_000),
        &arm(),
        &load(4, None),
        &clocks,
    )
    .unwrap_err();
    assert!(error.to_string().contains("no sample writers"), "{error}");
}

fn long_load(writers: usize) -> WriteLoad {
    WriteLoad {
        warmup_ms: 3_000,
        measure_ms: 5_000,
        ..load(writers, None)
    }
}

#[test]
fn a_panicking_writer_fails_the_run_at_once_with_its_message() {
    let mut engine = recording_engine("panicking");
    engine.panicking_writer = Some(1);
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: engine.server.cpu_files.clone(),
    };
    let started = std::time::Instant::now();
    let error = run_write_throughput(
        &engine,
        &workload(40, 1_000_000_000),
        &arm(),
        &long_load(4),
        &clocks,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("writer 1 panicked: writer 1 panics"),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_writer_that_never_returns_fails_the_run_after_the_stop_grace() {
    let mut engine = recording_engine("hanging");
    engine.hanging_writer = Some(0);
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: engine.server.cpu_files.clone(),
    };
    let load = WriteLoad {
        stop_grace_ms: 300,
        ..load(4, None)
    };
    let started = std::time::Instant::now();
    let error = run_write_throughput(
        &engine,
        &workload(40, 1_000_000_000),
        &arm(),
        &load,
        &clocks,
    )
    .unwrap_err();
    assert!(error.to_string().contains("still running"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn a_writer_whose_series_end_first_ends_the_run_at_once_with_an_error() {
    let (engine, server, _root) = fake_engine("slice-ends-first");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let mut workload = workload(20, 10_000);
    let mut long = workload.data[0].clone();
    long.metric = "long_metric".into();
    long.description.duration_ms = 1_000_000_000_000;
    workload.data.push(long);
    let started = std::time::Instant::now();
    let error =
        run_write_throughput(&engine, &workload, &arm(), &long_load(2), &clocks).unwrap_err();
    assert!(error.to_string().contains("ran out of samples"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_writer_with_part_of_its_series_ending_first_ends_the_run_with_an_error() {
    let (engine, server, _root) = fake_engine("part-ends-first");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let mut long = workload(30, 1_000_000_000_000).data.remove(0);
    long.metric = "long_metric".into();
    let mut workload = workload(10, 10_000);
    workload.data.push(long);
    let started = std::time::Instant::now();
    let error =
        run_write_throughput(&engine, &workload, &arm(), &long_load(2), &clocks).unwrap_err();
    assert!(error.to_string().contains("ran out of samples"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn metrics_with_different_starts_are_refused() {
    let (engine, server, _root) = fake_engine("different-starts");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let mut workload = workload(20, 1_000_000_000);
    let mut late = workload.data[0].clone();
    late.metric = "late_metric".into();
    late.description.start_ms = 1_000_000_000;
    workload.data.push(late);
    let error =
        run_write_throughput(&engine, &workload, &arm(), &load(2, None), &clocks).unwrap_err();
    assert!(error.to_string().contains("same start"), "{error}");
}

#[test]
fn a_cgroup_engine_reports_its_cgroup_cpu_source_and_memory() {
    let root = proc_root("cgroup");
    let server = FakeServer::start_in_cgroup(&root.0, 4545, &root.0.join("group")).unwrap();
    let engine = FakeEngine {
        server: Some(server.clone()),
        ..FakeEngine::default()
    };
    assert!(matches!(engine.cpu_accounting(), CpuAccounting::Cgroup(_)));
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let result = run_write_throughput(
        &engine,
        &workload(40, 1_000_000_000),
        &arm(),
        &load(4, None),
        &clocks,
    )
    .unwrap();
    assert!(
        result.cpu_source.starts_with("cgroup"),
        "{}",
        result.cpu_source
    );
    assert_eq!(
        result
            .server_memory_after
            .map(|memory| memory.current_bytes),
        Some(3_145_728)
    );
    assert_eq!(result.server_process_memory_after, None);
}

#[test]
fn writers_split_the_series_of_several_metrics_between_them() {
    let mut engine = recording_engine("several-metrics");
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: engine.server.cpu_files.clone(),
    };
    let mut workload = workload(20, 1_000_000_000);
    let mut other = workload.data[0].clone();
    other.metric = "other_metric".into();
    other.description.series_num = 10;
    workload.data.push(other);
    engine.slow_writer = usize::MAX;
    let result = run_write_throughput(&engine, &workload, &arm(), &load(3, None), &clocks).unwrap();
    assert_eq!(result.series, 30);
    let writes = engine.writes.lock().unwrap();
    assert_eq!(writes.writers_of_series.len(), 30);
    assert!(writes
        .writers_of_series
        .values()
        .all(|writers| writers.len() == 1));
    let per_writer: Vec<usize> = (0..3)
        .map(|writer| {
            writes
                .writers_of_series
                .values()
                .filter(|writers| writers.contains(&writer))
                .count()
        })
        .collect();
    assert_eq!(per_writer, vec![10, 10, 10]);
}

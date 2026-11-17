//! The run directory and its JSON records.

use std::path::{Path, PathBuf};

use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use aqpbm_harness::data::DataRecord;
use aqpbm_harness::engine::{EngineDescription, Footprint, PlanSummary, StepCost};
use aqpbm_harness::machine::MachineDescription;
use aqpbm_harness::protocol::{JobRecord, Protocol};
use aqpbm_harness::records::{calibration_staleness, ArmResult, RunRecord};
use aqpbm_harness::rundir::{RunDir, RunDirError};
use chrono::{DateTime, TimeZone, Utc};

const E5_KEY: &str = "x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4";

/// A directory under the system temp dir, unique per process and test,
/// removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aqpbm-harness-rundir-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 2026-11-16 20:15:00 UTC.
fn utc() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, 20, 15, 0).unwrap()
}

fn create(root: &Path) -> Result<RunDir, RunDirError> {
    RunDir::create(root, E5_KEY, utc(), "sweep", "sketch-benefit")
}

fn fake_engine(version: &str, version_dirty: bool) -> EngineDescription {
    EngineDescription {
        id: "fake".into(),
        version: version.into(),
        version_dirty,
        build_profile: "release".into(),
    }
}

fn run_record() -> RunRecord {
    RunRecord {
        args: vec![
            "aqpbm".into(),
            "sweep".into(),
            "--label".into(),
            "sketch-benefit".into(),
        ],
        harness_version: "0.1.0".into(),
        machine: MachineDescription {
            cpu_architecture: "x86_64".into(),
            cpu_model: "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz".into(),
            logical_cores: 56,
            memory_type: "Registered-DDR4".into(),
            memory_kib: 263_724_000,
            os: "linux".into(),
        },
        engines: vec![fake_engine("0.1.0", false)],
        protocol: Protocol {
            warmup_runs: 1,
            measured_runs: 5,
            max_core_busy_fraction: 0.10,
        },
    }
}

fn arm_result(workload: &str) -> ArmResult {
    ArmResult {
        engine: "fake".into(),
        workload: workload.into(),
        arm: "planned".into(),
        plan: PlanSummary::default(),
        predicted: None,
        ingest: Vec::new(),
        evaluate: Vec::new(),
        answers: Vec::new(),
        footprint: Footprint::default(),
        measured_engine_cost_per_second: None,
        node_profiles: None,
        ingested_samples: 0,
        server_memory_before: None,
        server_memory_after: None,
        server_cpu_ns_whole_arm: None,
        installs_held: 1,
    }
}

fn job(core: usize, busy_before: f64, busy_after: f64, loadavg: [f64; 2]) -> JobRecord {
    JobRecord {
        core,
        busy_before,
        busy_after,
        loadavg: loadavg.map(Some),
        marks: Vec::new(),
        samples: vec![StepCost::default()],
        median: StepCost::default(),
    }
}

/// A run made at 2026-11-16 20:15:00 UTC by `sweep` with label
/// `sketch-benefit` on the E5 machine is
/// `<root>/runs/x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4/20261116T201500Z-sweep-sketch-benefit/`,
/// created with `workloads/`, `data/`, `plans/` and `results/`.
/// Adapted from the ASAPPlanner test `gate_output_files`.
#[test]
fn run_dir_is_machine_key_utc_subcommand_label() {
    let root = TempDir::new("run_dir_is_machine_key_utc_subcommand_label");
    let run = create(&root.0).unwrap();
    let expected = root
        .0
        .join("runs")
        .join(E5_KEY)
        .join("20261116T201500Z-sweep-sketch-benefit");
    assert_eq!(run.path(), expected);
    assert!(run.path().is_dir(), "{:?}", run.path());
    for sub in ["workloads", "data", "plans", "results"] {
        assert!(
            run.path().join(sub).is_dir(),
            "{sub} missing in {:?}",
            run.path()
        );
    }
}

/// The label `Sketch Benefit/1` is refused: the error names the label and the
/// charset `[a-z0-9-]`, and nothing is created under the root.
#[test]
fn a_label_outside_the_slug_charset_is_refused() {
    let root = TempDir::new("a_label_outside_the_slug_charset_is_refused");
    let error = RunDir::create(&root.0, E5_KEY, utc(), "sweep", "Sketch Benefit/1").unwrap_err();
    assert!(
        matches!(&error, RunDirError::BadName { name, .. } if name == "Sketch Benefit/1"),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("Sketch Benefit/1"), "{message}");
    assert!(message.contains("[a-z0-9-]"), "{message}");
    assert!(!root.0.join("runs").exists());
}

/// Creating the same run directory twice fails with `RunDirExists` naming
/// it, and the first run's `run.json` stays byte for byte as written.
#[test]
fn an_existing_run_dir_is_never_overwritten() {
    let root = TempDir::new("an_existing_run_dir_is_never_overwritten");
    let first = create(&root.0).unwrap();
    first.write_run(&run_record()).unwrap();
    let run_json = first.path().join("run.json");
    let before = std::fs::read(&run_json).unwrap();

    let error = create(&root.0).unwrap_err();
    assert!(
        matches!(&error, RunDirError::RunDirExists(path) if *path == first.path()),
        "{error:?}"
    );
    assert_eq!(std::fs::read(&run_json).unwrap(), before);
}

/// `run.json` (args, harness version, the E5 machine, the fake engine and the
/// protocol warm-up 1 / runs 5 / busy limit 0.10) reads back equal; with an
/// added `"extra": 1` it is refused, naming `extra`.
#[test]
fn run_json_round_trips_and_refuses_unknown_fields() {
    let root = TempDir::new("run_json_round_trips_and_refuses_unknown_fields");
    let run = create(&root.0).unwrap();
    let record = run_record();
    run.write_run(&record).unwrap();
    let text = std::fs::read_to_string(run.path().join("run.json")).unwrap();
    let read: RunRecord = serde_json::from_str(&text).unwrap();
    assert_eq!(read, record);

    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("extra".into(), serde_json::json!(1));
    let error = serde_json::from_value::<RunRecord>(value).unwrap_err();
    assert!(error.to_string().contains("extra"), "{error}");
}

/// The result of engine `fake`, workload `w`, arm `planned` is filed at
/// `results/fake/w__planned.json`; a workload named `a__b` is refused as
/// ambiguous, and no file is written for it.
#[test]
fn arm_results_are_filed_per_engine_workload_and_arm() {
    let root = TempDir::new("arm_results_are_filed_per_engine_workload_and_arm");
    let run = create(&root.0).unwrap();
    let path = run.write_arm(&arm_result("w")).unwrap();
    assert_eq!(path, run.path().join("results/fake/w__planned.json"));
    assert!(path.is_file(), "{path:?}");

    let error = run.write_arm(&arm_result("a__b")).unwrap_err();
    assert!(
        matches!(&error, RunDirError::AmbiguousName { name, .. } if name == "a__b"),
        "{error:?}"
    );
    assert!(!run.path().join("results/fake/a__b__planned.json").exists());
}

/// Two jobs, on cores 3 and 4, give two `status.log` lines, each with the job
/// name, `core <n>`, `busy-before`/`busy-after` with their fractions, and the
/// load averages.
#[test]
fn status_log_has_one_line_per_job() {
    let root = TempDir::new("status_log_has_one_line_per_job");
    let run = create(&root.0).unwrap();
    run.append_status("ingest-0", &job(3, 0.02, 0.03, [0.5, 0.75]))
        .unwrap();
    run.append_status("evaluate-0", &job(4, 0.04, 0.06, [1.25, 1.5]))
        .unwrap();

    let log = std::fs::read_to_string(run.path().join("status.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    let expected = [
        [
            "ingest-0",
            "core 3",
            "busy-before 0.02",
            "busy-after 0.03",
            "0.5",
            "0.75",
        ],
        [
            "evaluate-0",
            "core 4",
            "busy-before 0.04",
            "busy-after 0.06",
            "1.25",
            "1.5",
        ],
    ];
    for (line, needles) in lines.iter().zip(expected) {
        for needle in needles {
            assert!(line.contains(needle), "{needle} not in {line}");
        }
    }
}

/// A calibration measured on clean `8e65fca45` is stale on `a1b2c3d4e`: the
/// warning names both versions and `calibrate`. On a dirty `8e65fca45` the
/// warning says dirty. On the identical clean engine there is no warning.
#[test]
fn a_stale_calibration_is_named() {
    let recorded = fake_engine("8e65fca45", false);

    let warning = calibration_staleness(&recorded, &fake_engine("a1b2c3d4e", false))
        .expect("a different version is stale");
    for needle in ["8e65fca45", "a1b2c3d4e", "calibrate"] {
        assert!(warning.contains(needle), "{needle} not in {warning}");
    }

    let warning = calibration_staleness(&recorded, &fake_engine("8e65fca45", true))
        .expect("a dirty engine is stale");
    assert!(warning.contains("dirty"), "{warning}");

    assert_eq!(calibration_staleness(&recorded, &recorded.clone()), None);
}

/// A result file is never overwritten: writing the arm `w`/`planned` twice
/// fails the second time and keeps the first file.
#[test]
fn a_result_file_is_never_overwritten() {
    let root = TempDir::new("a_result_file_is_never_overwritten");
    let run = create(&root.0).unwrap();
    let path = run.write_arm(&arm_result("w")).unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    let error = run.write_arm(&arm_result("w")).unwrap_err();
    assert!(
        matches!(&error, RunDirError::Io { path: p, source } if *p == path
            && source.kind() == std::io::ErrorKind::AlreadyExists),
        "{error:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
}

/// A calibration measured on a dirty `8e65fca45` is stale on a clean
/// `8e65fca45`, and the warning says the calibration's build was dirty, not
/// the current engine.
#[test]
fn a_calibration_from_a_dirty_build_names_the_recorded_build() {
    let warning = calibration_staleness(
        &fake_engine("8e65fca45", true),
        &fake_engine("8e65fca45", false),
    )
    .expect("a dirty recorded build is stale");
    assert!(
        warning.contains("the calibration was measured on a dirty build"),
        "{warning}"
    );
    assert!(
        !warning.contains("the engine is a dirty build"),
        "{warning}"
    );
}

/// A data record is written to `data/<metric>.datagen.json` and reads back
/// equal; a metric name with `/` (which would leave `data/`) is refused by
/// name.
#[test]
fn a_data_record_is_filed_under_data_by_metric() {
    let tmp = TempDir::new("write-data");
    let run = create(&tmp.0).unwrap();
    let record = DataRecord {
        metric: "node_cpu:rate".into(),
        description: TimeSeriesDescription {
            labels: vec![SeriesLabelSpec {
                name: "instance".into(),
                cardinality: 2,
                skewness: 0.0,
            }],
            series_num: 2,
            start_ms: 0,
            sample_interval_ms: 1_000,
            duration_ms: 60_000,
            value_distribution: DataDistribution::Uniform(UniformParameter {
                lower_bound: 0.0,
                upper_bound: 1.0,
                seed: 0,
            }),
            counter: false,
            seed: 1,
        },
    };
    let path = run.write_data(&record).unwrap();
    assert_eq!(
        path,
        run.path().join("data").join("node_cpu:rate.datagen.json")
    );
    let read_back: DataRecord =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(read_back, record);

    let escaping = DataRecord {
        metric: "../m".into(),
        ..record
    };
    let error = run.write_data(&escaping).unwrap_err();
    assert!(
        matches!(&error, RunDirError::BadName { name, .. } if name == "../m"),
        "{error:?}"
    );
}

/// `create_at` makes the given directory with the four subdirectories and
/// refuses an existing one; `open` accepts it and refuses a directory
/// missing `plans/`.
#[test]
fn create_at_and_open_a_run_directory_shared_by_lanes() {
    let temp = TempDir::new("create-at");
    let path = temp.0.join("goal").join("run-1");
    let run = RunDir::create_at(&path).unwrap();
    assert_eq!(run.path(), path.as_path());
    for sub in ["workloads", "data", "plans", "results"] {
        assert!(path.join(sub).is_dir(), "{sub}");
    }
    assert!(matches!(
        RunDir::create_at(&path),
        Err(RunDirError::RunDirExists(existing)) if existing == path
    ));
    assert_eq!(RunDir::open(&path).unwrap().path(), path.as_path());
    std::fs::remove_dir(path.join("plans")).unwrap();
    match RunDir::open(&path) {
        Err(RunDirError::NotARunDir { missing, .. }) => assert_eq!(missing, "plans"),
        other => panic!("expected NotARunDir, got {other:?}"),
    }
}

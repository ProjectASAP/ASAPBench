//! Lanes over one run directory and the report that joins them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use aqpbm_harness::bench_external::{
    read_variants, read_workloads, report, run_lane, write_report, CellOutcome, LaneCell,
    LaneSetup, SUMMARY_CSV_HEADER,
};
use aqpbm_harness::cgroup::CgroupMemory;
use aqpbm_harness::data::{DataRecord, SeriesLabels};
use aqpbm_harness::engine::{
    Answers, Footprint, PlanSummary, QueryLanguage, QueryText, StepCost, StepReport, WorkloadQuery,
    WorkloadSpec,
};
use aqpbm_harness::fake::FakeEngine;
use aqpbm_harness::protocol::{JobRecord, LoadProbe, MeasurementClocks, Protocol, ProtocolError};
use aqpbm_harness::records::ArmResult;
use aqpbm_harness::rundir::RunDir;

struct TempDir(PathBuf);

impl TempDir {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aqpbm-harness-bench-external-{}-{test}",
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

struct IdleProbe;

impl LoadProbe for IdleProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        0.0
    }
}

/// `sum_over_time(m[10s])` every 10 s over 2 series, 1 sample per second
/// for 60 s.
fn workload(name: &str) -> WorkloadSpec {
    WorkloadSpec {
        name: name.into(),
        queries: vec![WorkloadQuery {
            id: "sum".into(),
            texts: vec![QueryText {
                language: QueryLanguage::PromQl,
                text: "sum_over_time(m[10s])".into(),
            }],
            window_ms: 10_000,
            evaluation_interval_ms: 10_000,
        }],
        data: vec![DataRecord {
            metric: "m".into(),
            description: TimeSeriesDescription {
                labels: vec![SeriesLabelSpec {
                    name: "s".into(),
                    cardinality: 2,
                    skewness: 0.0,
                }],
                series_num: 2,
                start_ms: 0,
                sample_interval_ms: 1_000,
                duration_ms: 60_000,
                value_distribution: DataDistribution::Uniform(UniformParameter {
                    lower_bound: 0.0,
                    upper_bound: 10.0,
                    seed: 0,
                }),
                counter: false,
                seed: 1,
            },
        }],
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Workloads w1 and w2; variant v1 (no substitutions allowed) with a plan
/// for w1 and none for w2; variant v2 (substitutions allowed) with the
/// given plan texts for w1 and w2.
fn fixture(temp: &TempDir, v2_w1: &str, v2_w2: &str) -> RunDir {
    let run = RunDir::create_at(&temp.0.join("run")).unwrap();
    for name in ["w1", "w2"] {
        write(
            &run.path().join("workloads").join(format!("{name}.json")),
            &serde_json::to_string(&workload(name)).unwrap(),
        );
    }
    let plans = run.path().join("plans");
    write(
        &plans.join("v1/variant.json"),
        r#"{"substitutions_allowed": false}"#,
    );
    write(&plans.join("v1/w1.plan.json"), "{}");
    write(
        &plans.join("v1/w2.no-plan.json"),
        r#"{"reason": "ExcludedByDeployment"}"#,
    );
    write(
        &plans.join("v2/variant.json"),
        r#"{"substitutions_allowed": true}"#,
    );
    write(&plans.join("v2/w1.plan.json"), v2_w1);
    write(&plans.join("v2/w2.plan.json"), v2_w2);
    run
}

fn protocol() -> Protocol {
    Protocol {
        warmup_runs: 0,
        measured_runs: 1,
        max_core_busy_fraction: 1.0,
    }
}

fn run_fake(run: &RunDir, only: Option<&[String]>) -> Result<Vec<LaneCell>, String> {
    let engine = FakeEngine::default();
    let clocks = MeasurementClocks::with_calling_thread(&engine.clock);
    let protocol = protocol();
    run_lane(
        run,
        &engine,
        only,
        LaneSetup {
            protocol: &protocol,
            core: 0,
            probe: &mut IdleProbe,
            clocks: &clocks,
            args: vec!["test".into()],
        },
    )
    .map_err(|error| error.to_string())
}

fn outcomes(cells: &[LaneCell]) -> Vec<(String, String, CellOutcome)> {
    cells
        .iter()
        .map(|cell| {
            (
                cell.workload.clone(),
                cell.variant.clone(),
                cell.outcome.clone(),
            )
        })
        .collect()
}

fn read_result(run: &RunDir, workload: &str, variant: &str) -> ArmResult {
    let path = run
        .path()
        .join(format!("results/fake/{workload}__{variant}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// L1: every cell is written; ran cells got the absolute plan file path.
#[test]
fn a_lane_runs_every_cell_and_passes_the_plan_file() {
    let temp = TempDir::new("l1");
    let run = fixture(&temp, "{}", "{}");
    let cells = run_fake(&run, None).unwrap();
    assert_eq!(
        outcomes(&cells),
        vec![
            ("w1".into(), "v1".into(), CellOutcome::Ran),
            (
                "w2".into(),
                "v1".into(),
                CellOutcome::NoPlan {
                    reason: "ExcludedByDeployment".into()
                }
            ),
            ("w1".into(), "v2".into(), CellOutcome::Ran),
            ("w2".into(), "v2".into(), CellOutcome::Ran),
        ]
    );
    for (workload, variant) in [("w1", "v1"), ("w1", "v2"), ("w2", "v2")] {
        let result = read_result(&run, workload, variant);
        let plan = std::path::absolute(
            run.path()
                .join(format!("plans/{variant}/{workload}.plan.json")),
        )
        .unwrap();
        assert!(
            result
                .plan
                .text
                .contains(&format!("plan_file = {}", plan.display())),
            "{}",
            result.plan.text
        );
        assert_eq!(result.ingested_samples, 120);
        assert!(run
            .path()
            .join(format!("results/fake/{workload}__{variant}.cell.json"))
            .is_file());
    }
    assert!(!run.path().join("results/fake/w2__v1.json").exists());
    let lane_records = std::fs::read_dir(run.path().join("results/fake"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("lane-")
        })
        .count();
    assert_eq!(lane_records, 1);
}

/// L2 + L5: a lane can run variant by variant; a cell is never rerun.
#[test]
fn a_lane_runs_variant_by_variant_and_never_reruns_a_cell() {
    let temp = TempDir::new("l5");
    let run = fixture(&temp, "{}", "{}");
    let v2 = run_fake(&run, Some(&["v2".to_string()])).unwrap();
    assert_eq!(v2.len(), 2);
    assert!(v2.iter().all(|cell| cell.variant == "v2"));
    assert_eq!(run_fake(&run, Some(&["v1".to_string()])).unwrap().len(), 2);
    let error = run_fake(&run, Some(&["v1".to_string()])).unwrap_err();
    assert!(
        error.contains("w1__v1.cell.json") && error.contains("never overwritten"),
        "{error}"
    );
    let unknown = run_fake(&run, Some(&["v9".to_string()])).unwrap_err();
    assert!(unknown.contains("v9"), "{unknown}");
}

/// L3 + L7: an install error, and a substitution under a variant that
/// allows none, are `NotRun` cells; the other cells still run.
#[test]
fn failed_cells_are_written_and_the_lane_goes_on() {
    let temp = TempDir::new("l3");
    let run = fixture(
        &temp,
        r#"{"refuse": "refused: UnsupportedShape filter@node 3"}"#,
        "{}",
    );
    write(
        &run.path().join("plans/v1/w1.plan.json"),
        r#"{"substitutions": ["quantileDD for Kll at node 2"]}"#,
    );
    let cells = run_fake(&run, None).unwrap();
    let by_cell: BTreeMap<(String, String), CellOutcome> = outcomes(&cells)
        .into_iter()
        .map(|(w, v, o)| ((w, v), o))
        .collect();
    assert_eq!(
        by_cell[&("w1".into(), "v2".into())],
        CellOutcome::NotRun {
            error: "refused: UnsupportedShape filter@node 3".into()
        }
    );
    match &by_cell[&("w1".into(), "v1".into())] {
        CellOutcome::NotRun { error } => {
            assert!(error.contains("quantileDD for Kll at node 2"), "{error}")
        }
        other => panic!("expected NotRun, got {other:?}"),
    }
    assert_eq!(by_cell[&("w2".into(), "v2".into())], CellOutcome::Ran);
    assert!(!run.path().join("results/fake/w1__v1.json").exists());
}

/// L4 + L6: an ambiguous or unknown file in a variant is refused before
/// anything runs.
#[test]
fn a_bad_variant_layout_is_refused_before_any_cell() {
    let temp = TempDir::new("l4");
    let run = fixture(&temp, "{}", "{}");
    write(
        &run.path().join("plans/v2/w2.no-plan.json"),
        r#"{"reason": "x"}"#,
    );
    let error = run_fake(&run, None).unwrap_err();
    assert!(
        error.contains("w2.plan.json") && error.contains("w2.no-plan.json"),
        "{error}"
    );
    assert!(!run.path().join("results/fake").exists());

    let temp = TempDir::new("l6");
    let run = fixture(&temp, "{}", "{}");
    write(&run.path().join("plans/v1/w1.txt"), "");
    let error = run_fake(&run, None).unwrap_err();
    assert!(error.contains("w1.txt"), "{error}");

    let temp = TempDir::new("l6b");
    let run = fixture(&temp, "{}", "{}");
    std::fs::remove_file(run.path().join("plans/v2/variant.json")).unwrap();
    let workloads = read_workloads(&run).unwrap();
    let error = read_variants(&run, &workloads).unwrap_err().to_string();
    assert!(error.contains("variant.json"), "{error}");
}

fn series(name: &str) -> SeriesLabels {
    [("s".to_string(), name.to_string())].into_iter().collect()
}

fn answers(values: &[(&str, f64)]) -> Answers {
    let mut per_series = BTreeMap::new();
    for (name, value) in values {
        per_series.insert(series(name), *value);
    }
    Answers([("q".to_string(), per_series)].into_iter().collect())
}

fn job(cost: StepCost) -> JobRecord {
    JobRecord {
        core: 0,
        busy_before: 0.0,
        busy_after: 0.0,
        loadavg: [None, None],
        marks: Vec::new(),
        samples: vec![cost.clone()],
        median: cost,
        median_run: 0,
    }
}

fn thread_job(ns: u64) -> JobRecord {
    job(StepCost {
        thread_cpu_ns: Some(ns),
        process_cpu_ns: ns,
        wall_ns: ns,
        report: StepReport::default(),
    })
}

fn server_job(ns: u64, counter_ns: Option<u64>) -> JobRecord {
    job(StepCost {
        thread_cpu_ns: None,
        process_cpu_ns: ns,
        wall_ns: ns,
        report: StepReport {
            scanned_bytes: None,
            engine_reported_cpu_ns: counter_ns,
        },
    })
}

fn result(lane: &str, workload: &str, variant: &str, answers: Vec<(i64, Answers)>) -> ArmResult {
    ArmResult {
        engine: lane.into(),
        workload: workload.into(),
        arm: variant.into(),
        plan: PlanSummary::default(),
        predicted: None,
        ingest: vec![thread_job(1_000)],
        evaluate: vec![thread_job(1_000)],
        answers,
        footprint: Footprint::default(),
        measured_engine_cost_per_second: None,
        node_profiles: None,
        ingested_samples: 4,
        server_memory_before: None,
        server_memory_after: None,
        server_cpu_ns_whole_arm: None,
        cpu_source: "calling-thread".into(),
        installs_held: 1,
    }
}

/// Writes a cell, and its result when it ran.
fn put(
    run: &RunDir,
    lane: &str,
    workload: &str,
    variant: &str,
    outcome: CellOutcome,
    arm: Option<ArmResult>,
) {
    if let Some(arm) = arm {
        run.write_arm(&arm).unwrap();
    }
    let cell = LaneCell {
        lane: lane.into(),
        workload: workload.into(),
        variant: variant.into(),
        outcome,
    };
    write(
        &run.path()
            .join(format!("results/{lane}/{workload}__{variant}.cell.json")),
        &serde_json::to_string(&cell).unwrap(),
    );
}

fn truth_answers() -> Vec<(i64, Answers)> {
    vec![
        (10, answers(&[("s1", 10.0), ("s2", 0.0)])),
        (20, answers(&[])),
    ]
}

/// The P1 fixture: truth `a/v1`, `b/v1` a cgroup lane that ran, `b/v2` no
/// plan, `c/v1` not run, and workload w2 where the truth has no plan.
fn report_fixture(temp: &TempDir) -> RunDir {
    let run = RunDir::create_at(&temp.0.join("run")).unwrap();
    put(
        &run,
        "a",
        "w1",
        "v1",
        CellOutcome::Ran,
        Some(result("a", "w1", "v1", truth_answers())),
    );
    let mut b = result(
        "b",
        "w1",
        "v1",
        vec![
            (10, answers(&[("s1", 11.0), ("s2", 0.0), ("s3", 7.0)])),
            (20, answers(&[])),
        ],
    );
    b.ingest = vec![server_job(3_000, None), server_job(5_000, None)];
    b.evaluate = vec![
        server_job(1_500_000, Some(1_000_000)),
        server_job(2_500_000, Some(3_000_000)),
    ];
    b.server_cpu_ns_whole_arm = Some(9_000_000);
    b.cpu_source = "cgroup agent http://192.0.2.1:9101".into();
    let memory = CgroupMemory {
        current_bytes: 3 * 1_048_576,
        anon_bytes: 0,
        file_bytes: 0,
        lifetime_peak_bytes: Some(5 * 1_048_576),
    };
    b.server_memory_before = Some(memory);
    b.server_memory_after = Some(memory);
    b.installs_held = 6;
    b.footprint = Footprint {
        view_bytes: 100,
        raw_retained_bytes: 20,
        peak_bytes: None,
    };
    b.plan.substitutions = vec!["quantileDD for Kll at node 3".into(), "x".into()];
    b.plan.sharing_lost = vec!["node 4 for nodes 6, 7".into()];
    put(&run, "b", "w1", "v1", CellOutcome::Ran, Some(b));
    put(
        &run,
        "b",
        "w1",
        "v2",
        CellOutcome::NoPlan {
            reason: "ExcludedByDeployment".into(),
        },
        None,
    );
    put(
        &run,
        "c",
        "w1",
        "v1",
        CellOutcome::NotRun {
            error: "refused: filter@node 3".into(),
        },
        None,
    );
    put(
        &run,
        "a",
        "w2",
        "v1",
        CellOutcome::NoPlan {
            reason: "ExcludedByDeployment".into(),
        },
        None,
    );
    put(
        &run,
        "b",
        "w2",
        "v1",
        CellOutcome::Ran,
        Some(result("b", "w2", "v1", truth_answers())),
    );
    run
}

/// P1, P2, P5, P6: the summary rows, by hand.
#[test]
fn the_summary_has_absolute_numbers_and_errors_against_the_truth() {
    let temp = TempDir::new("p1");
    let run = report_fixture(&temp);
    let report = report(&run, "a", "v1").unwrap();
    let csv = report.summary_csv();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], SUMMARY_CSV_HEADER);
    assert_eq!(SUMMARY_CSV_HEADER.split(',').count(), 21);
    let empty_after_status = ",".repeat(17);
    let expected = [
        "w1,a,v1,ok,thread,250,0.001,,,,,,1,0,0,0,2,0,0,,".to_string(),
        "w2,a,v1,no plan: ExcludedByDeployment".to_string() + &empty_after_status,
        "w1,b,v1,ok,cgroup,2000,2,2,9,3,3,5,6,120,0.1,0.05,2,0,1,quantileDD for Kll at node 3;x,\"node 4 for nodes 6, 7\"".to_string(),
        "w1,b,v2,no plan: ExcludedByDeployment".to_string() + &empty_after_status,
        "w2,b,v1,ok,thread,250,0.001,,,,,,1,0,,,,,,,".to_string(),
        "w1,c,v1,not run: refused: filter@node 3".to_string() + &empty_after_status,
    ];
    assert_eq!(
        lines[1..].to_vec(),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );

    let accuracy = report.accuracy_csv();
    assert!(
        accuracy
            .lines()
            .any(|line| line == "w1,b,v1,q,0.1,0.05,2,0,1"),
        "{accuracy}"
    );

    let markdown = report.summary_markdown();
    assert!(
        markdown.contains("| q 2.000 ms, i 2000.0 ns/sample, err 0.100 [cgroup] |"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| not run: refused: filter@node 3 |"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| no plan: ExcludedByDeployment |"),
        "{markdown}"
    );

    write_report(&run, &report).unwrap();
    for file in ["summary.csv", "summary.md", "accuracy.csv"] {
        assert!(run.path().join(file).is_file(), "{file}");
    }
    assert!(report_error(&run, "a", "v9").contains("a/v9"));
}

fn report_error(run: &RunDir, lane: &str, variant: &str) -> String {
    report(run, lane, variant).unwrap_err().to_string()
}

/// P2: the relative-error rules.
#[test]
fn relative_errors_follow_the_stated_rules() {
    use aqpbm_harness::bench_external::relative_error;
    assert_eq!(relative_error(11.0, 10.0), 0.1);
    assert_eq!(relative_error(0.0, 0.0), 0.0);
    assert_eq!(relative_error(1.0, 0.0), f64::INFINITY);
    assert_eq!(relative_error(f64::NAN, f64::NAN), 0.0);
    assert_eq!(relative_error(f64::NAN, 1.0), f64::INFINITY);
    assert_eq!(relative_error(1.0, f64::NAN), f64::INFINITY);
    assert_eq!(relative_error(1.0, f64::INFINITY), f64::INFINITY);
    assert_eq!(relative_error(f64::INFINITY, 1.0), f64::INFINITY);
    assert_eq!(
        relative_error(f64::NEG_INFINITY, f64::INFINITY),
        f64::INFINITY
    );
    assert_eq!(relative_error(f64::INFINITY, f64::INFINITY), 0.0);
}

/// Review items 5 and 7: lanes that read different data, or answered at
/// other times than the truth, are refused by the report.
#[test]
fn the_report_refuses_lanes_that_did_not_run_the_same_data_or_times() {
    let temp = TempDir::new("p7");
    let run = RunDir::create_at(&temp.0.join("run")).unwrap();
    put(
        &run,
        "a",
        "w1",
        "v1",
        CellOutcome::Ran,
        Some(result("a", "w1", "v1", truth_answers())),
    );
    put(
        &run,
        "b",
        "w1",
        "v1",
        CellOutcome::Ran,
        Some(result("b", "w1", "v1", vec![(10, answers(&[]))])),
    );
    let error = report_error(&run, "a", "v1");
    assert!(
        error.contains("[10]") && error.contains("[10, 20]"),
        "{error}"
    );

    let temp = TempDir::new("p8");
    let run = RunDir::create_at(&temp.0.join("run")).unwrap();
    put(
        &run,
        "a",
        "w1",
        "v1",
        CellOutcome::Ran,
        Some(result("a", "w1", "v1", truth_answers())),
    );
    let mut other = result("b", "w1", "v1", truth_answers());
    other.ingested_samples = 5;
    put(&run, "b", "w1", "v1", CellOutcome::Ran, Some(other));
    let error = report_error(&run, "a", "v1");
    assert!(
        error.contains("4") && error.contains("5") && error.contains("same data"),
        "{error}"
    );
}

/// A workload or variant name that cannot be part of a result file name is
/// refused before any cell runs, not after its arm ran.
#[test]
fn result_file_names_are_checked_before_any_cell_runs() {
    let temp = TempDir::new("names");
    let run = fixture(&temp, "{}", "{}");
    let workloads = run.path().join("workloads");
    let spec = workload("w__3");
    write(
        &workloads.join("w__3.json"),
        &serde_json::to_string(&spec).unwrap(),
    );
    for variant in ["v1", "v2"] {
        write(
            &run.path().join(format!("plans/{variant}/w__3.plan.json")),
            "{}",
        );
    }
    let error = run_fake(&run, None).unwrap_err();
    assert!(error.contains("w__3") && error.contains("__"), "{error}");
    assert!(!run.path().join("results/fake").exists());
}

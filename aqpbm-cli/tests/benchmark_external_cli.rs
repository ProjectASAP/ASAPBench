//! `approxbench benchmark-external run` and `report` on the fake engine.

use std::path::{Path, PathBuf};
use std::process::Command;

use aqpbm_core::harness::data::DataRecord;
use aqpbm_core::harness::engine::{QueryLanguage, QueryText, WorkloadQuery, WorkloadSpec};
use aqpbm_core::harness::run_directory::RunDirectory;
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};

fn workload(name: &str) -> WorkloadSpec {
    WorkloadSpec {
        name: name.into(),
        queries: vec![WorkloadQuery {
            id: "sum".into(),
            texts: vec![QueryText {
                language: QueryLanguage::PromQl,
                text: "sum_over_time(m[10s])".into(),
                dialect: None,
            }],
            window_ms: 10_000,
            evaluation_interval_ms: 10_000,
            truth: None,
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
        tables: Vec::new(),
        one_shot_queries: Vec::new(),
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fixture(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "approxbench-benchmark-external-{}-{test}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let run = RunDirectory::create_at(&root.join("run")).unwrap();
    for name in ["w1", "w2"] {
        write(
            &run.path().join(format!("workloads/{name}.json")),
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
    write(&plans.join("v2/w1.plan.json"), "{}");
    write(&plans.join("v2/w2.plan.json"), "{}");
    run.path().to_path_buf()
}

fn approxbench(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_approxbench"))
        .args(args)
        .output()
        .unwrap()
}

/// X1: a fake engine runs every cell, and the report has one row per cell.
#[test]
fn a_fake_engine_runs_and_reports() {
    let run = fixture("x1");
    let run_directory = run.to_str().unwrap();
    let out = approxbench(&[
        "benchmark-external",
        "run",
        "--run-directory",
        run_directory,
        "--engine",
        "fake",
        "--warmup",
        "0",
        "--runs",
        "1",
        "--max-core-busy",
        "1.0",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = approxbench(&[
        "benchmark-external",
        "report",
        "--run-directory",
        run_directory,
        "--truth",
        "fake/v2",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary = std::fs::read_to_string(run.join("summary.csv")).unwrap();
    assert_eq!(summary.lines().count(), 5, "{summary}");
    assert!(
        summary
            .lines()
            .any(|line| line.starts_with("w1,fake,v1,ok,thread,")),
        "{summary}"
    );
    assert!(
        summary
            .lines()
            .any(|line| line.starts_with("w2,fake,v1,no plan: ExcludedByDeployment")),
        "{summary}"
    );
    let _ = std::fs::remove_dir_all(run.parent().unwrap());
}

/// X2: an unknown engine names the known ones.
#[test]
fn an_unknown_engine_names_the_known_engines() {
    let run = fixture("x2");
    let out = approxbench(&[
        "benchmark-external",
        "run",
        "--run-directory",
        run.to_str().unwrap(),
        "--engine",
        "no-such-engine",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("clickhouse") && stderr.contains("fake"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(run.parent().unwrap());
}

/// X3: the clickhouse engine needs the server and its cgroup directory.
#[test]
fn the_clickhouse_engine_needs_its_server() {
    let run = fixture("x3");
    let out = approxbench(&[
        "benchmark-external",
        "run",
        "--run-directory",
        run.to_str().unwrap(),
        "--engine",
        "clickhouse",
        "--clickhouse-url",
        "http://10.0.0.2:8123",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--clickhouse-url") && stderr.contains("--server-cgroup-directory"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(run.parent().unwrap());
}

/// X4: engine `victoriametrics` needs its server, cgroup directory and
/// config file, and runs one arm at a time (wall-clock data): only with
/// `--warmup 0 --runs 1`.
#[test]
fn the_victoriametrics_engine_needs_its_server_and_one_run() {
    let run = fixture("x4");
    let base = [
        "benchmark-external",
        "run",
        "--run-directory",
        run.to_str().unwrap(),
        "--engine",
        "victoriametrics",
    ];
    let out = approxbench(&base);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--victoriametrics-url"), "{stderr}");
    let mut with_server = base.to_vec();
    with_server.extend([
        "--victoriametrics-url",
        "http://127.0.0.1:1",
        "--server-cgroup-directory",
        "/sys/fs/cgroup/system.slice/docker-1.scope",
        "--victoriametrics-stream-aggregation-config",
        "/data/victoriametrics/config/stream-aggregation.yaml",
    ]);
    let out = approxbench(&with_server);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--warmup 0 --runs 1"), "{stderr}");
    let _ = std::fs::remove_dir_all(run.parent().unwrap());
}

#[test]
fn the_asapquery_backend_engine_needs_its_server_and_one_run() {
    let run = fixture("x5");
    let base = [
        "benchmark-external",
        "run",
        "--run-directory",
        run.to_str().unwrap(),
        "--engine",
        "asapquery-backend",
    ];
    let out = approxbench(&base);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--asapquery-backend-url"), "{stderr}");
    let mut with_server = base.to_vec();
    with_server.extend([
        "--asapquery-backend-url",
        "http://127.0.0.1:1",
        "--server-cgroup-directory",
        "/sys/fs/cgroup/system.slice/docker-1.scope",
    ]);
    let out = approxbench(&with_server);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--warmup 0 --runs 1"), "{stderr}");
    let _ = std::fs::remove_dir_all(run.parent().unwrap());
}

#[test]
fn the_fake_engine_refuses_server_flags() {
    let run = fixture("x6");
    let out = approxbench(&[
        "benchmark-external",
        "run",
        "--run-directory",
        run.to_str().unwrap(),
        "--engine",
        "fake",
        "--asapquery-backend-url",
        "http://127.0.0.1:1",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--asapquery-backend-url"), "{stderr}");
    let _ = std::fs::remove_dir_all(run.parent().unwrap());
}

#[test]
fn write_throughput_names_its_engines_and_needs_the_precompute_engine_url() {
    let run = fixture("write-throughput");
    let run_directory = run.to_str().unwrap();
    let base = [
        "benchmark-external",
        "write-throughput",
        "--run-directory",
        run_directory,
        "--workload",
        "w1",
        "--writers",
        "2",
        "--max-event-time-lead-ms",
        "2000",
        "--engine",
    ];
    let out = approxbench(&[&base[..], &["fake"]].concat());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("asapquery-precompute-engine"), "{stderr}");

    let out = approxbench(&[&base[..], &["asapquery-precompute-engine"]].concat());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--asapquery-precompute-engine-url") && stderr.contains("--server-pid"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(run.parent().unwrap());
}

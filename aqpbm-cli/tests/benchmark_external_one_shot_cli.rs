use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use aqpbm_core::harness::engine::{
    LoadStatements, OneShotQuery, QueryLanguage, QueryText, SqlDialect, TableSource, WorkloadSpec,
};

struct Fixture(PathBuf);

impl Fixture {
    fn new(test: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "approxbench-one-shot-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for sub in [
            "run/workloads",
            "run/data",
            "run/plans",
            "run/results",
            "source",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Fixture(root)
    }

    fn run(&self) -> PathBuf {
        self.0.join("run")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sql(text: &str) -> QueryText {
    QueryText {
        language: QueryLanguage::Sql,
        text: text.into(),
        dialect: None,
    }
}

fn workload() -> WorkloadSpec {
    WorkloadSpec::one_shot(
        "events".into(),
        vec![TableSource {
            name: "events".into(),
            file_name: "events.csv".into(),
            url: None,
            rows: 4,
            load_statements: vec![LoadStatements {
                dialect: SqlDialect::DuckDb,
                statements: vec![
                    "CREATE TABLE events (k INTEGER, v DOUBLE)".into(),
                    "INSERT INTO events SELECT * FROM read_csv('{file}')".into(),
                ],
            }],
        }],
        vec![
            OneShotQuery {
                id: "total".into(),
                texts: vec![sql("SELECT SUM(v), COUNT(*) FROM events")],
                group_by_column_positions: Vec::new(),
                variants: BTreeMap::new(),
            },
            OneShotQuery {
                id: "by-key".into(),
                texts: vec![sql("SELECT k, SUM(v) FROM events GROUP BY k")],
                group_by_column_positions: vec![0],
                variants: BTreeMap::from([(
                    "approximate".to_string(),
                    vec![sql("SELECT k, SUM(v) * 1.0 FROM events GROUP BY k")],
                )]),
            },
        ],
    )
}

fn write_workload(run: &Path) {
    std::fs::write(
        run.join("workloads/events.json"),
        serde_json::to_string_pretty(&workload()).unwrap(),
    )
    .unwrap();
}

fn approxbench(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_approxbench"))
        .arg("benchmark-external")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn an_unknown_engine_and_a_missing_database_are_refused() {
    let fixture = Fixture::new("refused");
    write_workload(&fixture.run());
    let run = fixture.run();
    let run = run.to_str().unwrap();
    let common = [
        "one-shot-queries",
        "--run-directory",
        run,
        "--workload",
        "events",
        "--configuration",
        "exact",
    ];
    let output = approxbench(&[&common[..], &["--engine", "sqlite"]].concat());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duckdb, clickhouse, pilotdb"));
    let output = approxbench(&[&common[..], &["--engine", "pilotdb"]].concat());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--duckdb-database"));
    let output = approxbench(&[&common[..], &["--engine", "postgresql"]].concat());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--postgresql-connection"));
    let output = approxbench(&[&common[..], &["--engine", "druid"]].concat());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--druid-url"));
    let output = approxbench(&[
        "one-shot-report",
        "--run-directory",
        run,
        "--truth",
        "duckdb",
    ]);
    assert!(!output.status.success());
}

#[test]
fn duckdb_loads_runs_and_reports_against_itself() {
    let Ok(duckdb) = std::env::var("AQPBM_DUCKDB") else {
        return;
    };
    let fixture = Fixture::new("duckdb");
    let run = fixture.run();
    write_workload(&run);
    std::fs::write(
        fixture.0.join("source/events.csv"),
        "k,v\n1,10\n1,20\n2,5\n3,1\n",
    )
    .unwrap();
    let database = fixture.0.join("events.db");
    for (configuration, variant) in [
        ("exact", &[][..]),
        ("exact-again", &[][..]),
        ("approximate", &["--query-variant", "approximate"][..]),
    ] {
        let output = approxbench(
            &[
                &[
                    "one-shot-queries",
                    "--run-directory",
                    run.to_str().unwrap(),
                    "--engine",
                    "duckdb",
                    "--workload",
                    "events",
                    "--configuration",
                    configuration,
                    "--duckdb-program",
                    &duckdb,
                    "--duckdb-database",
                    database.to_str().unwrap(),
                    "--duckdb-threads",
                    "2",
                    "--data-directory",
                    fixture.0.join("source").to_str().unwrap(),
                    "--runs",
                    "2",
                ],
                variant,
            ]
            .concat(),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!(
            "results/duckdb/events__{configuration}.one-shot.json"
        )));
    }
    let first: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run.join("results/duckdb/events__exact.one-shot.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(first["tables"][0]["loaded_in_this_run"], true);
    let again: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run.join("results/duckdb/events__exact-again.one-shot.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(again["tables"][0]["loaded_in_this_run"], false);
    let approximate: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(run.join("results/duckdb/events__approximate.one-shot.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(approximate["query_variant"], "approximate");
    assert_eq!(approximate["queries"].as_array().unwrap().len(), 1);
    assert_eq!(
        approximate["queries"][0]["sql"],
        "SELECT k, SUM(v) * 1.0 FROM events GROUP BY k"
    );
    let output = approxbench(&[
        "one-shot-report",
        "--run-directory",
        run.to_str().unwrap(),
        "--truth",
        "duckdb/exact",
        "--baseline",
        "duckdb/exact",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = std::fs::read_to_string(run.join("one-shot-summary.csv")).unwrap();
    assert_eq!(summary.lines().count(), 6, "{summary}");
    for line in summary.lines().skip(1) {
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields[4], "ok", "{line}");
        assert_eq!(fields[13], "", "{line}");
        assert_eq!(fields[14], "0", "{line}");
    }
    assert!(run.join("one-shot-summary.md").is_file());
    assert!(std::fs::read_to_string(run.join("one-shot-tables.csv"))
        .unwrap()
        .contains("duckdb,exact,events,4,"));
}

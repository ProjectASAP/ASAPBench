use std::collections::{BTreeMap, BTreeSet};

use aqpbm_core::harness::engine::EngineDescription;
use aqpbm_core::harness::one_shot::{
    ErrorTarget, OneShotResult, QueryRecord, ResultTable, ResultValue, RunRecord, TableRecord,
};
use aqpbm_core::harness::one_shot_report::{compare_tables, one_shot_report, ConfigurationName};

fn n(value: f64) -> ResultValue {
    ResultValue::Number(value)
}

fn t(value: &str) -> ResultValue {
    ResultValue::Text(value.into())
}

fn table(rows: Vec<Vec<ResultValue>>) -> ResultTable {
    ResultTable {
        columns: (0..rows[0].len()).map(|i| format!("c{i}")).collect(),
        rows,
    }
}

#[test]
fn groups_are_matched_by_key_not_by_row_order() {
    let truth = table(vec![vec![n(2.0), n(100.0)], vec![n(3.0), n(50.0)]]);
    let answer = table(vec![vec![n(3.0), n(55.0)], vec![n(2.0), n(99.0)]]);
    let comparison = compare_tables(&answer, &truth, &[0]).unwrap();
    assert_eq!(comparison.compared_cells, 2);
    assert!((comparison.max_relative_error.unwrap() - 0.1).abs() < 1e-12);
    assert_eq!((comparison.missing_groups, comparison.extra_groups), (0, 0));
}

#[test]
fn missing_and_extra_groups_are_counted() {
    let truth = table(vec![vec![t("a"), n(1.0)], vec![t("b"), n(1.0)]]);
    let answer = table(vec![vec![t("a"), n(1.0)], vec![t("c"), n(1.0)]]);
    let comparison = compare_tables(&answer, &truth, &[0]).unwrap();
    assert_eq!(comparison.max_relative_error, Some(0.0));
    assert_eq!((comparison.missing_groups, comparison.extra_groups), (1, 1));
}

#[test]
fn without_group_columns_rows_are_matched_by_position() {
    let truth = table(vec![vec![n(10.0), t("x"), n(4.0)]]);
    let answer = table(vec![vec![n(9.0), t("y"), n(4.0)]]);
    let comparison = compare_tables(&answer, &truth, &[]).unwrap();
    assert!((comparison.max_relative_error.unwrap() - 0.1).abs() < 1e-12);
    assert_eq!(comparison.differing_text_cells, 1);
    assert!(compare_tables(&table(vec![vec![n(1.0)]]), &truth, &[]).is_err());
    assert!(compare_tables(&answer, &truth, &[5]).is_err());
}

fn run(seconds: f64, value: f64, exact: Option<bool>) -> RunRecord {
    RunRecord {
        cold: false,
        wall_seconds: seconds + 1.0,
        engine_reported_seconds: Some(seconds),
        engine_reported_cpu_seconds: Some(seconds * 4.0),
        details: exact
            .map(|exact| BTreeMap::from([("answered_exactly".to_string(), exact.into())]))
            .unwrap_or_default(),
        result: Some(table(vec![vec![n(value)]])),
        error: None,
    }
}

fn result(
    engine: &str,
    configuration: &str,
    runs: Vec<RunRecord>,
    error_target: Option<ErrorTarget>,
) -> OneShotResult {
    OneShotResult {
        engine: EngineDescription {
            id: engine.into(),
            version: "1".into(),
            version_dirty: false,
            build_profile: String::new(),
        },
        workload: "clickbench".into(),
        configuration: configuration.into(),
        configuration_options: serde_json::Value::Null,
        error_target,
        query_variant: None,
        cold_first_run: true,
        tables: vec![TableRecord {
            name: "hits".into(),
            rows: 10,
            loaded_in_this_run: true,
            load_seconds: Some(3.0),
            data_size_bytes: Some(100),
        }],
        queries: vec![QueryRecord {
            id: "1".into(),
            group_by_column_positions: Vec::new(),
            sql: Some("SELECT COUNT(*) FROM hits".into()),
            skipped: None,
            runs,
        }],
        cpu_frequency_governors: BTreeSet::new(),
    }
}

#[test]
fn the_report_times_hot_runs_scores_against_the_truth_and_counts_misses() {
    let target = ErrorTarget {
        relative_error: 0.05,
        failure_probability: 0.05,
    };
    let results = vec![
        result(
            "duckdb",
            "exact",
            vec![
                run(4.0, 100.0, None),
                run(2.0, 100.0, None),
                run(2.2, 100.0, None),
            ],
            None,
        ),
        result(
            "pilotdb",
            "five-percent",
            vec![
                run(1.0, 101.0, Some(false)),
                run(0.5, 98.0, Some(false)),
                run(0.4, 90.0, Some(false)),
                run(0.6, 100.0, Some(true)),
            ],
            Some(target),
        ),
    ];
    let truth = ConfigurationName::parse("duckdb/exact").unwrap();
    let report = one_shot_report(&results, Some(&truth), std::slice::from_ref(&truth)).unwrap();
    let pilotdb = &report.rows[1];
    assert_eq!(pilotdb.configuration.to_string(), "pilotdb/five-percent");
    assert_eq!(pilotdb.time_source, "engine");
    assert_eq!(pilotdb.first_run_seconds, Some(1.0));
    assert_eq!(pilotdb.hot_min_seconds, Some(0.4));
    assert_eq!(pilotdb.hot_median_seconds, Some(0.5));
    assert_eq!(pilotdb.hot_median_engine_cpu_seconds, Some(2.0));
    assert!((pilotdb.speedups[0].1.unwrap() - 2.1 / 0.5).abs() < 1e-12);
    assert!((pilotdb.max_relative_error.unwrap() - 0.1).abs() < 1e-12);
    assert_eq!(pilotdb.runs_over_target, Some(1));
    assert_eq!(pilotdb.runs_answered_exactly, Some(1));
    assert_eq!(pilotdb.target_relative_error, Some(0.05));
    let duckdb = &report.rows[0];
    assert_eq!(duckdb.max_relative_error, Some(0.0));
    assert_eq!(duckdb.runs_over_target, None);
    assert_eq!(duckdb.speedups[0].1, Some(1.0));

    let csv = report.summary_csv();
    assert!(
        csv.lines()
            .next()
            .unwrap()
            .contains("speedup_over_duckdb/exact"),
        "{csv}"
    );
    assert_eq!(csv.lines().count(), 3);
    assert!(report.summary_markdown().contains("1/4 (target 0.05)"));
    assert_eq!(
        report.tables_csv().lines().nth(1),
        Some("duckdb,exact,hits,10,3,100")
    );
}

#[test]
fn a_failed_query_and_an_unknown_truth_are_reported() {
    let mut failed = run(1.0, 1.0, None);
    failed.error = Some("timeout".into());
    failed.result = None;
    failed.engine_reported_seconds = None;
    let results = vec![result("clickhouse", "exact", vec![failed], None)];
    let report = one_shot_report(&results, None, &[]).unwrap();
    assert_eq!(report.rows[0].status, "failed: timeout");
    assert_eq!(report.rows[0].first_run_seconds, None);
    let missing = ConfigurationName::parse("duckdb/exact").unwrap();
    assert!(one_shot_report(&results, Some(&missing), &[]).is_err());
    assert!(ConfigurationName::parse("duckdb").is_err());
}

#[test]
fn a_null_or_text_where_the_truth_has_a_number_is_an_infinite_error() {
    let truth = table(vec![vec![n(7.0), t("2013-07-15")]]);
    let answer = table(vec![vec![ResultValue::Null, t("2013-07-15 00:00:00")]]);
    let comparison = compare_tables(&answer, &truth, &[]).unwrap();
    assert_eq!(comparison.max_relative_error, Some(f64::INFINITY));
    assert_eq!(comparison.differing_text_cells, 0);
    let answer = table(vec![vec![t("seven"), t("2013-07-16")]]);
    let comparison = compare_tables(&answer, &truth, &[]).unwrap();
    assert_eq!(comparison.max_relative_error, Some(f64::INFINITY));
    assert_eq!(comparison.differing_text_cells, 1);
}

#[test]
fn empty_results_compare_whatever_their_header() {
    let empty_without_header = ResultTable {
        columns: Vec::new(),
        rows: Vec::new(),
    };
    let empty = ResultTable {
        columns: vec!["k".into(), "c".into()],
        rows: Vec::new(),
    };
    let comparison = compare_tables(&empty_without_header, &empty, &[0]).unwrap();
    assert_eq!(comparison.compared_cells, 0);
    assert_eq!((comparison.missing_groups, comparison.extra_groups), (0, 0));
    let truth = table(vec![vec![n(1.0), n(5.0)], vec![n(2.0), n(6.0)]]);
    let comparison = compare_tables(&empty_without_header, &truth, &[0]).unwrap();
    assert_eq!((comparison.missing_groups, comparison.extra_groups), (2, 0));
}

#[test]
fn a_truth_whose_runs_differ_is_flagged_and_one_run_gives_no_speedup() {
    let mut differing = run(2.0, 100.0, None);
    differing.result = Some(table(vec![vec![n(99.0)]]));
    let results = vec![
        result(
            "duckdb",
            "exact",
            vec![run(4.0, 100.0, None), differing, run(2.0, 100.0, None)],
            None,
        ),
        result("clickhouse", "exact", vec![run(1.0, 100.0, None)], None),
    ];
    let truth = ConfigurationName::parse("duckdb/exact").unwrap();
    let report = one_shot_report(&results, Some(&truth), std::slice::from_ref(&truth)).unwrap();
    assert_eq!(report.rows[0].truth_runs_agree, Some(false));
    assert_eq!(report.rows[1].truth_runs_agree, Some(false));
    assert_eq!(report.rows[1].hot_median_seconds, None);
    assert_eq!(report.rows[1].speedups[0].1, None);
    assert!(report
        .summary_csv()
        .lines()
        .next()
        .unwrap()
        .contains("truth_runs_agree"));
}

#[test]
fn csv_fields_with_commas_and_quotes_are_quoted() {
    let mut failed = run(1.0, 1.0, None);
    failed.error = Some("Code: 62, \"syntax\"".into());
    failed.result = None;
    let results = vec![result("clickhouse", "exact", vec![failed], None)];
    let csv = one_shot_report(&results, None, &[]).unwrap().summary_csv();
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let record = reader.records().next().unwrap().unwrap();
    assert_eq!(&record[4], "failed: Code: 62, \"syntax\"");
}

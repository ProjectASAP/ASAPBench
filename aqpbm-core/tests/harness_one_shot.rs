use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aqpbm_core::harness::engine::{
    EngineDescription, EngineError, LoadStatements, OneShotQuery, QueryLanguage, QueryText,
    SqlDialect, TableSource, WorkloadSpec,
};
use aqpbm_core::harness::one_shot::{
    run_one_shot_queries, OneShotError, OneShotQueryEngine, OneShotSchedule, OneShotSetup,
    QueryRun, ResultTable, ResultValue,
};
use aqpbm_core::harness::run_directory::RunDirectory;

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> TempDirectory {
        let path = std::env::temp_dir().join(format!(
            "aqpbm-core-harness-one-shot-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDirectory(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct ScriptedEngine {
    rows: Option<u64>,
    rows_after_load: u64,
    loaded: Vec<String>,
    restarts: u32,
    asked: Vec<String>,
    fail_on_call: Option<usize>,
    fail_restart: bool,
    idle_waits: u32,
}

impl OneShotQueryEngine for ScriptedEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "scripted".into(),
            version: "1".into(),
            version_dirty: false,
            build_profile: "test".into(),
        }
    }
    fn dialect(&self) -> SqlDialect {
        SqlDialect::DuckDb
    }
    fn table_rows(&mut self, _table: &str) -> Result<Option<u64>, EngineError> {
        Ok(self.rows)
    }
    fn load_table(&mut self, statements: &[String]) -> Result<(), EngineError> {
        self.loaded.extend(statements.iter().cloned());
        self.rows = Some(self.rows_after_load);
        Ok(())
    }
    fn data_size_bytes(&mut self, _table: &str) -> Result<Option<u64>, EngineError> {
        Ok(Some(4096))
    }
    fn restart(&mut self) -> Result<(), EngineError> {
        self.restarts += 1;
        if self.fail_restart {
            return Err(EngineError("server did not come back".into()));
        }
        Ok(())
    }
    fn wait_until_idle(&mut self) -> Result<(), EngineError> {
        self.idle_waits += 1;
        Ok(())
    }
    fn run_query(&mut self, sql: &str) -> Result<QueryRun, EngineError> {
        self.asked.push(sql.to_string());
        if self.fail_on_call == Some(self.asked.len()) {
            return Err(EngineError("out of memory".into()));
        }
        Ok(QueryRun {
            table: ResultTable {
                columns: vec!["n".into()],
                rows: vec![vec![ResultValue::Number(self.asked.len() as f64)]],
            },
            engine_reported_seconds: Some(0.5),
            engine_reported_cpu_seconds: None,
            details: BTreeMap::new(),
        })
    }
    fn last_query_cpu_seconds(&mut self) -> Option<f64> {
        Some(2.0)
    }
}

fn sql(text: &str, dialect: Option<SqlDialect>) -> QueryText {
    QueryText {
        language: QueryLanguage::Sql,
        text: text.into(),
        dialect,
    }
}

fn workload() -> WorkloadSpec {
    WorkloadSpec::one_shot(
        "clickbench".into(),
        vec![TableSource {
            name: "hits".into(),
            file_name: "hits.parquet".into(),
            url: None,
            rows: 10,
            load_statements: vec![LoadStatements {
                dialect: SqlDialect::DuckDb,
                statements: vec!["INSERT INTO hits SELECT * FROM read_parquet('{file}')".into()],
            }],
        }],
        vec![
            OneShotQuery {
                id: "1".into(),
                texts: vec![sql("SELECT COUNT(*) FROM hits", None)],
                group_by_column_positions: Vec::new(),
                variants: BTreeMap::new(),
            },
            OneShotQuery {
                id: "2".into(),
                texts: vec![sql("SELECT 2", Some(SqlDialect::ClickHouse))],
                group_by_column_positions: Vec::new(),
                variants: BTreeMap::from([(
                    "approximate".to_string(),
                    vec![sql("SELECT 2.0", Some(SqlDialect::ClickHouse))],
                )]),
            },
            OneShotQuery {
                id: "3".into(),
                texts: vec![sql("SELECT 3", None)],
                group_by_column_positions: Vec::new(),
                variants: BTreeMap::from([(
                    "approximate".to_string(),
                    vec![sql("SELECT 3.0", None)],
                )]),
            },
        ],
    )
}

fn schedule(runs: u32, cold_first_run: bool) -> OneShotSchedule {
    OneShotSchedule {
        runs,
        cold_first_run,
        queries: Vec::new(),
        query_variant: None,
    }
}

fn setup<'a>(
    schedule: &'a OneShotSchedule,
    data: Option<&'a Path>,
    drop_caches: &'a Path,
) -> OneShotSetup<'a> {
    OneShotSetup {
        configuration: "exact",
        schedule,
        data_directory: data,
        drop_caches,
    }
}

#[test]
fn a_missing_table_is_loaded_from_the_data_directory_and_timed() {
    let directory = TempDirectory::new("load");
    std::fs::write(directory.0.join("hits.parquet"), "x").unwrap();
    let mut engine = ScriptedEngine {
        rows_after_load: 10,
        ..ScriptedEngine::default()
    };
    let schedule = schedule(1, false);
    let drop_caches = directory.0.join("drop_caches");
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, Some(&directory.0), &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        engine.loaded,
        vec![format!(
            "INSERT INTO hits SELECT * FROM read_parquet('{}')",
            directory.0.join("hits.parquet").display()
        )]
    );
    let table = &result.tables[0];
    assert!(table.loaded_in_this_run);
    assert!(table.load_seconds.is_some());
    assert_eq!(table.data_size_bytes, Some(4096));
    assert!(!drop_caches.exists());
}

#[test]
fn a_loaded_table_is_kept_and_a_wrong_row_count_is_refused() {
    let directory = TempDirectory::new("rows");
    let drop_caches = directory.0.join("drop_caches");
    let schedule = schedule(1, false);
    let mut engine = ScriptedEngine {
        rows: Some(10),
        ..ScriptedEngine::default()
    };
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    assert!(!result.tables[0].loaded_in_this_run);
    assert!(engine.loaded.is_empty());

    let mut engine = ScriptedEngine {
        rows: Some(7),
        ..ScriptedEngine::default()
    };
    let error = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            OneShotError::RowsDiffer {
                held: 7,
                expected: 10,
                ..
            }
        ),
        "{error}"
    );

    let mut engine = ScriptedEngine::default();
    let error = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(error, OneShotError::NoDataDirectory { .. }),
        "{error}"
    );
}

#[test]
fn the_first_run_is_cold_and_a_query_without_the_dialect_is_skipped() {
    let directory = TempDirectory::new("cold");
    let drop_caches = directory.0.join("drop_caches");
    let schedule = schedule(3, true);
    let mut engine = ScriptedEngine {
        rows: Some(10),
        ..ScriptedEngine::default()
    };
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&drop_caches).unwrap(), "3\n");
    assert_eq!(engine.restarts, 2);
    assert_eq!(
        engine.asked,
        vec!["SELECT COUNT(*) FROM hits"; 3]
            .into_iter()
            .chain(vec!["SELECT 3"; 3])
            .collect::<Vec<_>>()
    );
    let first = &result.queries[0];
    assert_eq!(
        first.runs.iter().map(|run| run.cold).collect::<Vec<_>>(),
        vec![true, false, false]
    );
    assert_eq!(first.runs[2].engine_reported_cpu_seconds, Some(2.0));
    assert_eq!(result.queries[1].id, "2");
    assert!(result.queries[1].runs.is_empty());
    assert!(result.queries[1]
        .skipped
        .as_deref()
        .unwrap()
        .contains("DuckDb"));
}

#[test]
fn a_failed_run_ends_that_query_only() {
    let directory = TempDirectory::new("fails");
    let drop_caches = directory.0.join("drop_caches");
    let schedule = schedule(3, false);
    let mut engine = ScriptedEngine {
        rows: Some(10),
        fail_on_call: Some(2),
        ..ScriptedEngine::default()
    };
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    let first = &result.queries[0];
    assert_eq!(first.runs.len(), 2);
    assert_eq!(first.runs[1].error.as_deref(), Some("out of memory"));
    assert!(first.runs[1].result.is_none());
    assert_eq!(result.queries[2].runs.len(), 3);
}

#[test]
fn only_the_chosen_queries_run_and_an_unknown_one_is_refused() {
    let directory = TempDirectory::new("subset");
    let drop_caches = directory.0.join("drop_caches");
    let mut chosen = schedule(1, false);
    chosen.queries = vec!["3".into()];
    let mut engine = ScriptedEngine {
        rows: Some(10),
        ..ScriptedEngine::default()
    };
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&chosen, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        result
            .queries
            .iter()
            .map(|query| query.id.as_str())
            .collect::<Vec<_>>(),
        vec!["3"]
    );
    chosen.queries = vec!["44".into()];
    let error = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&chosen, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(error, OneShotError::UnknownQuery { .. }),
        "{error}"
    );
}

#[test]
fn a_variant_runs_only_the_queries_written_in_it() {
    let directory = TempDirectory::new("variant");
    let drop_caches = directory.0.join("drop_caches");
    let mut approximate = schedule(1, false);
    approximate.query_variant = Some("approximate".into());
    let mut engine = ScriptedEngine {
        rows: Some(10),
        ..ScriptedEngine::default()
    };
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&approximate, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(engine.asked, vec!["SELECT 3.0"]);
    assert_eq!(result.query_variant.as_deref(), Some("approximate"));
    let ids: Vec<(&str, Option<&str>)> = result
        .queries
        .iter()
        .map(|query| (query.id.as_str(), query.skipped.as_deref()))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("2", Some("no approximate SQL text for DuckDb")),
            ("3", None)
        ]
    );
    approximate.query_variant = Some("sampled".into());
    let error = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&approximate, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(error, OneShotError::NoVariant { .. }), "{error}");
}

#[test]
fn csv_results_keep_column_order_and_value_kinds() {
    let table = ResultTable::from_csv_with_header(
        "AdvEngineID,count_star(),d\n2,404602,2013-07-15\n\\N,1.5, x\n",
    )
    .unwrap();
    assert_eq!(table.columns, vec!["AdvEngineID", "count_star()", "d"]);
    assert_eq!(
        table.rows,
        vec![
            vec![
                ResultValue::Integer(2),
                ResultValue::Integer(404_602),
                ResultValue::Text("2013-07-15".into())
            ],
            vec![
                ResultValue::Null,
                ResultValue::Number(1.5),
                ResultValue::Text(" x".into())
            ],
        ]
    );
    assert_eq!(ResultValue::Number(2.0).key_text(), "2");
    assert_eq!(ResultValue::Number(2.5).key_text(), "2.5");
    assert_eq!(
        ResultValue::from_json(&serde_json::json!(3)),
        ResultValue::Integer(3)
    );
}

#[test]
fn large_integers_dates_and_words_keep_their_identity() {
    let big = ResultValue::from_text("435090932899640449");
    assert_eq!(big, ResultValue::Integer(435_090_932_899_640_449));
    assert_ne!(
        big.key_text(),
        ResultValue::from_text("435090932899640450").key_text()
    );
    for word in ["nan", "inf", "infinity", "NaN", "-", "e"] {
        assert_eq!(
            ResultValue::from_text(word),
            ResultValue::Text(word.into()),
            "{word}"
        );
    }
    assert_eq!(ResultValue::from_text("1e3"), ResultValue::Number(1000.0));
    assert_eq!(
        ResultValue::from_text("2013-07-15 00:00:00").key_text(),
        "2013-07-15"
    );
    assert_eq!(
        ResultValue::from_text("2013-07-15T00:00:00.000").key_text(),
        "2013-07-15"
    );
    assert_eq!(
        ResultValue::from_text("2013-07-15 10:01:00").key_text(),
        "2013-07-15 10:01:00"
    );
    assert_eq!(ResultValue::Number(630_500.0).key_text(), "630500");
}

#[test]
fn a_failed_restart_ends_that_query_and_keeps_the_rest() {
    let directory = TempDirectory::new("restart");
    let drop_caches = directory.0.join("drop_caches");
    let schedule = schedule(2, true);
    let mut engine = ScriptedEngine {
        rows: Some(10),
        fail_restart: true,
        ..ScriptedEngine::default()
    };
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    assert!(engine.asked.is_empty());
    assert_eq!(engine.idle_waits, 1);
    for id in ["1", "3"] {
        let query = result.queries.iter().find(|query| query.id == id).unwrap();
        assert_eq!(query.runs.len(), 1);
        assert!(query.runs[0].cold);
        assert!(query.runs[0]
            .error
            .as_deref()
            .unwrap()
            .contains("server did not come back"));
    }
}

#[test]
fn a_result_is_written_once_and_read_back() {
    let directory = TempDirectory::new("files");
    let run = directory.0.join("run");
    for sub in ["workloads", "data", "plans", "results"] {
        std::fs::create_dir_all(run.join(sub)).unwrap();
    }
    let run_directory = RunDirectory::open(&run).unwrap();
    let drop_caches = directory.0.join("drop_caches");
    let schedule = schedule(1, false);
    let mut engine = ScriptedEngine {
        rows: Some(10),
        ..ScriptedEngine::default()
    };
    let result = run_one_shot_queries(
        &mut engine,
        &workload(),
        &setup(&schedule, None, &drop_caches),
        &mut |_| {},
    )
    .unwrap();
    let path = run_directory.write_one_shot(&result).unwrap();
    assert!(path.ends_with("results/scripted/clickbench__exact.one-shot.json"));
    assert!(run_directory.write_one_shot(&result).is_err());
    assert_eq!(run_directory.read_one_shot_results().unwrap(), vec![result]);
}

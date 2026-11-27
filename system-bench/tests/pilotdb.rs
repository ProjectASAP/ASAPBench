use std::path::PathBuf;

use aqpbm_core::harness::one_shot::{OneShotQueryEngine, ResultValue};
use system_bench::engines_external::pilotdb::{
    parse_driver_answer, PilotDbEngine, PilotDbOnDuckDb,
};

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> TempDirectory {
        let path = std::env::temp_dir().join(format!(
            "system-bench-pilotdb-{}-{test}",
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

#[test]
fn the_driver_answer_keeps_column_order_and_reports_how_it_ran() {
    let run = parse_driver_answer(
        r#"{"columns": ["sum(x)", "count_star()", "avg(y)"], "rows": [[12.5, 3, null]],
            "wall_seconds": 0.9, "cpu_seconds": 2.5,
            "phases_seconds": {"pilot_query_execution": 0.2, "runtime": 0.7},
            "answered_exactly": false, "sample_rate_percent": 1.25}"#,
    )
    .unwrap();
    assert_eq!(run.table.columns, vec!["sum(x)", "count_star()", "avg(y)"]);
    assert_eq!(
        run.table.rows,
        vec![vec![
            ResultValue::Number(12.5),
            ResultValue::Integer(3),
            ResultValue::Null
        ]]
    );
    assert_eq!(run.engine_reported_seconds, Some(0.9));
    assert_eq!(run.engine_reported_cpu_seconds, Some(2.5));
    assert_eq!(run.details["answered_exactly"], false);
    assert_eq!(run.details["sample_rate_percent"], 1.25);
    assert_eq!(run.details["pilotdb_runtime_seconds"], 0.7);
    assert!(parse_driver_answer("Traceback").is_err());
}

#[test]
fn a_real_pilotdb_answers_over_a_large_table_and_cleans_up() {
    let Some(python) = std::env::var_os("AQPBM_PILOTDB_PYTHON") else {
        return;
    };
    let directory = TempDirectory::new("real");
    let database = directory.0.join("t.db");
    let created = std::process::Command::new(&python)
        .arg("-c")
        .arg(format!(
            "import duckdb\nc = duckdb.connect('{}')\n\
             c.execute('CREATE TABLE big AS SELECT range AS id, range % 7 AS k, (range * 7919) % 1000 AS v FROM range(20000000)')\n\
             c.close()",
            database.display()
        ))
        .status()
        .unwrap();
    assert!(created.success());
    let mut engine = PilotDbEngine::new(PilotDbOnDuckDb {
        python: PathBuf::from(python),
        database: database.clone(),
        relative_error: 0.05,
        failure_probability: 0.05,
        threads: Some(4),
        scratch_directory: directory.0.clone(),
    })
    .unwrap();
    assert!(engine.description().version.starts_with("pilotdb"));
    assert_eq!(engine.table_rows("big").unwrap(), Some(20_000_000));
    assert_eq!(engine.table_rows("missing").unwrap(), None);
    let run = engine
        .run_query("SELECT SUM(v), COUNT(*), AVG(v) FROM big")
        .unwrap();
    assert_eq!(run.table.columns.len(), 3);
    let count = run.table.rows[0][1].as_number().unwrap();
    assert!((count - 20_000_000.0).abs() / 20_000_000.0 < 0.2, "{count}");
    assert!(run.engine_reported_seconds.is_some());
    assert!(run.details.contains_key("answered_exactly"));
    assert!(run.details.contains_key("sample_rate_percent"));
    let dates = engine
        .run_query("SELECT DATE '2013-07-15' + CAST(k AS INTEGER) AS d, COUNT(*) FROM big GROUP BY DATE '2013-07-15' + CAST(k AS INTEGER) ORDER BY d")
        .unwrap();
    assert_eq!(dates.table.rows.len(), 7);
    assert_eq!(dates.table.rows[0][0].key_text(), "2013-07-15");
    let leftovers: Vec<_> = std::fs::read_dir(&directory.0)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("pilotdb-"))
        .collect();
    assert!(leftovers.is_empty());
    assert!(engine.run_query("SELECT * FROM missing").is_err());
}

use std::time::Duration;

use aqpbm_core::harness::one_shot::{OneShotQueryEngine, ResultValue};
use system_bench::engines_external::clickhouse_connection::ClickHouseConnection;
use system_bench::engines_external::clickhouse_query_text::{
    ClickHouseQueryTextEngine, ClickHouseQueryTextServer,
};

fn server(
    url: String,
    database: &str,
    restart_command: Option<String>,
) -> ClickHouseQueryTextServer {
    ClickHouseQueryTextServer {
        connection: ClickHouseConnection {
            url,
            statement_timeout: Duration::from_secs(60),
        },
        database: database.into(),
        max_threads: Some(2),
        restart_command,
        ready_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(120),
        settings: vec![("use_query_condition_cache".into(), "0".into())],
    }
}

struct TestDatabase {
    setup: ClickHouseQueryTextEngine,
    name: String,
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = self
            .setup
            .load_table(&[format!("DROP DATABASE IF EXISTS {}", self.name)]);
    }
}

#[test]
fn a_real_server_loads_counts_answers_and_restarts() {
    let Ok(url) = std::env::var("AQPBM_CLICKHOUSE_URL") else {
        return;
    };
    let name = format!("one_shot_test_{}", std::process::id());
    let mut database = TestDatabase {
        setup: ClickHouseQueryTextEngine::new(server(url.clone(), "default", None)).unwrap(),
        name: name.clone(),
    };
    database
        .setup
        .load_table(&[format!("CREATE DATABASE {name}")])
        .unwrap();
    let mut engine = ClickHouseQueryTextEngine::new(server(
        url,
        &name,
        std::env::var("AQPBM_CLICKHOUSE_RESTART").ok(),
    ))
    .unwrap();
    assert_eq!(engine.table_rows("t").unwrap(), None);
    engine
        .load_table(&[
            "CREATE TABLE t (k UInt8, v Float64, s String) ENGINE = MergeTree ORDER BY k".into(),
            "INSERT INTO t VALUES (1, 10, 'a,b'), (1, 20, 'a,b'), (2, 5, 'c')".into(),
        ])
        .unwrap();
    engine.wait_until_idle().unwrap();
    assert_eq!(engine.table_rows("t").unwrap(), Some(3));
    assert!(engine.data_size_bytes("t").unwrap().unwrap() > 0);
    let run = engine
        .run_query("SELECT k, sum(v), any(s), NULL AS n FROM t GROUP BY k ORDER BY k;")
        .unwrap();
    assert_eq!(run.table.columns[0], "k");
    assert_eq!(
        run.table.rows,
        vec![
            vec![
                ResultValue::Integer(1),
                ResultValue::Integer(30),
                ResultValue::Text("a,b".into()),
                ResultValue::Null
            ],
            vec![
                ResultValue::Integer(2),
                ResultValue::Integer(5),
                ResultValue::Text("c".into()),
                ResultValue::Null
            ],
        ]
    );
    assert!(run.engine_reported_seconds.unwrap() > 0.0);
    assert_eq!(run.engine_reported_cpu_seconds, None);
    assert!(engine.last_query_cpu_seconds().is_some());
    assert_eq!(engine.last_query_cpu_seconds(), None);
    assert!(run.details.contains_key("read_bytes"));
    let settings = engine
        .run_query("SELECT getSetting('use_query_condition_cache'), getSetting('use_query_cache')")
        .unwrap();
    assert_eq!(
        settings.table.rows,
        vec![vec![
            ResultValue::Text("false".into()),
            ResultValue::Text("false".into())
        ]]
    );
    let error = engine.run_query("SELECT * FROM missing").unwrap_err();
    assert!(error.0.contains("missing"), "{error}");
    if std::env::var("AQPBM_CLICKHOUSE_RESTART").is_ok() {
        engine.restart().unwrap();
        assert_eq!(engine.table_rows("t").unwrap(), Some(3));
    } else {
        assert!(engine.restart().is_err());
    }
}

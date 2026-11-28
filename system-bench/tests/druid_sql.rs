use std::time::Duration;

use aqpbm_core::harness::one_shot::{OneShotQueryEngine, ResultValue};
use serde_json::json;
use system_bench::engines_external::druid_sql::{
    context_value, is_ingestion, result_table, DruidEngine, DruidServer,
};

#[test]
fn an_answer_keeps_names_and_shows_timestamps_as_sql_text() {
    let table = result_table(&json!([
        ["t", "k", "u", "s"],
        ["TIMESTAMP", "BIGINT", "DOUBLE", "VARCHAR"],
        ["2013-07-15T10:00:00.000Z", 7, 2.5, null],
        ["2013-07-15T00:00:00.000Z", 9007199254740993_i64, 1.0, "a,b"],
    ]))
    .unwrap();
    assert_eq!(table.columns, vec!["t", "k", "u", "s"]);
    assert_eq!(
        table.rows,
        vec![
            vec![
                ResultValue::Text("2013-07-15 10:00:00".into()),
                ResultValue::Integer(7),
                ResultValue::Number(2.5),
                ResultValue::Null
            ],
            vec![
                ResultValue::Text("2013-07-15 00:00:00".into()),
                ResultValue::Integer(9007199254740993),
                ResultValue::Number(1.0),
                ResultValue::Text("a,b".into())
            ],
        ]
    );
    assert_eq!(table.rows[1][0].key_text(), "2013-07-15");
    assert!(result_table(&json!([["EXPR$0"], ["BIGINT"]]))
        .unwrap()
        .rows
        .is_empty());
    assert!(result_table(&json!({"error": "x"})).is_err());
}

#[test]
fn ingestion_is_insert_or_replace_after_any_set_statements() {
    assert!(is_ingestion("REPLACE INTO hits OVERWRITE ALL SELECT 1"));
    assert!(is_ingestion(
        "  SET maxNumTasks = 3; set rowsPerSegment = 5000000;\ninsert INTO hits SELECT 1"
    ));
    assert!(!is_ingestion("SELECT * FROM hits"));
    assert!(!is_ingestion("SET maxNumTasks = 3; SELECT 1"));
}

#[test]
fn context_values_keep_their_json_kind() {
    assert_eq!(context_value("false"), json!(false));
    assert_eq!(context_value("4"), json!(4));
    assert_eq!(context_value("0.5"), json!(0.5));
    assert_eq!(context_value("UTC"), json!("UTC"));
}

fn server(url: String, context: Vec<(&str, serde_json::Value)>) -> DruidServer {
    DruidServer {
        url,
        context: context
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
        statement_timeout: Duration::from_secs(120),
        load_timeout: Duration::from_secs(600),
        restart_command: None,
        ready_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(300),
    }
}

struct TestDatasource {
    url: String,
    name: String,
}

impl Drop for TestDatasource {
    fn drop(&mut self) {
        let _ = ureq::delete(&format!(
            "{}/druid/coordinator/v1/datasources/{}",
            self.url, self.name
        ))
        .call();
    }
}

#[test]
fn a_real_druid_ingests_counts_and_answers_exactly_or_with_its_defaults() {
    let Ok(url) = std::env::var("AQPBM_DRUID_URL") else {
        return;
    };
    let name = format!("one_shot_test_{}", std::process::id());
    let datasource = TestDatasource {
        url: url.clone(),
        name: name.clone(),
    };
    let mut engine = DruidEngine::new(server(datasource.url.clone(), Vec::new())).unwrap();
    assert_eq!(engine.table_rows(&name).unwrap(), None);
    let rows: Vec<String> = (0..2000)
        .map(|row| format!("{},{},{}", 1_373_846_400 + row, row % 3, row))
        .collect();
    engine
        .load_table(&[format!(
            "INSERT INTO \"{name}\" SELECT MILLIS_TO_TIMESTAMP(\"t\" * 1000) AS __time, \"k\", \"u\" \
             FROM TABLE(EXTERN('{}', '{{\"type\": \"csv\", \"findColumnsFromHeader\": false, \
             \"columns\": [\"t\", \"k\", \"u\"]}}')) EXTEND (\"t\" BIGINT, \"k\" BIGINT, \"u\" BIGINT) \
             PARTITIONED BY DAY",
            json!({"type": "inline", "data": rows.join("\n")})
                .to_string()
                .replace('\'', "''")
        )])
        .unwrap();
    assert_eq!(engine.table_rows(&name).unwrap(), Some(2000));
    assert!(engine.data_size_bytes(&name).unwrap().unwrap() > 0);
    let distinct =
        format!("SELECT k, COUNT(DISTINCT u) AS d FROM \"{name}\" GROUP BY k ORDER BY k");
    let mut exact = DruidEngine::new(server(
        url,
        vec![("useApproximateCountDistinct", json!(false))],
    ))
    .unwrap();
    let run = exact.run_query(&distinct).unwrap();
    assert_eq!(
        run.table.rows,
        vec![
            vec![ResultValue::Integer(0), ResultValue::Integer(667)],
            vec![ResultValue::Integer(1), ResultValue::Integer(667)],
            vec![ResultValue::Integer(2), ResultValue::Integer(666)],
        ]
    );
    let first = engine
        .run_query(&format!("SELECT MIN(__time) FROM \"{name}\""))
        .unwrap();
    assert_eq!(
        first.table.rows,
        vec![vec![ResultValue::Text("2013-07-15 00:00:00".into())]]
    );
    assert!(engine.run_query(&distinct).unwrap().table.rows.len() == 3);
    let error = engine.run_query("SELECT * FROM missing").unwrap_err();
    assert!(error.0.contains("missing"), "{error}");
    assert!(engine.restart().is_err());
}

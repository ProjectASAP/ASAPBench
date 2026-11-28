use std::collections::BTreeMap;
use std::path::Path;

use aqpbm_core::harness::engine::{
    LoadStatements, OneShotQuery, QueryLanguage, QueryText, SqlDialect, TableSource, WorkloadSpec,
};

fn sql(text: &str, dialect: Option<SqlDialect>) -> QueryText {
    QueryText {
        language: QueryLanguage::Sql,
        text: text.into(),
        dialect,
    }
}

#[test]
fn a_workload_written_before_tables_existed_still_reads() {
    let workload: WorkloadSpec =
        serde_json::from_str(r#"{"name": "w", "queries": [], "data": []}"#).unwrap();
    assert!(workload.tables.is_empty());
    assert!(workload.one_shot_queries.is_empty());
    let text = serde_json::to_string(&workload).unwrap();
    assert!(!text.contains("tables"), "{text}");
    assert!(!text.contains("one_shot_queries"), "{text}");
}

#[test]
fn a_query_picks_its_dialect_then_plain_sql() {
    let query = OneShotQuery {
        id: "q".into(),
        texts: vec![
            QueryText {
                language: QueryLanguage::PromQl,
                text: "sum(x)".into(),
                dialect: None,
            },
            sql("SELECT 1", None),
            sql("SELECT 2", Some(SqlDialect::new("ClickHouse"))),
        ],
        group_by_column_positions: Vec::new(),
        variants: BTreeMap::new(),
    };
    assert_eq!(
        query.sql_for(&SqlDialect::new("ClickHouse"), None),
        Some("SELECT 2")
    );
    assert_eq!(
        query.sql_for(&SqlDialect::new("DuckDb"), None),
        Some("SELECT 1")
    );
    let only_clickhouse = OneShotQuery {
        texts: vec![sql("SELECT 2", Some(SqlDialect::new("ClickHouse")))],
        ..query
    };
    assert_eq!(
        only_clickhouse.sql_for(&SqlDialect::new("DuckDb"), None),
        None
    );
}

#[test]
fn a_variant_picks_only_its_own_texts() {
    let query: OneShotQuery = serde_json::from_str(
        r#"{"id": "5",
            "texts": [{"language": "Sql", "text": "SELECT COUNT(DISTINCT u) FROM t"}],
            "variants": {"approximate": [
                {"language": "Sql", "dialect": "PostgreSql",
                 "text": "SELECT distinct_count(approx_count_distinct(u)) FROM t"}]}}"#,
    )
    .unwrap();
    assert_eq!(
        query.sql_for(&SqlDialect::new("PostgreSql"), Some("approximate")),
        Some("SELECT distinct_count(approx_count_distinct(u)) FROM t")
    );
    assert_eq!(
        query.sql_for(&SqlDialect::new("PostgreSql"), None),
        Some("SELECT COUNT(DISTINCT u) FROM t")
    );
    assert_eq!(
        query.sql_for(&SqlDialect::new("DuckDb"), Some("approximate")),
        None
    );
    assert_eq!(
        query.sql_for(&SqlDialect::new("PostgreSql"), Some("sampled")),
        None
    );
    let text = serde_json::to_string(&OneShotQuery {
        variants: BTreeMap::new(),
        ..query
    })
    .unwrap();
    assert!(!text.contains("variants"), "{text}");
}

#[test]
fn load_statements_name_the_file() {
    let table = TableSource {
        name: "hits".into(),
        file_name: "hits.parquet".into(),
        url: None,
        rows: 3,
        load_statements: vec![LoadStatements {
            dialect: SqlDialect::new("DuckDb"),
            statements: vec![
                "CREATE TABLE hits (a INT)".into(),
                "INSERT INTO hits SELECT * FROM read_parquet('{file}')".into(),
            ],
        }],
    };
    assert_eq!(
        table.statements_for(&SqlDialect::new("DuckDb"), Path::new("/data/hits.parquet")),
        Some(vec![
            "CREATE TABLE hits (a INT)".to_string(),
            "INSERT INTO hits SELECT * FROM read_parquet('/data/hits.parquet')".to_string(),
        ])
    );
    assert_eq!(
        table.statements_for(
            &SqlDialect::new("ClickHouse"),
            Path::new("/data/hits.parquet")
        ),
        None
    );
}

#[test]
fn a_one_shot_workload_round_trips() {
    let text = r#"{
        "name": "clickbench",
        "queries": [],
        "data": [],
        "tables": [{"name": "hits", "file_name": "hits.parquet", "url": "https://example.org/hits.parquet",
                    "rows": 99997497,
                    "load_statements": [{"dialect": "DuckDb", "statements": ["CREATE TABLE hits (a INT)"]}]}],
        "one_shot_queries": [{"id": "8", "group_by_column_positions": [0],
                              "texts": [{"language": "Sql", "text": "SELECT a, COUNT(*) FROM hits GROUP BY a"}]}]
    }"#;
    let workload: WorkloadSpec = serde_json::from_str(text).unwrap();
    assert_eq!(workload.tables[0].rows, 99_997_497);
    assert_eq!(
        workload.one_shot_queries[0].group_by_column_positions,
        vec![0]
    );
    let again: WorkloadSpec =
        serde_json::from_str(&serde_json::to_string(&workload).unwrap()).unwrap();
    assert_eq!(again, workload);
}

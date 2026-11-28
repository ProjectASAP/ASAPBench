use std::path::PathBuf;
use std::time::Duration;

use aqpbm_core::harness::one_shot::{OneShotQueryEngine, ResultValue};
use system_bench::engines_external::postgresql_command_line::{
    psql_script, split_timing_lines, PostgreSqlCommandLine, PostgreSqlEngine,
};

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> TempDirectory {
        let path = std::env::temp_dir().join(format!(
            "system-bench-postgresql-{}-{test}",
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

fn command_line(program: PathBuf, connection: String) -> PostgreSqlCommandLine {
    PostgreSqlCommandLine {
        program,
        connection,
        settings: vec![("jit".into(), "off".into())],
        restart_command: None,
        ready_timeout: Duration::from_secs(30),
    }
}

#[test]
fn timing_lines_are_summed_and_taken_out_of_the_result() {
    let (text, seconds) =
        split_timing_lines("a,b\n1,2\nTime: 250.500 ms\nTime: 1234.500 ms (00:01.235)\n");
    assert_eq!(text, "a,b\n1,2\n");
    assert!((seconds.unwrap() - 1.485).abs() < 1e-12);
    assert_eq!(split_timing_lines("a\n1\n").1, None);
}

#[test]
fn a_load_script_ends_statements_but_not_meta_commands() {
    assert_eq!(
        psql_script(&[
            "CREATE TABLE t (k INT)".into(),
            "\\copy t FROM 'x.csv' WITH (FORMAT csv)".into(),
            "VACUUM ANALYZE t;".into(),
        ]),
        "CREATE TABLE t (k INT);\n\\copy t FROM 'x.csv' WITH (FORMAT csv)\nVACUUM ANALYZE t;\n"
    );
}

#[cfg(unix)]
#[test]
fn a_query_is_csv_with_settings_then_the_timing() {
    use std::io::Write;
    let directory = TempDirectory::new("stub");
    let arguments = directory.0.join("arguments");
    let program = directory.0.join("psql");
    let mut writer = std::process::Command::new("sh")
        .arg("-c")
        .arg("cat > \"$0\" && chmod 755 \"$0\"")
        .arg(&program)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writer
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'psql (PostgreSQL) 99.9'; exit 0; fi\n\
                 for a in \"$@\"; do printf '%s\\n' \"$a\"; done > {}\n\
                 printf 'k,count\\n7,3\\n\\\\N,1\\nTime: 125.000 ms\\n'\n",
                arguments.display()
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(writer.wait().unwrap().success());
    let mut engine =
        PostgreSqlEngine::new(command_line(program, "host=db dbname=bench".into())).unwrap();
    let run = engine
        .run_query("SELECT k, count(*) FROM t GROUP BY k")
        .unwrap();
    assert_eq!(run.table.columns, vec!["k", "count"]);
    assert_eq!(run.table.rows[1][0], ResultValue::Null);
    assert_eq!(run.engine_reported_seconds, Some(0.125));
    let recorded = std::fs::read_to_string(&arguments).unwrap();
    assert_eq!(
        recorded.lines().collect::<Vec<_>>(),
        vec![
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-d",
            "host=db dbname=bench",
            "--csv",
            "-c",
            "\\pset null '\\\\N'",
            "-c",
            "SET \"jit\" = 'off'",
            "-c",
            "\\timing on",
            "-c",
            "SELECT k, count(*) FROM t GROUP BY k",
        ]
    );
}

struct TestTable {
    engine: PostgreSqlEngine,
    name: String,
}

impl Drop for TestTable {
    fn drop(&mut self) {
        let _ = self
            .engine
            .load_table(&[format!("DROP TABLE IF EXISTS {}", self.name)]);
    }
}

#[test]
fn a_real_server_loads_counts_and_answers() {
    let Ok(connection) = std::env::var("AQPBM_POSTGRESQL_CONNECTION") else {
        return;
    };
    let program =
        std::env::var_os("AQPBM_PSQL").map_or_else(|| PathBuf::from("psql"), PathBuf::from);
    let name = format!("one_shot_test_{}", std::process::id());
    let mut table = TestTable {
        engine: PostgreSqlEngine::new(command_line(program, connection)).unwrap(),
        name: name.clone(),
    };
    let engine = &mut table.engine;
    assert_eq!(engine.table_rows(&name).unwrap(), None);
    engine
        .load_table(&[
            format!("CREATE TABLE {name} (k INTEGER, v DOUBLE PRECISION, s TEXT)"),
            format!("INSERT INTO {name} VALUES (1, 10, 'a,b'), (1, 20, ''), (2, 5, NULL)"),
        ])
        .unwrap();
    assert_eq!(engine.table_rows(&name).unwrap(), Some(3));
    assert!(engine.data_size_bytes(&name).unwrap().unwrap() > 0);
    let run = engine
        .run_query(&format!(
            "SELECT k, sum(v), max(s), NULL AS n FROM {name} GROUP BY k ORDER BY k"
        ))
        .unwrap();
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
                ResultValue::Null,
                ResultValue::Null
            ],
        ]
    );
    assert!(run.engine_reported_seconds.unwrap() > 0.0);
    assert!(engine.description().version.starts_with("PostgreSQL "));
    let error = engine.run_query("SELECT * FROM missing").unwrap_err();
    assert!(error.0.contains("missing"), "{error}");
    assert!(engine.restart().is_err());
}

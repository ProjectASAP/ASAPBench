use std::path::PathBuf;

use aqpbm_core::harness::one_shot::{OneShotQueryEngine, ResultValue};
use system_bench::engines_external::duckdb_command_line::{
    split_timer_lines, DuckDbCommandLine, DuckDbEngine, DuckDbTimer,
};

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(test: &str) -> TempDirectory {
        let path =
            std::env::temp_dir().join(format!("system-bench-duckdb-{}-{test}", std::process::id()));
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
fn timer_lines_are_summed_and_taken_out_of_the_result() {
    let (text, timer) = split_timer_lines(
        "a,b\n1,2\nRun Time (s): real 0.250 user 0.500000 sys 0.100000\nRun Time (s): real 0.050 user 0.010000 sys 0.000000\n",
    );
    assert_eq!(text, "a,b\n1,2\n");
    let DuckDbTimer {
        real_seconds,
        cpu_seconds,
    } = timer.unwrap();
    assert!((real_seconds - 0.3).abs() < 1e-12);
    assert!((cpu_seconds - 0.61).abs() < 1e-12);
    assert_eq!(split_timer_lines("a\n1\n").1, None);
}

#[cfg(unix)]
#[test]
fn the_command_line_is_read_only_csv_with_threads_then_the_timer() {
    use std::io::Write;
    let directory = TempDirectory::new("stub");
    let arguments = directory.0.join("arguments");
    let program = directory.0.join("duckdb");
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
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'v9.9.9 stub'; exit 0; fi\n\
                 for a in \"$@\"; do printf '%s\\n' \"$a\"; done > {}\n\
                 printf 'k,count_star()\\n7,3\\n\\\\N,1\\nRun Time (s): real 0.125 user 0.250000 sys 0.000000\\n'\n",
                arguments.display()
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(writer.wait().unwrap().success());
    let mut engine = DuckDbEngine::new(DuckDbCommandLine {
        program: program.clone(),
        database: directory.0.join("hits.db"),
        threads: Some(16),
        storage_version: None,
    })
    .unwrap();
    assert_eq!(engine.description().version, "v9.9.9 stub");
    let run = engine
        .run_query("SELECT k, COUNT(*) FROM t GROUP BY k")
        .unwrap();
    assert_eq!(run.table.columns, vec!["k", "count_star()"]);
    assert_eq!(run.table.rows[1][0], ResultValue::Null);
    assert_eq!(run.engine_reported_seconds, Some(0.125));
    assert_eq!(run.engine_reported_cpu_seconds, Some(0.25));
    let recorded = std::fs::read_to_string(&arguments).unwrap();
    assert_eq!(
        recorded.lines().collect::<Vec<_>>(),
        vec![
            "-readonly",
            "-csv",
            "-nullvalue",
            "\\N",
            directory.0.join("hits.db").to_str().unwrap(),
            "-c",
            "SET threads = 16",
            "-c",
            ".timer on",
            "-c",
            "SELECT k, COUNT(*) FROM t GROUP BY k",
        ]
    );
    assert_eq!(engine.table_rows("t").unwrap(), None);
}

#[test]
fn a_real_duckdb_loads_counts_and_answers() {
    let Some(program) = std::env::var_os("AQPBM_DUCKDB") else {
        return;
    };
    let directory = TempDirectory::new("real");
    let csv = directory.0.join("source.csv");
    std::fs::write(&csv, "k,v\n1,10\n1,20\n2,5\n").unwrap();
    let mut engine = DuckDbEngine::new(DuckDbCommandLine {
        program: PathBuf::from(program),
        database: directory.0.join("t.db"),
        threads: Some(2),
        storage_version: Some("latest".into()),
    })
    .unwrap();
    assert_eq!(engine.table_rows("t").unwrap(), None);
    engine
        .load_table(&[
            "CREATE TABLE t (k INTEGER, v DOUBLE)".into(),
            format!("INSERT INTO t SELECT * FROM read_csv('{}')", csv.display()),
        ])
        .unwrap();
    assert_eq!(engine.table_rows("t").unwrap(), Some(3));
    assert!(engine.data_size_bytes("t").unwrap().unwrap() > 0);
    let run = engine
        .run_query("SELECT k, SUM(v), NULL AS n FROM t GROUP BY k ORDER BY k")
        .unwrap();
    assert_eq!(
        run.table.rows,
        vec![
            vec![
                ResultValue::Integer(1),
                ResultValue::Number(30.0),
                ResultValue::Null
            ],
            vec![
                ResultValue::Integer(2),
                ResultValue::Number(5.0),
                ResultValue::Null
            ],
        ]
    );
    assert!(run.engine_reported_seconds.is_some());
    let error = engine.run_query("SELECT * FROM missing").unwrap_err();
    assert!(error.0.contains("missing"), "{error}");
}

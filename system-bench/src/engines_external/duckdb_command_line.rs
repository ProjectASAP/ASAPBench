use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use aqpbm_core::harness::engine::{EngineDescription, EngineError, SqlDialect};
use aqpbm_core::harness::one_shot::{OneShotQueryEngine, QueryRun, ResultTable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuckDbCommandLine {
    pub program: PathBuf,
    pub database: PathBuf,
    pub threads: Option<u32>,
    pub storage_version: Option<String>,
}

pub struct DuckDbEngine {
    command_line: DuckDbCommandLine,
    version: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckDbTimer {
    pub real_seconds: f64,
    pub cpu_seconds: f64,
}

pub fn split_timer_lines(output: &str) -> (String, Option<DuckDbTimer>) {
    let mut kept = String::new();
    let mut timer: Option<DuckDbTimer> = None;
    for line in output.lines() {
        match line.strip_prefix("Run Time (s): ") {
            Some(times) => {
                let words: Vec<&str> = times.split_whitespace().collect();
                let value = |name: &str| {
                    words
                        .iter()
                        .position(|word| *word == name)
                        .and_then(|at| words.get(at + 1))
                        .and_then(|value| value.parse::<f64>().ok())
                };
                if let (Some(real), Some(user), Some(system)) =
                    (value("real"), value("user"), value("sys"))
                {
                    let before = timer.unwrap_or(DuckDbTimer {
                        real_seconds: 0.0,
                        cpu_seconds: 0.0,
                    });
                    timer = Some(DuckDbTimer {
                        real_seconds: before.real_seconds + real,
                        cpu_seconds: before.cpu_seconds + user + system,
                    });
                }
            }
            None => {
                kept.push_str(line);
                kept.push('\n');
            }
        }
    }
    (kept, timer)
}

impl DuckDbEngine {
    pub fn new(command_line: DuckDbCommandLine) -> Result<DuckDbEngine, EngineError> {
        let output = Command::new(&command_line.program)
            .arg("--version")
            .output()
            .map_err(|error| EngineError(format!("{}: {error}", command_line.program.display())))?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(DuckDbEngine {
            command_line,
            version,
        })
    }

    fn run(&self, read_only: bool, statements: &[&str]) -> Result<String, EngineError> {
        let mut command = Command::new(&self.command_line.program);
        if read_only {
            command.arg("-readonly");
        } else if let Some(version) = &self.command_line.storage_version {
            command.args(["-storage_version", version]);
        }
        command
            .args(["-csv", "-nullvalue", "\\N"])
            .arg(&self.command_line.database);
        if let Some(threads) = self.command_line.threads {
            command.arg("-c").arg(format!("SET threads = {threads}"));
        }
        command.args(["-c", ".timer on"]);
        for statement in statements {
            command.arg("-c").arg(statement);
        }
        let output = command.output().map_err(|error| {
            EngineError(format!("{}: {error}", self.command_line.program.display()))
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EngineError(format!(
                "DuckDB exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(stdout)
    }

    fn single_number(&self, sql: &str) -> Result<u64, EngineError> {
        let (text, _) = split_timer_lines(&self.run(true, &[sql])?);
        let table = ResultTable::from_csv_with_header(&text)?;
        table
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(|value| value.as_number())
            .map(|number| number as u64)
            .ok_or_else(|| EngineError(format!("no number from {sql}: {text}")))
    }
}

fn quoted_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn quoted_name(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

impl OneShotQueryEngine for DuckDbEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "duckdb".into(),
            version: self.version.clone(),
            version_dirty: false,
            build_profile: match self.command_line.threads {
                Some(threads) => format!("command line, threads={threads}"),
                None => "command line".into(),
            },
        }
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::DuckDb
    }

    fn configuration(&self) -> serde_json::Value {
        serde_json::json!({
            "database": self.command_line.database,
            "threads": self.command_line.threads,
            "storage_version": self.command_line.storage_version,
        })
    }

    fn table_rows(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        if !self.command_line.database.exists() {
            return Ok(None);
        }
        let tables = self.single_number(&format!(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = {}",
            quoted_text(table)
        ))?;
        if tables == 0 {
            return Ok(None);
        }
        self.single_number(&format!("SELECT count(*) FROM {}", quoted_name(table)))
            .map(Some)
    }

    fn load_table(&mut self, statements: &[String]) -> Result<(), EngineError> {
        let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
        self.run(false, &statements).map(|_| ())
    }

    fn data_size_bytes(&mut self, _table: &str) -> Result<Option<u64>, EngineError> {
        let database = &self.command_line.database;
        let mut wal = database.as_os_str().to_owned();
        wal.push(".wal");
        Ok(Some(file_bytes(database) + file_bytes(Path::new(&wal))))
    }

    fn restart(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryRun, EngineError> {
        let (text, timer) = split_timer_lines(&self.run(true, &[sql])?);
        Ok(QueryRun {
            table: ResultTable::from_csv_with_header(&text)?,
            engine_reported_seconds: timer.map(|timer| timer.real_seconds),
            engine_reported_cpu_seconds: timer.map(|timer| timer.cpu_seconds),
            details: BTreeMap::new(),
        })
    }
}

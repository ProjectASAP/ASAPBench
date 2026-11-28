use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::duckdb_command_line::DuckDbDatabaseOptions;
use aqpbm_core::harness::engine::{EngineDescription, EngineError, SqlDialect};
use aqpbm_core::harness::one_shot::{
    ErrorTarget, OneShotQueryEngine, QueryRun, ResultTable, ResultValue,
};
use serde::Deserialize;

const DRIVER: &str = include_str!("pilotdb_duckdb.py");

const VERSION: &str = "import importlib.metadata as m, os, subprocess, pilotdb, duckdb
root = os.path.dirname(os.path.dirname(pilotdb.__file__))
commit = subprocess.run(['git', '-C', root, 'rev-parse', '--short', 'HEAD'], capture_output=True, text=True).stdout.strip()
print(f\"pilotdb {m.version('pilotdb')} {commit or 'unknown commit'}, duckdb {duckdb.__version__}\")";

#[derive(Debug, Clone, PartialEq)]
pub struct PilotDbOnDuckDb {
    pub python: PathBuf,
    pub database: PathBuf,
    pub relative_error: f64,
    pub failure_probability: f64,
    pub threads: Option<u32>,
    pub scratch_directory: PathBuf,
}

pub struct PilotDbEngine {
    setup: PilotDbOnDuckDb,
    version: String,
}

#[derive(Debug, Deserialize)]
pub struct DriverAnswer {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub wall_seconds: f64,
    pub cpu_seconds: f64,
    pub phases_seconds: BTreeMap<String, f64>,
    pub answered_exactly: bool,
    pub sample_rate_percent: Option<f64>,
}

impl DriverAnswer {
    pub fn into_query_run(self) -> QueryRun {
        let table = ResultTable {
            columns: self.columns,
            rows: self
                .rows
                .iter()
                .map(|row| row.iter().map(ResultValue::from_json).collect())
                .collect(),
        };
        let mut details = BTreeMap::new();
        details.insert("answered_exactly".to_string(), self.answered_exactly.into());
        details.insert(
            "sample_rate_percent".to_string(),
            self.sample_rate_percent.into(),
        );
        details.insert(
            "phases_seconds".to_string(),
            serde_json::to_value(&self.phases_seconds).unwrap_or_default(),
        );
        if let Some(runtime) = self.phases_seconds.get("runtime") {
            details.insert("pilotdb_runtime_seconds".to_string(), (*runtime).into());
        }
        QueryRun {
            table,
            engine_reported_seconds: Some(self.wall_seconds),
            engine_reported_cpu_seconds: Some(self.cpu_seconds),
            details,
        }
    }
}

static CALL_COUNT: AtomicU64 = AtomicU64::new(0);

struct ScratchDirectory(PathBuf);

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

impl PilotDbEngine {
    pub fn new(setup: PilotDbOnDuckDb) -> Result<PilotDbEngine, EngineError> {
        let output = Command::new(&setup.python)
            .args(["-c", VERSION])
            .output()
            .map_err(|error| EngineError(format!("{}: {error}", setup.python.display())))?;
        if !output.status.success() {
            return Err(EngineError(format!(
                "{} cannot import pilotdb and duckdb: {}",
                setup.python.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let version = last_line(&String::from_utf8_lossy(&output.stdout)).to_string();
        Ok(PilotDbEngine { setup, version })
    }

    fn driver(&self, arguments: &[&str]) -> Result<String, EngineError> {
        let scratch = ScratchDirectory(self.setup.scratch_directory.join(format!(
            "pilotdb-{}-{}",
            std::process::id(),
            CALL_COUNT.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::create_dir_all(&scratch.0)
            .map_err(|error| EngineError(format!("{}: {error}", scratch.0.display())))?;
        let output = Command::new(&self.setup.python)
            .arg("-c")
            .arg(DRIVER)
            .args(arguments)
            .current_dir(&scratch.0)
            .output()
            .map_err(|error| EngineError(format!("{}: {error}", self.setup.python.display())))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
            return Err(EngineError(format!(
                "PilotDB exited with {}: {}",
                output.status,
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            )));
        }
        Ok(last_line(&String::from_utf8_lossy(&output.stdout)).to_string())
    }

    fn database(&self) -> &str {
        self.setup.database.to_str().unwrap_or_default()
    }
}

pub fn parse_driver_answer(line: &str) -> Result<QueryRun, EngineError> {
    serde_json::from_str::<DriverAnswer>(line)
        .map(DriverAnswer::into_query_run)
        .map_err(|error| EngineError(format!("PilotDB's answer {line}: {error}")))
}

impl OneShotQueryEngine for PilotDbEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "pilotdb".into(),
            version: self.version.clone(),
            version_dirty: false,
            build_profile: match self.setup.threads {
                Some(threads) => format!("on DuckDB, threads={threads}"),
                None => "on DuckDB".into(),
            },
        }
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::new(super::duckdb_command_line::DIALECT)
    }

    fn configuration(&self) -> serde_json::Value {
        serde_json::json!({
            "database": self.setup.database,
            "relative_error": self.setup.relative_error,
            "failure_probability": self.setup.failure_probability,
            "threads": self.setup.threads,
        })
    }

    fn error_target(&self) -> Option<ErrorTarget> {
        Some(ErrorTarget {
            relative_error: self.setup.relative_error,
            failure_probability: self.setup.failure_probability,
        })
    }

    fn table_rows(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        if !Path::new(&self.setup.database).exists() {
            return Ok(None);
        }
        let line = self.driver(&["table-rows", self.database(), table])?;
        #[derive(Deserialize)]
        struct Rows {
            rows: Option<u64>,
        }
        serde_json::from_str::<Rows>(&line)
            .map(|rows| rows.rows)
            .map_err(|error| EngineError(format!("PilotDB's row count {line}: {error}")))
    }

    fn restart(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryRun, EngineError> {
        let relative_error = self.setup.relative_error.to_string();
        let failure_probability = self.setup.failure_probability.to_string();
        let threads = self
            .setup
            .threads
            .map_or_else(String::new, |threads| threads.to_string());
        let line = self.driver(&[
            "answer",
            self.database(),
            sql,
            &relative_error,
            &failure_probability,
            &threads,
        ])?;
        parse_driver_answer(&line)
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct PilotDbOptions {
    #[arg(long, default_value = "python3")]
    pub pilotdb_python: PathBuf,
    #[command(flatten)]
    pub database: DuckDbDatabaseOptions,
    #[arg(long, default_value_t = 0.05)]
    pub pilotdb_relative_error: f64,
    #[arg(long, default_value_t = 0.05)]
    pub pilotdb_failure_probability: f64,
}

pub fn one_shot_queries(matches: &clap::ArgMatches) -> anyhow::Result<Box<dyn OneShotQueryEngine>> {
    let options = <PilotDbOptions as clap::FromArgMatches>::from_arg_matches(matches)?;
    Ok(Box::new(PilotDbEngine::new(PilotDbOnDuckDb {
        python: options.pilotdb_python,
        database: options.database.database("pilotdb")?,
        relative_error: options.pilotdb_relative_error,
        failure_probability: options.pilotdb_failure_probability,
        threads: options.database.duckdb_threads,
        scratch_directory: std::env::temp_dir(),
    })?))
}

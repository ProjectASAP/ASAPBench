use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::harness::engine::{EngineDescription, EngineError, SqlDialect, WorkloadSpec};

pub const LINUX_DROP_CACHES: &str = "/proc/sys/vm/drop_caches";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResultValue {
    Null,
    Integer(i64),
    Number(f64),
    Text(String),
}

fn looks_numeric(text: &str) -> bool {
    text.bytes().any(|byte| byte.is_ascii_digit())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'))
}

fn midnight_as_date(text: &str) -> &str {
    let bytes = text.as_bytes();
    let is_date = bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                *byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        });
    if !is_date {
        return text;
    }
    let midnight = text[10..]
        .strip_prefix(' ')
        .or_else(|| text[10..].strip_prefix('T'))
        .and_then(|time| time.strip_prefix("00:00:00"))
        .is_some_and(|fraction| {
            fraction.is_empty()
                || fraction
                    .strip_prefix('.')
                    .is_some_and(|zeros| zeros.bytes().all(|byte| byte == b'0'))
        });
    if midnight {
        &text[..10]
    } else {
        text
    }
}

impl ResultValue {
    pub fn from_text(text: &str) -> ResultValue {
        if text == "\\N" {
            return ResultValue::Null;
        }
        if !looks_numeric(text) {
            return ResultValue::Text(text.to_string());
        }
        if let Ok(integer) = text.parse::<i64>() {
            return ResultValue::Integer(integer);
        }
        match text.parse::<f64>() {
            Ok(number) => ResultValue::Number(number),
            Err(_) => ResultValue::Text(text.to_string()),
        }
    }

    pub fn from_json(value: &serde_json::Value) -> ResultValue {
        match value {
            serde_json::Value::Null => ResultValue::Null,
            serde_json::Value::Bool(flag) => ResultValue::Integer(i64::from(*flag)),
            serde_json::Value::Number(number) => match number.as_i64() {
                Some(integer) => ResultValue::Integer(integer),
                None => number
                    .as_f64()
                    .map_or(ResultValue::Null, ResultValue::Number),
            },
            serde_json::Value::String(text) => ResultValue::from_text(text),
            other => ResultValue::Text(other.to_string()),
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            ResultValue::Integer(integer) => Some(*integer as f64),
            ResultValue::Number(number) => Some(*number),
            _ => None,
        }
    }

    pub fn key_text(&self) -> String {
        match self {
            ResultValue::Null => "\\N".to_string(),
            ResultValue::Integer(integer) => integer.to_string(),
            ResultValue::Number(number) if number.fract() == 0.0 && number.abs() < 1e15 => {
                format!("{}", *number as i64)
            }
            ResultValue::Number(number) => format!("{number}"),
            ResultValue::Text(text) => midnight_as_date(text).to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ResultValue>>,
}

impl ResultTable {
    pub fn from_csv_with_header(text: &str) -> Result<ResultTable, EngineError> {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(false)
            .from_reader(text.as_bytes());
        let columns = reader
            .headers()
            .map_err(|error| EngineError(format!("result header: {error}")))?
            .iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        for record in reader.records() {
            let record = record.map_err(|error| EngineError(format!("result row: {error}")))?;
            rows.push(record.iter().map(ResultValue::from_text).collect());
        }
        Ok(ResultTable { columns, rows })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorTarget {
    pub relative_error: f64,
    pub failure_probability: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryRun {
    pub table: ResultTable,
    pub engine_reported_seconds: Option<f64>,
    pub engine_reported_cpu_seconds: Option<f64>,
    pub details: BTreeMap<String, serde_json::Value>,
}

pub trait OneShotQueryEngine {
    fn description(&self) -> EngineDescription;
    fn dialect(&self) -> SqlDialect;
    fn configuration(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn error_target(&self) -> Option<ErrorTarget> {
        None
    }
    fn table_rows(&mut self, table: &str) -> Result<Option<u64>, EngineError>;
    fn load_table(&mut self, statements: &[String]) -> Result<(), EngineError> {
        let _ = statements;
        Err(EngineError(format!(
            "{} does not load tables; load them with the engine whose database it reads",
            self.description().id
        )))
    }
    fn data_size_bytes(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        let _ = table;
        Ok(None)
    }
    fn restart(&mut self) -> Result<(), EngineError>;
    fn run_query(&mut self, sql: &str) -> Result<QueryRun, EngineError>;
    fn last_query_cpu_seconds(&mut self) -> Option<f64> {
        None
    }
    fn wait_until_idle(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OneShotSchedule {
    pub runs: u32,
    pub cold_first_run: bool,
    pub queries: Vec<String>,
    pub query_variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OneShotResult {
    pub engine: EngineDescription,
    pub workload: String,
    pub configuration: String,
    pub configuration_options: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_target: Option<ErrorTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_variant: Option<String>,
    pub cold_first_run: bool,
    pub tables: Vec<TableRecord>,
    pub queries: Vec<QueryRecord>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub cpu_frequency_governors: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableRecord {
    pub name: String,
    pub rows: u64,
    pub loaded_in_this_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by_column_positions: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub runs: Vec<RunRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub cold: bool,
    pub wall_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_reported_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_reported_cpu_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OneShotError {
    #[error("{0}")]
    Engine(#[from] EngineError),
    #[error("workload {workload} has no query {query}")]
    UnknownQuery { workload: String, query: String },
    #[error("workload {0} has no one-shot queries")]
    NoQueries(String),
    #[error("no query of workload {workload} has a {variant} variant")]
    NoVariant { workload: String, variant: String },
    #[error("runs must be at least 1")]
    NoRuns,
    #[error("table {table} holds {held} rows, the workload expects {expected}; drop it to load it again")]
    RowsDiffer {
        table: String,
        held: u64,
        expected: u64,
    },
    #[error("table {table} is not loaded and no data directory was given to load it from")]
    NoDataDirectory { table: String },
    #[error("table {table}: no source file at {path}")]
    NoSourceFile { table: String, path: PathBuf },
    #[error("table {table} has no load statements for {dialect:?}")]
    NoLoadStatements { table: String, dialect: SqlDialect },
    #[error("a cold run drops the page cache through {path}: {source}{}", root_hint(.source))]
    DropPageCache {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn root_hint(source: &std::io::Error) -> &'static str {
    if source.kind() == std::io::ErrorKind::PermissionDenied {
        "; dropping the page cache needs root"
    } else {
        ""
    }
}

pub fn drop_page_cache(drop_caches: &Path) -> Result<(), OneShotError> {
    #[cfg(unix)]
    unsafe {
        libc::sync();
    }
    std::fs::write(drop_caches, "3\n").map_err(|source| OneShotError::DropPageCache {
        path: drop_caches.to_path_buf(),
        source,
    })
}

pub struct OneShotSetup<'a> {
    pub configuration: &'a str,
    pub schedule: &'a OneShotSchedule,
    pub data_directory: Option<&'a Path>,
    pub drop_caches: &'a Path,
}

pub fn run_one_shot_queries(
    engine: &mut dyn OneShotQueryEngine,
    workload: &WorkloadSpec,
    setup: &OneShotSetup<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<OneShotResult, OneShotError> {
    let schedule = setup.schedule;
    if schedule.runs == 0 {
        return Err(OneShotError::NoRuns);
    }
    if workload.one_shot_queries.is_empty() {
        return Err(OneShotError::NoQueries(workload.name.clone()));
    }
    for id in &schedule.queries {
        if !workload
            .one_shot_queries
            .iter()
            .any(|query| &query.id == id)
        {
            return Err(OneShotError::UnknownQuery {
                workload: workload.name.clone(),
                query: id.clone(),
            });
        }
    }
    let variant = schedule.query_variant.as_deref();
    if let Some(variant) = variant {
        if !workload
            .one_shot_queries
            .iter()
            .any(|query| query.variants.contains_key(variant))
        {
            return Err(OneShotError::NoVariant {
                workload: workload.name.clone(),
                variant: variant.to_string(),
            });
        }
    }
    let dialect = engine.dialect();
    let mut tables = Vec::with_capacity(workload.tables.len());
    for table in &workload.tables {
        let held = engine.table_rows(&table.name)?;
        let (loaded_in_this_run, load_seconds) = match held {
            Some(rows) if rows == table.rows => (false, None),
            Some(rows) => {
                return Err(OneShotError::RowsDiffer {
                    table: table.name.clone(),
                    held: rows,
                    expected: table.rows,
                })
            }
            None => {
                let directory =
                    setup
                        .data_directory
                        .ok_or_else(|| OneShotError::NoDataDirectory {
                            table: table.name.clone(),
                        })?;
                let file = std::path::absolute(directory.join(&table.file_name))
                    .unwrap_or_else(|_| directory.join(&table.file_name));
                if !file.is_file() {
                    return Err(OneShotError::NoSourceFile {
                        table: table.name.clone(),
                        path: file,
                    });
                }
                let statements = table.statements_for(&dialect, &file).ok_or_else(|| {
                    OneShotError::NoLoadStatements {
                        table: table.name.clone(),
                        dialect: dialect.clone(),
                    }
                })?;
                progress(&format!("loading {} from {}", table.name, file.display()));
                let start = Instant::now();
                engine.load_table(&statements)?;
                let seconds = start.elapsed().as_secs_f64();
                let rows = engine.table_rows(&table.name)?.unwrap_or(0);
                if rows != table.rows {
                    return Err(OneShotError::RowsDiffer {
                        table: table.name.clone(),
                        held: rows,
                        expected: table.rows,
                    });
                }
                (true, Some(seconds))
            }
        };
        tables.push(TableRecord {
            name: table.name.clone(),
            rows: table.rows,
            loaded_in_this_run,
            load_seconds,
            data_size_bytes: engine.data_size_bytes(&table.name)?,
        });
    }
    engine.wait_until_idle()?;
    let mut queries = Vec::new();
    for query in &workload.one_shot_queries {
        if !schedule.queries.is_empty() && !schedule.queries.contains(&query.id) {
            continue;
        }
        if variant.is_some_and(|variant| !query.variants.contains_key(variant)) {
            continue;
        }
        let Some(sql) = query.sql_for(&dialect, variant) else {
            queries.push(QueryRecord {
                id: query.id.clone(),
                group_by_column_positions: query.group_by_column_positions.clone(),
                sql: None,
                skipped: Some(match variant {
                    Some(variant) => format!("no {variant} SQL text for {dialect:?}"),
                    None => format!("no SQL text for {dialect:?}"),
                }),
                runs: Vec::new(),
            });
            continue;
        };
        let mut runs = Vec::with_capacity(schedule.runs as usize);
        for run in 0..schedule.runs {
            let cold = schedule.cold_first_run && run == 0;
            let prepared = if cold {
                engine
                    .restart()
                    .map_err(|error| format!("restarting for a cold run: {error}"))
                    .and_then(|()| {
                        drop_page_cache(setup.drop_caches).map_err(|error| error.to_string())
                    })
            } else {
                Ok(())
            };
            if let Err(error) = prepared {
                runs.push(RunRecord {
                    cold,
                    wall_seconds: 0.0,
                    engine_reported_seconds: None,
                    engine_reported_cpu_seconds: None,
                    details: BTreeMap::new(),
                    result: None,
                    error: Some(error),
                });
                break;
            }
            let start = Instant::now();
            let outcome = engine.run_query(sql);
            let wall_seconds = start.elapsed().as_secs_f64();
            match outcome {
                Ok(answer) => runs.push(RunRecord {
                    cold,
                    wall_seconds,
                    engine_reported_seconds: answer.engine_reported_seconds,
                    engine_reported_cpu_seconds: answer
                        .engine_reported_cpu_seconds
                        .or_else(|| engine.last_query_cpu_seconds()),
                    details: answer.details,
                    result: Some(answer.table),
                    error: None,
                }),
                Err(error) => {
                    runs.push(RunRecord {
                        cold,
                        wall_seconds,
                        engine_reported_seconds: None,
                        engine_reported_cpu_seconds: None,
                        details: BTreeMap::new(),
                        result: None,
                        error: Some(error.0),
                    });
                    break;
                }
            }
        }
        let last = runs.last().expect("at least one run");
        progress(&format!(
            "query {}: {}",
            query.id,
            match &last.error {
                Some(error) => format!("failed: {error}"),
                None => format!(
                    "{} runs, last {:.3} s",
                    runs.len(),
                    last.engine_reported_seconds.unwrap_or(last.wall_seconds)
                ),
            }
        ));
        queries.push(QueryRecord {
            id: query.id.clone(),
            group_by_column_positions: query.group_by_column_positions.clone(),
            sql: Some(sql.to_string()),
            skipped: None,
            runs,
        });
    }
    Ok(OneShotResult {
        engine: engine.description(),
        workload: workload.name.clone(),
        configuration: setup.configuration.to_string(),
        configuration_options: engine.configuration(),
        error_target: engine.error_target(),
        query_variant: schedule.query_variant.clone(),
        cold_first_run: schedule.cold_first_run,
        tables,
        queries,
        cpu_frequency_governors: BTreeSet::new(),
    })
}

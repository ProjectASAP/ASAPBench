use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use aqpbm_core::harness::engine::{EngineDescription, EngineError, SqlDialect};
use aqpbm_core::harness::one_shot::{OneShotQueryEngine, QueryRun, ResultTable};

use super::restart_command::restart_and_wait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgreSqlCommandLine {
    pub program: PathBuf,
    pub connection: String,
    pub settings: Vec<(String, String)>,
    pub restart_command: Option<String>,
    pub ready_timeout: Duration,
}

pub struct PostgreSqlEngine {
    command_line: PostgreSqlCommandLine,
    client_version: String,
}

pub fn split_timing_lines(output: &str) -> (String, Option<f64>) {
    let mut kept = String::new();
    let mut milliseconds: Option<f64> = None;
    for line in output.lines() {
        let timing = line
            .strip_prefix("Time: ")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|number| number.parse::<f64>().ok());
        match timing {
            Some(value) => milliseconds = Some(milliseconds.unwrap_or(0.0) + value),
            None => {
                kept.push_str(line);
                kept.push('\n');
            }
        }
    }
    (kept, milliseconds.map(|milliseconds| milliseconds / 1e3))
}

pub fn psql_script(statements: &[String]) -> String {
    let mut script = String::new();
    for statement in statements {
        let statement = statement.trim();
        script.push_str(statement);
        if !statement.starts_with('\\') && !statement.ends_with(';') {
            script.push(';');
        }
        script.push('\n');
    }
    script
}

fn quoted_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn quoted_name(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl PostgreSqlEngine {
    pub fn new(command_line: PostgreSqlCommandLine) -> Result<PostgreSqlEngine, EngineError> {
        let output = Command::new(&command_line.program)
            .arg("--version")
            .output()
            .map_err(|error| EngineError(format!("{}: {error}", command_line.program.display())))?;
        let client_version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let engine = PostgreSqlEngine {
            command_line,
            client_version,
        };
        engine.text("SELECT 1")?;
        Ok(engine)
    }

    fn psql(&self, arguments: &[&str], script: Option<&str>) -> Result<String, EngineError> {
        let mut command = Command::new(&self.command_line.program);
        command
            .args(["-X", "-q", "-v", "ON_ERROR_STOP=1"])
            .args(["-d", &self.command_line.connection])
            .args(arguments)
            .stdin(if script.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            EngineError(format!("{}: {error}", self.command_line.program.display()))
        })?;
        if let Some(script) = script {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin
                .write_all(script.as_bytes())
                .map_err(|error| EngineError(format!("writing to psql: {error}")))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| EngineError(format!("psql: {error}")))?;
        if !output.status.success() {
            return Err(EngineError(format!(
                "psql exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn query_arguments(&self, sql: &str, timed: bool) -> Vec<String> {
        let mut arguments = vec![
            "--csv".to_string(),
            "-c".to_string(),
            "\\pset null '\\\\N'".to_string(),
        ];
        for (name, value) in &self.command_line.settings {
            arguments.push("-c".into());
            arguments.push(format!(
                "SET {} = {}",
                quoted_name(name),
                quoted_text(value)
            ));
        }
        if timed {
            arguments.push("-c".into());
            arguments.push("\\timing on".into());
        }
        arguments.push("-c".into());
        arguments.push(sql.to_string());
        arguments
    }

    fn table(&self, sql: &str, timed: bool) -> Result<(ResultTable, Option<f64>), EngineError> {
        let arguments = self.query_arguments(sql, timed);
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let (text, seconds) = split_timing_lines(&self.psql(&arguments, None)?);
        if text.trim().is_empty() {
            return Err(EngineError(format!("psql printed no result for {sql}")));
        }
        Ok((ResultTable::from_csv_with_header(&text)?, seconds))
    }

    fn text(&self, sql: &str) -> Result<String, EngineError> {
        let (table, _) = self.table(sql, false)?;
        Ok(table
            .rows
            .first()
            .and_then(|row| row.first())
            .map(|value| value.key_text())
            .unwrap_or_default())
    }

    fn number(&self, sql: &str) -> Result<u64, EngineError> {
        let text = self.text(sql)?;
        text.parse()
            .map_err(|_| EngineError(format!("no number from {sql}: {text}")))
    }

    fn has_extension(&self, name: &str) -> Result<bool, EngineError> {
        Ok(self.number(&format!(
            "SELECT count(*) FROM pg_extension WHERE extname = {}",
            quoted_text(name)
        ))? > 0)
    }
}

impl OneShotQueryEngine for PostgreSqlEngine {
    fn description(&self) -> EngineDescription {
        let server = self
            .text(
                "SELECT current_setting('server_version') || coalesce(' with ' || \
                 (SELECT string_agg(extname || ' ' || extversion, ', ' ORDER BY extname) \
                  FROM pg_extension WHERE extname <> 'plpgsql'), '')",
            )
            .unwrap_or_else(|error| format!("unknown server version: {error}"));
        EngineDescription {
            id: "postgresql".into(),
            version: format!("PostgreSQL {server}"),
            version_dirty: false,
            build_profile: self.client_version.clone(),
        }
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::PostgreSql
    }

    fn configuration(&self) -> serde_json::Value {
        serde_json::json!({
            "connection": self.command_line.connection,
            "settings": self.command_line.settings.iter().cloned().collect::<BTreeMap<_, _>>(),
        })
    }

    fn table_rows(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        let tables = self.number(&format!(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_name = {}",
            quoted_text(table)
        ))?;
        if tables == 0 {
            return Ok(None);
        }
        self.number(&format!("SELECT count(*) FROM {}", quoted_name(table)))
            .map(Some)
    }

    fn load_table(&mut self, statements: &[String]) -> Result<(), EngineError> {
        self.psql(&["-f", "-"], Some(&psql_script(statements)))
            .map(|_| ())
    }

    fn data_size_bytes(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        let hypertable = self.has_extension("timescaledb")?
            && self.number(&format!(
                "SELECT count(*) FROM timescaledb_information.hypertables \
                 WHERE hypertable_schema = current_schema() AND hypertable_name = {}",
                quoted_text(table)
            ))? > 0;
        let size = if hypertable {
            format!("SELECT hypertable_size({})", quoted_text(table))
        } else {
            format!("SELECT pg_total_relation_size({})", quoted_text(table))
        };
        self.number(&size).map(Some)
    }

    fn restart(&mut self) -> Result<(), EngineError> {
        let command = self.command_line.restart_command.clone();
        let ready_timeout = self.command_line.ready_timeout;
        restart_and_wait("PostgreSQL", command.as_deref(), ready_timeout, || {
            self.text("SELECT 1").is_ok()
        })
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryRun, EngineError> {
        let (table, seconds) = self.table(sql, true)?;
        Ok(QueryRun {
            table,
            engine_reported_seconds: seconds,
            engine_reported_cpu_seconds: None,
            details: BTreeMap::new(),
        })
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct PostgreSqlOptions {
    #[arg(long, default_value = "psql")]
    pub postgresql_program: PathBuf,
    #[arg(long)]
    pub postgresql_connection: Option<String>,
    #[arg(long = "postgresql-setting", value_parser = super::parse_setting)]
    pub postgresql_settings: Vec<(String, String)>,
    #[arg(long)]
    pub postgresql_restart_command: Option<String>,
    #[arg(long, default_value_t = 120)]
    pub postgresql_ready_timeout_seconds: u64,
}

pub fn one_shot_queries(matches: &clap::ArgMatches) -> anyhow::Result<Box<dyn OneShotQueryEngine>> {
    let options = <PostgreSqlOptions as clap::FromArgMatches>::from_arg_matches(matches)?;
    let Some(connection) = options.postgresql_connection else {
        anyhow::bail!("engine postgresql needs --postgresql-connection");
    };
    Ok(Box::new(PostgreSqlEngine::new(PostgreSqlCommandLine {
        program: options.postgresql_program,
        connection,
        settings: options.postgresql_settings,
        restart_command: options.postgresql_restart_command,
        ready_timeout: Duration::from_secs(options.postgresql_ready_timeout_seconds),
    })?))
}

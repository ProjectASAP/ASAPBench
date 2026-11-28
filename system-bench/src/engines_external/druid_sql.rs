use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aqpbm_core::harness::engine::{EngineDescription, EngineError, SqlDialect};
use aqpbm_core::harness::one_shot::{OneShotQueryEngine, QueryRun, ResultTable, ResultValue};
use serde_json::{json, Map, Value};

use super::restart_command::restart_and_wait;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub struct DruidServer {
    pub url: String,
    pub context: Vec<(String, Value)>,
    pub statement_timeout: Duration,
    pub load_timeout: Duration,
    pub restart_command: Option<String>,
    pub ready_timeout: Duration,
    pub idle_timeout: Duration,
}

pub struct DruidEngine {
    server: DruidServer,
    agent: ureq::Agent,
    url: String,
    version: String,
}

pub fn context_value(text: &str) -> Value {
    match text {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => text
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| text.parse::<f64>().map(Value::from))
            .unwrap_or_else(|_| Value::String(text.to_string())),
    }
}

pub fn result_table(answer: &Value) -> Result<ResultTable, EngineError> {
    let rows = answer
        .as_array()
        .ok_or_else(|| EngineError(format!("Druid's answer is not an array: {answer}")))?;
    let (Some(names), Some(types)) = (rows.first(), rows.get(1)) else {
        return Err(EngineError(format!(
            "Druid's answer has no header rows: {answer}"
        )));
    };
    let text_list = |row: &Value| -> Vec<String> {
        row.as_array()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    let columns = text_list(names);
    let types = text_list(types);
    let mut table = ResultTable {
        columns,
        rows: Vec::with_capacity(rows.len() - 2),
    };
    for row in &rows[2..] {
        let cells = row
            .as_array()
            .ok_or_else(|| EngineError(format!("Druid's row is not an array: {row}")))?;
        table.rows.push(
            cells
                .iter()
                .zip(&types)
                .map(|(cell, kind)| match (kind.as_str(), cell.as_str()) {
                    ("TIMESTAMP", Some(text)) => ResultValue::Text(timestamp_text(text)),
                    _ => ResultValue::from_json(cell),
                })
                .collect(),
        );
    }
    Ok(table)
}

fn timestamp_text(text: &str) -> String {
    let text = text.strip_suffix('Z').unwrap_or(text);
    let text = text.strip_suffix(".000").unwrap_or(text);
    text.replacen('T', " ", 1)
}

pub fn is_ingestion(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    while rest
        .get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("SET "))
    {
        rest = rest
            .split_once(';')
            .map_or("", |(_, after)| after)
            .trim_start();
    }
    ["INSERT", "REPLACE"].iter().any(|keyword| {
        rest.get(..keyword.len())
            .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
    })
}

fn quoted_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn quoted_name(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl DruidEngine {
    pub fn new(server: DruidServer) -> Result<DruidEngine, EngineError> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(server.statement_timeout + Duration::from_secs(5))
            .build();
        let url = server.url.trim_end_matches('/').to_string();
        let mut engine = DruidEngine {
            server,
            agent,
            url,
            version: String::new(),
        };
        let status = engine.get("/status")?;
        engine.version = status
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        Ok(engine)
    }

    fn answer(&self, response: Result<ureq::Response, ureq::Error>) -> Result<Value, EngineError> {
        match response {
            Ok(response) => {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut response.into_reader(), &mut text)
                    .map_err(|error| EngineError(format!("reading Druid's answer: {error}")))?;
                serde_json::from_str(&text)
                    .map_err(|error| EngineError(format!("Druid's answer {text}: {error}")))
            }
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                Err(EngineError(format!(
                    "Druid answered {code}: {}",
                    text.trim_end()
                )))
            }
            Err(error) => Err(EngineError(format!("Druid at {}: {error}", self.url))),
        }
    }

    fn get(&self, path: &str) -> Result<Value, EngineError> {
        self.answer(self.agent.get(&format!("{}{path}", self.url)).call())
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, EngineError> {
        self.answer(
            self.agent
                .post(&format!("{}{path}", self.url))
                .set("Content-Type", "application/json")
                .send_string(&body.to_string()),
        )
    }

    fn query_context(&self) -> Value {
        let mut context = Map::new();
        context.insert(
            "timeout".into(),
            Value::from(self.server.statement_timeout.as_millis() as u64),
        );
        for name in [
            "useCache",
            "populateCache",
            "useResultLevelCache",
            "populateResultLevelCache",
        ] {
            context.insert(name.into(), Value::Bool(false));
        }
        self.with_server_context(context)
    }

    fn with_server_context(&self, mut context: Map<String, Value>) -> Value {
        for (name, value) in &self.server.context {
            context.insert(name.clone(), value.clone());
        }
        Value::Object(context)
    }

    fn sql(&self, sql: &str) -> Result<ResultTable, EngineError> {
        let answer = self.post(
            "/druid/v2/sql/",
            &json!({
                "query": sql.trim_end().trim_end_matches(';'),
                "resultFormat": "array",
                "header": true,
                "sqlTypesHeader": true,
                "context": self.query_context(),
            }),
        )?;
        result_table(&answer)
    }

    fn number(&self, sql: &str) -> Result<u64, EngineError> {
        let table = self.sql(sql)?;
        match table.rows.first().and_then(|row| row.first()) {
            None | Some(ResultValue::Null) => Ok(0),
            Some(value) => value
                .as_number()
                .map(|number| number as u64)
                .ok_or_else(|| EngineError(format!("no number from {sql}: {value:?}"))),
        }
    }

    fn ingest(&self, sql: &str) -> Result<(), EngineError> {
        let task = self.post(
            "/druid/v2/sql/task/",
            &json!({
                "query": sql.trim_end().trim_end_matches(';'),
                "context": self.with_server_context(Map::from_iter([(
                    "waitUntilSegmentsLoad".to_string(),
                    Value::Bool(true),
                )])),
            }),
        )?;
        let Some(task_id) = task.get("taskId").and_then(Value::as_str) else {
            return Err(EngineError(format!("Druid started no task: {task}")));
        };
        let start = Instant::now();
        loop {
            let status = self.get(&format!("/druid/indexer/v1/task/{task_id}/status"))?;
            let state = status
                .pointer("/status/statusCode")
                .or_else(|| status.pointer("/status/status"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match state {
                "SUCCESS" => return Ok(()),
                "FAILED" => {
                    let reason = status
                        .pointer("/status/errorMsg")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason given");
                    return Err(EngineError(format!(
                        "Druid task {task_id} failed: {reason}"
                    )));
                }
                _ => {}
            }
            if start.elapsed() > self.server.load_timeout {
                return Err(EngineError(format!(
                    "Druid task {task_id} still {state} after {:?}",
                    self.server.load_timeout
                )));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn unavailable_segments(&self) -> Result<u64, EngineError> {
        self.number(
            "SELECT COUNT(*) FROM sys.segments \
             WHERE is_published = 1 AND is_overshadowed = 0 AND is_available = 0",
        )
    }
}

impl OneShotQueryEngine for DruidEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "druid".into(),
            version: self.version.clone(),
            version_dirty: false,
            build_profile: "SQL over HTTP".into(),
        }
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Druid
    }

    fn configuration(&self) -> Value {
        json!({
            "url": self.server.url,
            "context": self.query_context(),
        })
    }

    fn table_rows(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        let tables = self.number(&format!(
            "SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_SCHEMA = 'druid' AND TABLE_NAME = {}",
            quoted_text(table)
        ))?;
        if tables == 0 {
            return Ok(None);
        }
        self.number(&format!("SELECT COUNT(*) FROM {}", quoted_name(table)))
            .map(Some)
    }

    fn load_table(&mut self, statements: &[String]) -> Result<(), EngineError> {
        for statement in statements {
            if is_ingestion(statement) {
                self.ingest(statement)?;
            } else {
                self.sql(statement)?;
            }
        }
        self.wait_until_idle()
    }

    fn data_size_bytes(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        self.number(&format!(
            "SELECT SUM(\"size\") FROM sys.segments \
             WHERE datasource = {} AND is_published = 1 AND is_overshadowed = 0",
            quoted_text(table)
        ))
        .map(Some)
    }

    fn restart(&mut self) -> Result<(), EngineError> {
        let command = self.server.restart_command.clone();
        let ready_timeout = self.server.ready_timeout;
        restart_and_wait("Druid", command.as_deref(), ready_timeout, || {
            matches!(self.unavailable_segments(), Ok(0))
        })
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryRun, EngineError> {
        Ok(QueryRun {
            table: self.sql(sql)?,
            engine_reported_seconds: None,
            engine_reported_cpu_seconds: None,
            details: BTreeMap::new(),
        })
    }

    fn wait_until_idle(&mut self) -> Result<(), EngineError> {
        let start = Instant::now();
        loop {
            if self.unavailable_segments()? == 0 {
                return Ok(());
            }
            if start.elapsed() > self.server.idle_timeout {
                return Err(EngineError(format!(
                    "Druid segments still loading after {:?}",
                    self.server.idle_timeout
                )));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct DruidOptions {
    #[arg(long)]
    pub druid_url: Option<String>,
    #[arg(long = "druid-context", value_parser = super::parse_setting)]
    pub druid_context: Vec<(String, String)>,
    #[arg(long, default_value_t = 600)]
    pub druid_statement_timeout_seconds: u64,
    #[arg(long, default_value_t = 21600)]
    pub druid_load_timeout_seconds: u64,
    #[arg(long)]
    pub druid_restart_command: Option<String>,
    #[arg(long, default_value_t = 600)]
    pub druid_ready_timeout_seconds: u64,
    #[arg(long, default_value_t = 3600)]
    pub druid_idle_timeout_seconds: u64,
}

pub fn one_shot_queries(matches: &clap::ArgMatches) -> anyhow::Result<Box<dyn OneShotQueryEngine>> {
    let options = <DruidOptions as clap::FromArgMatches>::from_arg_matches(matches)?;
    let Some(url) = options.druid_url else {
        anyhow::bail!("engine druid needs --druid-url");
    };
    Ok(Box::new(DruidEngine::new(DruidServer {
        url,
        context: options
            .druid_context
            .iter()
            .map(|(name, value)| (name.clone(), context_value(value)))
            .collect(),
        statement_timeout: Duration::from_secs(options.druid_statement_timeout_seconds),
        load_timeout: Duration::from_secs(options.druid_load_timeout_seconds),
        restart_command: options.druid_restart_command,
        ready_timeout: Duration::from_secs(options.druid_ready_timeout_seconds),
        idle_timeout: Duration::from_secs(options.druid_idle_timeout_seconds),
    })?))
}

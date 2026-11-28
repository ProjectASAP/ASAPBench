use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aqpbm_core::harness::engine::{EngineDescription, EngineError, SqlDialect};
use aqpbm_core::harness::one_shot::{OneShotQueryEngine, QueryRun, ResultTable};

use super::clickhouse_connection::ClickHouseConnectionOptions;
use super::clickhouse_connection::{ClickHouseConnection, Http};
use super::restart_command::restart_and_wait;

pub const DIALECT: &str = "ClickHouse";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickHouseQueryTextServer {
    pub connection: ClickHouseConnection,
    pub database: String,
    pub max_threads: Option<u32>,
    pub restart_command: Option<String>,
    pub ready_timeout: Duration,
    pub idle_timeout: Duration,
    pub settings: Vec<(String, String)>,
}

pub struct ClickHouseQueryTextEngine {
    server: ClickHouseQueryTextServer,
    http: Http,
    version: String,
    last_query_id: Option<String>,
}

static QUERY_COUNT: AtomicU64 = AtomicU64::new(0);

fn quoted_text(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

impl ClickHouseQueryTextEngine {
    pub fn new(
        server: ClickHouseQueryTextServer,
    ) -> Result<ClickHouseQueryTextEngine, EngineError> {
        let http = Http::new(&server.connection);
        let mut engine = ClickHouseQueryTextEngine {
            server,
            http,
            version: String::new(),
            last_query_id: None,
        };
        engine.version = engine.text("SELECT version()")?.trim().to_string();
        Ok(engine)
    }

    fn settings(&self) -> Vec<(&'static str, String)> {
        let mut settings = vec![
            ("database", self.server.database.clone()),
            ("use_query_cache", "0".to_string()),
        ];
        if let Some(threads) = self.server.max_threads {
            settings.push(("max_threads", threads.to_string()));
        }
        settings
    }

    fn statement_settings(&self) -> Vec<(&str, &str)> {
        self.server
            .settings
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }

    fn statement(
        &self,
        sql: &str,
        extra: &[(&str, &str)],
    ) -> Result<(String, serde_json::Value), EngineError> {
        let settings = self.settings();
        let mut params: Vec<(&str, &str)> = settings
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        params.extend(self.statement_settings());
        params.extend_from_slice(extra);
        self.http.run_with_summary(sql, &params, None)
    }

    fn text(&self, sql: &str) -> Result<String, EngineError> {
        self.statement(sql, &[("default_format", "TabSeparated")])
            .map(|(text, _)| text)
    }

    fn number(&self, sql: &str) -> Result<Option<u64>, EngineError> {
        let text = self.text(sql)?;
        let text = text.trim();
        if text.is_empty() || text == "\\N" {
            return Ok(None);
        }
        text.parse()
            .map(Some)
            .map_err(|_| EngineError(format!("no number from {sql}: {text}")))
    }

    fn wait_for_merges(&self) -> Result<(), EngineError> {
        let start = Instant::now();
        let mut idle_checks = 0;
        while idle_checks < 3 {
            let merges = self
                .number("SELECT count() FROM system.merges WHERE database = currentDatabase()")?
                .unwrap_or(0);
            idle_checks = if merges == 0 { idle_checks + 1 } else { 0 };
            if start.elapsed() > self.server.idle_timeout {
                return Err(EngineError(format!(
                    "background merges still running after {:?}",
                    self.server.idle_timeout
                )));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(())
    }

    fn query_cpu_seconds(&self, query_id: &str) -> Option<f64> {
        self.text("SYSTEM FLUSH LOGS").ok()?;
        let microseconds = self
            .number(&format!(
                "SELECT ProfileEvents['UserTimeMicroseconds'] + ProfileEvents['SystemTimeMicroseconds'] \
                 FROM system.query_log WHERE query_id = {} AND type = 'QueryFinish' LIMIT 1",
                quoted_text(query_id)
            ))
            .ok()??;
        Some(microseconds as f64 / 1e6)
    }
}

impl OneShotQueryEngine for ClickHouseQueryTextEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "clickhouse".into(),
            version: self.version.clone(),
            version_dirty: false,
            build_profile: match self.server.max_threads {
                Some(threads) => format!("max_threads={threads}"),
                None => "server default max_threads".into(),
            },
        }
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::new(DIALECT)
    }

    fn configuration(&self) -> serde_json::Value {
        serde_json::json!({
            "database": self.server.database,
            "max_threads": self.server.max_threads,
            "settings": self.server.settings.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
        })
    }

    fn table_rows(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        let tables = self.number(&format!(
            "SELECT count() FROM system.tables WHERE database = currentDatabase() AND name = {}",
            quoted_text(table)
        ))?;
        if tables.unwrap_or(0) == 0 {
            return Ok(None);
        }
        self.number(&format!(
            "SELECT count() FROM `{}`",
            table.replace('`', "\\`")
        ))
    }

    fn load_table(&mut self, statements: &[String]) -> Result<(), EngineError> {
        for statement in statements {
            self.statement(statement, &[])?;
        }
        Ok(())
    }

    fn data_size_bytes(&mut self, table: &str) -> Result<Option<u64>, EngineError> {
        self.number(&format!(
            "SELECT sum(bytes_on_disk) FROM system.parts \
             WHERE active AND database = currentDatabase() AND table = {}",
            quoted_text(table)
        ))
    }

    fn restart(&mut self) -> Result<(), EngineError> {
        let command = self.server.restart_command.clone();
        let ready_timeout = self.server.ready_timeout;
        restart_and_wait("ClickHouse", command.as_deref(), ready_timeout, || {
            self.text("SELECT 1").is_ok()
        })
    }

    fn run_query(&mut self, sql: &str) -> Result<QueryRun, EngineError> {
        let query_id = format!(
            "approxbench-{}-{}",
            std::process::id(),
            QUERY_COUNT.fetch_add(1, Ordering::Relaxed)
        );
        let (text, summary) = self.statement(
            sql.trim_end().trim_end_matches(';'),
            &[("default_format", "CSVWithNames"), ("query_id", &query_id)],
        )?;
        let elapsed_seconds = summary
            .get("elapsed_ns")
            .and_then(|elapsed| elapsed.as_str()?.parse::<f64>().ok())
            .map(|nanoseconds| nanoseconds / 1e9);
        let mut details = BTreeMap::new();
        if let Some(read_bytes) = summary
            .get("read_bytes")
            .and_then(|bytes| bytes.as_str()?.parse::<u64>().ok())
        {
            details.insert("read_bytes".to_string(), read_bytes.into());
        }
        self.last_query_id = Some(query_id);
        Ok(QueryRun {
            table: ResultTable::from_csv_with_header(&text)?,
            engine_reported_seconds: elapsed_seconds,
            engine_reported_cpu_seconds: None,
            details,
        })
    }

    fn wait_until_idle(&mut self) -> Result<(), EngineError> {
        self.wait_for_merges()
    }

    fn last_query_cpu_seconds(&mut self) -> Option<f64> {
        let query_id = self.last_query_id.take()?;
        self.query_cpu_seconds(&query_id)
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct ClickHouseQueryTextOptions {
    #[command(flatten)]
    pub connection: ClickHouseConnectionOptions,
    #[arg(long, default_value = "default")]
    pub clickhouse_database: String,
    #[arg(long)]
    pub clickhouse_max_threads: Option<u32>,
    #[arg(long)]
    pub clickhouse_restart_command: Option<String>,
    #[arg(long, default_value_t = 120)]
    pub clickhouse_ready_timeout_seconds: u64,
    #[arg(long, default_value_t = 3600)]
    pub clickhouse_idle_timeout_seconds: u64,
    #[arg(long = "clickhouse-setting", value_parser = super::parse_setting)]
    pub clickhouse_settings: Vec<(String, String)>,
}

pub fn one_shot_queries(matches: &clap::ArgMatches) -> anyhow::Result<Box<dyn OneShotQueryEngine>> {
    let options = <ClickHouseQueryTextOptions as clap::FromArgMatches>::from_arg_matches(matches)?;
    let Some(connection) = options.connection.connection() else {
        anyhow::bail!("engine clickhouse needs --clickhouse-url");
    };
    Ok(Box::new(ClickHouseQueryTextEngine::new(
        ClickHouseQueryTextServer {
            connection,
            database: options.clickhouse_database,
            max_threads: options.clickhouse_max_threads,
            restart_command: options.clickhouse_restart_command,
            ready_timeout: Duration::from_secs(options.clickhouse_ready_timeout_seconds),
            idle_timeout: Duration::from_secs(options.clickhouse_idle_timeout_seconds),
            settings: options.clickhouse_settings,
        },
    )?))
}

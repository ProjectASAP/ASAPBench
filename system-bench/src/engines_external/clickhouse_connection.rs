use std::time::Duration;

use aqpbm_core::harness::engine::EngineError;

/// How to reach a ClickHouse server, for every kind of measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickHouseConnection {
    /// The HTTP interface, e.g. `http://10.0.0.1:8123`.
    pub url: String,
    /// How long one statement may take before it fails, e.g. 600 s.
    pub statement_timeout: Duration,
}

/// The server's HTTP interface. A statement is the `query` parameter; an
/// insert's rows are the body.
#[derive(Clone)]
pub(crate) struct Http {
    agent: ureq::Agent,
    url: String,
    /// `max_execution_time` of every statement, in whole seconds.
    max_execution_seconds: String,
}

/// How long to wait to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

impl Http {
    pub(crate) fn new(connection: &ClickHouseConnection) -> Http {
        let statement_timeout = connection.statement_timeout;
        Http {
            // The client waits a little longer than the server lets a
            // statement run, so the server's own timeout error arrives.
            agent: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout(statement_timeout + Duration::from_secs(5))
                .build(),
            url: connection.url.trim_end_matches('/').to_string(),
            max_execution_seconds: statement_timeout.as_secs().max(1).to_string(),
        }
    }

    /// Runs `sql` with the URL parameters `params`; its output and the
    /// `read_bytes` of its summary header. The statement is the request
    /// body, except for an insert, whose body is its rows.
    pub(crate) fn run(
        &self,
        sql: &str,
        params: &[(&str, &str)],
        insert_rows: Option<&str>,
    ) -> Result<(String, u64), EngineError> {
        let (text, summary) = self.run_with_summary(sql, params, insert_rows)?;
        let read_bytes = summary
            .get("read_bytes")
            .and_then(|bytes| bytes.as_str()?.parse().ok())
            .unwrap_or(0);
        Ok((text, read_bytes))
    }

    pub(crate) fn run_with_summary(
        &self,
        sql: &str,
        params: &[(&str, &str)],
        insert_rows: Option<&str>,
    ) -> Result<(String, serde_json::Value), EngineError> {
        // `wait_end_of_query`: the status and the summary header come after
        // the statement ends, so an error raised mid-answer is a status
        // error and `read_bytes` is complete.
        let mut request = self
            .agent
            .post(&format!("{}/", self.url))
            .query("wait_end_of_query", "1")
            .query("max_execution_time", &self.max_execution_seconds);
        for (name, value) in params {
            request = request.query(name, value);
        }
        let sent = match insert_rows {
            Some(rows) => request.query("query", sql).send_string(rows),
            None => request.send_string(sql),
        };
        match sent {
            Ok(response) => {
                if let Some(code) = response.header("X-ClickHouse-Exception-Code") {
                    let code = code.to_string();
                    let text = response.into_string().unwrap_or_default();
                    return Err(EngineError(format!(
                        "ClickHouse exception {code}: {}",
                        text.trim_end()
                    )));
                }
                let summary = response
                    .header("X-ClickHouse-Summary")
                    .and_then(|summary| serde_json::from_str::<serde_json::Value>(summary).ok())
                    .unwrap_or(serde_json::Value::Null);
                // `into_string` stops at 10 MB; an answer may be larger.
                let mut text = String::new();
                std::io::Read::read_to_string(&mut response.into_reader(), &mut text)
                    .map_err(|error| EngineError(format!("reading the answer: {error}")))?;
                Ok((text, summary))
            }
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                Err(EngineError(format!(
                    "ClickHouse answered {code}: {}",
                    text.trim_end()
                )))
            }
            Err(error) => Err(EngineError(format!("ClickHouse at {}: {error}", self.url))),
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct ClickHouseConnectionOptions {
    /// The ClickHouse server's HTTP interface, e.g. `http://10.0.0.1:8123`
    /// (engine `clickhouse`).
    #[arg(long)]
    pub clickhouse_url: Option<String>,
    /// How long one ClickHouse statement may run, in seconds.
    #[arg(long, default_value_t = 600)]
    pub clickhouse_statement_timeout_seconds: u64,
}

impl ClickHouseConnectionOptions {
    pub fn connection(&self) -> Option<ClickHouseConnection> {
        Some(ClickHouseConnection {
            url: self.clickhouse_url.clone()?,
            statement_timeout: Duration::from_secs(self.clickhouse_statement_timeout_seconds),
        })
    }
}

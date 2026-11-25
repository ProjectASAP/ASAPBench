use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::engines_external::asapquery_backend_server::AsapqueryBackendConnection;
use crate::engines_external::victoriametrics_server::VictoriaMetricsConnection;

pub const SERVERS: [&str; 2] = ["asapquery-backend", "victoriametrics"];

const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

pub enum Server {
    AsapqueryBackend(AsapqueryBackendConnection),
    VictoriaMetrics(VictoriaMetricsConnection),
}

impl Server {
    pub fn connect(id: &str, url: &str) -> Option<Server> {
        match id {
            "asapquery-backend" => Some(Server::AsapqueryBackend(AsapqueryBackendConnection::new(
                url,
                REQUEST_TIMEOUT,
            ))),
            "victoriametrics" => Some(Server::VictoriaMetrics(VictoriaMetricsConnection::new(
                url,
                REQUEST_TIMEOUT,
            ))),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Server::AsapqueryBackend(_) => "ASAPQuery-backend",
            Server::VictoriaMetrics(_) => "VictoriaMetrics",
        }
    }

    pub fn agent(&self) -> &ureq::Agent {
        match self {
            Server::AsapqueryBackend(connection) => &connection.agent,
            Server::VictoriaMetrics(connection) => &connection.agent,
        }
    }

    pub fn base(&self) -> &str {
        match self {
            Server::AsapqueryBackend(connection) => &connection.base,
            Server::VictoriaMetrics(connection) => &connection.url,
        }
    }

    pub fn make_visible(&self, through_ms: i64) -> Result<()> {
        match self {
            Server::AsapqueryBackend(connection) => connection.make_visible(through_ms),
            Server::VictoriaMetrics(connection) => connection
                .force_flush()
                .map(|_| ())
                .map_err(|error| anyhow::anyhow!("{}", error.0))
                .context("flushing VictoriaMetrics"),
        }
    }

    pub fn status(&self) -> Result<Option<Value>> {
        match self {
            Server::AsapqueryBackend(connection) => connection.status().map(Some),
            Server::VictoriaMetrics(_) => Ok(None),
        }
    }
}

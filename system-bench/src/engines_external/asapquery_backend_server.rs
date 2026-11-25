use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aqpbm_core::harness::cgroup::CgroupSource;
use aqpbm_core::harness::data::SampleChunk;
use aqpbm_core::harness::engine::{
    Answers, ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, Footprint,
    InstalledArm, PlanSummary, QueryLanguage, StepReport, WorkloadSpec,
};

use crate::prometheus_api::query_text_arm::{
    ask_every_query, queries_to_ask, write_chunk, QueryToAsk,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsapqueryBackendServer {
    pub url: String,
    pub cgroup: CgroupSource,
    pub request_timeout: Duration,
}

pub struct AsapqueryBackendEngine {
    server: AsapqueryBackendServer,
    connection: AsapqueryBackendConnection,
    installed_once: AtomicBool,
}

#[derive(Clone)]
pub struct AsapqueryBackendConnection {
    pub agent: ureq::Agent,
    pub base: String,
}

impl AsapqueryBackendConnection {
    pub fn new(url: &str, request_timeout: Duration) -> AsapqueryBackendConnection {
        AsapqueryBackendConnection {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(request_timeout)
                .build(),
            base: url.trim_end_matches('/').to_string(),
        }
    }

    pub fn make_visible(&self, through_ms: i64) -> anyhow::Result<()> {
        use anyhow::Context;
        self.agent
            .post(&format!(
                "{}/api/v1/precompute/watermark?time_ms={through_ms}",
                self.base
            ))
            .call()
            .with_context(|| format!("closing panes through {through_ms}"))?;
        Ok(())
    }

    pub fn status(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::from_str(
            &self
                .agent
                .get(&format!("{}/api/v1/status/asap", self.base))
                .call()?
                .into_string()?,
        )?)
    }

    pub fn held_bytes(&self) -> anyhow::Result<Footprint> {
        let text = self
            .agent
            .get(&format!("{}/metrics", self.base))
            .call()?
            .into_string()?;
        let gauge = |name: &str| -> anyhow::Result<u64> {
            text.lines()
                .find_map(|line| {
                    let (metric, value) = line.split_once(' ')?;
                    (metric == name).then(|| value.trim().parse::<f64>().ok())?
                })
                .map(|value| value as u64)
                .ok_or_else(|| anyhow::anyhow!("the server's /metrics has no {name}"))
        };
        Ok(Footprint {
            view_bytes: gauge("asap_view_bytes")?,
            raw_retained_bytes: gauge("asap_raw_data_bytes")?,
            peak_bytes: None,
        })
    }
}

impl AsapqueryBackendEngine {
    pub fn new(server: AsapqueryBackendServer) -> Result<AsapqueryBackendEngine, EngineError> {
        let connection = AsapqueryBackendConnection::new(&server.url, server.request_timeout);
        connection.status().map_err(|error| {
            EngineError(format!("ASAPQuery-backend at {}: {error:#}", server.url))
        })?;
        Ok(AsapqueryBackendEngine {
            server,
            connection,
            installed_once: AtomicBool::new(false),
        })
    }
}

impl EngineUnderTest for AsapqueryBackendEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "asapquery-backend".into(),
            version: "unreported".into(),
            version_dirty: false,
            build_profile: "server".into(),
        }
    }

    fn cpu_accounting(&self) -> CpuAccounting {
        CpuAccounting::Cgroup(self.server.cgroup.clone())
    }

    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        if arm.engine_options.get("plan_file").is_some() {
            return Err(EngineError(
                "ASAPQuery-backend runs the plan it was started with; this adapter installs no \
                 plan file, so give it a variant with plans: false"
                    .into(),
            ));
        }
        let queries = queries_to_ask(workload, &[QueryLanguage::PromQl])?;
        if self.installed_once.swap(true, Ordering::SeqCst) {
            return Err(EngineError(
                "ASAPQuery-backend keeps every sample written to it: restart the server before \
                 the next arm, and run this engine with --warmup 0 --runs 1"
                    .into(),
            ));
        }
        Ok(Box::new(AsapqueryBackendArm {
            connection: self.connection.clone(),
            queries,
        }))
    }
}

struct AsapqueryBackendArm {
    connection: AsapqueryBackendConnection,
    queries: Vec<QueryToAsk>,
}

impl InstalledArm for AsapqueryBackendArm {
    fn plan_summary(&self) -> PlanSummary {
        PlanSummary::default()
    }

    fn ingest(&mut self, chunk: &SampleChunk) -> Result<StepReport, EngineError> {
        write_chunk(&self.connection.agent, &self.connection.base, chunk)?;
        self.connection
            .make_visible(chunk.end_ms)
            .map_err(|error| EngineError(format!("{error:#}")))?;
        Ok(StepReport::default())
    }

    fn evaluate(&mut self, at_ms: i64) -> Result<(Answers, StepReport), EngineError> {
        let answers = ask_every_query(
            &self.connection.agent,
            &self.connection.base,
            &self.queries,
            at_ms,
            &[],
        )?;
        Ok((answers, StepReport::default()))
    }

    fn footprint(&self) -> Result<Footprint, EngineError> {
        self.connection
            .held_bytes()
            .map_err(|error| EngineError(format!("{error:#}")))
    }
}

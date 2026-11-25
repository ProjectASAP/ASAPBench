use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aqpbm_core::harness::cgroup::CgroupSource;
use aqpbm_core::harness::data::SampleChunk;
use aqpbm_core::harness::engine::{
    Answers, ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, Footprint,
    InstalledArm, PlanSummary, QueryLanguage, StepReport, WorkloadSpec,
};

use crate::prometheus_api::query::ask;
use crate::prometheus_api::remote_write::{encode, request_of, send_until_accepted};

pub const BATCH_SAMPLES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsapqueryBackendServer {
    pub url: String,
    pub cgroup: Option<CgroupSource>,
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
        match &self.server.cgroup {
            Some(cgroup) => CpuAccounting::Cgroup(cgroup.clone()),
            None => CpuAccounting::CallingThread,
        }
    }

    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        if arm.engine_options.get("plan_file").is_some() {
            return Err(EngineError(
                "ASAPQuery-backend plans its queries itself and takes no plan file".into(),
            ));
        }
        let queries = workload
            .queries
            .iter()
            .map(|query| {
                query
                    .texts
                    .iter()
                    .find(|text| text.language == QueryLanguage::PromQl)
                    .map(|text| (query.id.clone(), text.text.clone()))
                    .ok_or_else(|| EngineError(format!("query {} has no PromQL text", query.id)))
            })
            .collect::<Result<Vec<_>, _>>()?;
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
    queries: Vec<(String, String)>,
}

impl InstalledArm for AsapqueryBackendArm {
    fn plan_summary(&self) -> PlanSummary {
        PlanSummary::default()
    }

    fn ingest(&mut self, chunk: &SampleChunk) -> Result<StepReport, EngineError> {
        for batch in chunk.samples.chunks(BATCH_SAMPLES) {
            send_until_accepted(
                &self.connection.agent,
                &self.connection.base,
                &encode(&request_of(batch)),
            )
            .map_err(|error| EngineError(format!("{error:#}")))?;
        }
        self.connection
            .make_visible(chunk.end_ms)
            .map_err(|error| EngineError(format!("{error:#}")))?;
        Ok(StepReport::default())
    }

    fn evaluate(&mut self, at_ms: i64) -> Result<(Answers, StepReport), EngineError> {
        let mut answers = BTreeMap::new();
        for (id, promql) in &self.queries {
            let asked = ask(&self.connection.agent, &self.connection.base, promql, at_ms);
            let samples = asked
                .outcome
                .map_err(|error| EngineError(format!("query {id}: {error}")))?;
            answers.insert(
                id.clone(),
                samples
                    .into_iter()
                    .filter(|(_, value)| value.is_finite())
                    .collect(),
            );
        }
        Ok((Answers(answers), StepReport::default()))
    }

    fn footprint(&self) -> Result<Footprint, EngineError> {
        Ok(Footprint::default())
    }
}

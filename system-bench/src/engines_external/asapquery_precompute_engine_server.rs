use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use aqpbm_core::harness::data::Sample;
use aqpbm_core::harness::engine::{
    Answers, ArmSpec, CpuAccounting, EngineDescription, EngineError, EngineUnderTest, Footprint,
    InstalledArm, PlanSummary, SampleChunk, SampleWriter, StepReport, WorkloadSpec,
};

use crate::prometheus_api::query_text_arm::write_chunk;
use crate::prometheus_api::remote_write::{send_to, write_url, RemoteWriteEncoder, Sent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsapqueryPrecomputeEngineServer {
    pub url: String,
    pub pids: Vec<u32>,
    pub streaming_config: PathBuf,
    pub version: String,
    pub request_timeout: Duration,
}

pub struct AsapqueryPrecomputeEngine {
    server: AsapqueryPrecomputeEngineServer,
    base: String,
    streaming_config: StreamingConfig,
}

#[derive(Debug, Deserialize)]
struct StreamingConfig {
    aggregations: Vec<Aggregation>,
    #[serde(default)]
    metrics: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Aggregation {
    metric: String,
}

impl StreamingConfig {
    fn read(path: &std::path::Path) -> Result<StreamingConfig, EngineError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            EngineError(format!("streaming config {}: {error}", path.display()))
        })?;
        serde_norway::from_str(&text)
            .map_err(|error| EngineError(format!("streaming config {}: {error}", path.display())))
    }

    fn check(&self, workload: &WorkloadSpec) -> Result<(), EngineError> {
        for record in &workload.data {
            if !self
                .aggregations
                .iter()
                .any(|aggregation| aggregation.metric == record.metric)
            {
                return Err(EngineError(format!(
                    "metric {} of workload {} has no aggregation in the engine's streaming \
                     config, so the engine would acknowledge its samples and drop them",
                    record.metric, workload.name
                )));
            }
            if let Some(expected) = self.metrics.get(&record.metric) {
                let expected: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
                let given: BTreeSet<&str> = record
                    .description
                    .labels
                    .iter()
                    .map(|label| label.name.as_str())
                    .collect();
                if expected != given {
                    return Err(EngineError(format!(
                        "metric {} has labels {given:?} in workload {}, but {expected:?} in the \
                         engine's streaming config",
                        record.metric, workload.name
                    )));
                }
            }
        }
        Ok(())
    }
}

const CONNECTION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

fn agent(request_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(request_timeout)
        .build()
}

impl AsapqueryPrecomputeEngine {
    pub fn new(
        server: AsapqueryPrecomputeEngineServer,
    ) -> Result<AsapqueryPrecomputeEngine, EngineError> {
        if server.pids.is_empty() {
            return Err(EngineError(
                "ASAPQuery's precompute engine is measured by process: give its pid".into(),
            ));
        }
        let streaming_config = StreamingConfig::read(&server.streaming_config)?;
        let base = server.url.trim_end_matches('/').to_string();
        match agent(CONNECTION_CHECK_TIMEOUT).get(&base).call() {
            Ok(_) | Err(ureq::Error::Status(..)) => {}
            Err(error) => {
                return Err(EngineError(format!(
                    "ASAPQuery's precompute engine at {base}: {error}"
                )))
            }
        }
        Ok(AsapqueryPrecomputeEngine {
            server,
            base,
            streaming_config,
        })
    }
}

impl EngineUnderTest for AsapqueryPrecomputeEngine {
    fn description(&self) -> EngineDescription {
        EngineDescription {
            id: "asapquery-precompute-engine".into(),
            version: self.server.version.clone(),
            version_dirty: false,
            build_profile: "server".into(),
        }
    }

    fn cpu_accounting(&self) -> CpuAccounting {
        CpuAccounting::Processes(self.server.pids.clone())
    }

    fn install(
        &self,
        workload: &WorkloadSpec,
        arm: &ArmSpec,
    ) -> Result<Box<dyn InstalledArm>, EngineError> {
        if arm.engine_options.get("plan_file").is_some() {
            return Err(EngineError(
                "ASAPQuery's precompute engine runs the streaming config it was started with; \
                 this adapter installs no plan file"
                    .into(),
            ));
        }
        self.streaming_config.check(workload)?;
        if !workload.queries.is_empty() {
            return Err(EngineError(format!(
                "ASAPQuery's precompute engine answers no queries, and workload {} has {}",
                workload.name,
                workload.queries.len()
            )));
        }
        Ok(Box::new(AsapqueryPrecomputeEngineArm {
            agent: agent(self.server.request_timeout),
            base: self.base.clone(),
            request_timeout: self.server.request_timeout,
        }))
    }
}

struct AsapqueryPrecomputeEngineArm {
    agent: ureq::Agent,
    base: String,
    request_timeout: Duration,
}

impl InstalledArm for AsapqueryPrecomputeEngineArm {
    fn plan_summary(&self) -> PlanSummary {
        PlanSummary::default()
    }

    fn ingest(&mut self, chunk: &SampleChunk) -> Result<StepReport, EngineError> {
        write_chunk(&self.agent, &self.base, chunk)?;
        Ok(StepReport::default())
    }

    fn evaluate(&mut self, _at_ms: i64) -> Result<(Answers, StepReport), EngineError> {
        Err(EngineError(
            "ASAPQuery's precompute engine answers no queries".into(),
        ))
    }

    fn footprint(&self) -> Result<Footprint, EngineError> {
        Err(EngineError(
            "ASAPQuery's precompute engine reports no stored bytes".into(),
        ))
    }

    fn sample_writers(&mut self, count: usize) -> Option<Vec<Box<dyn SampleWriter>>> {
        Some(
            (0..count)
                .map(|_| {
                    Box::new(RemoteWriteSampleWriter {
                        agent: agent(self.request_timeout),
                        url: write_url(&self.base),
                        encoder: RemoteWriteEncoder::default(),
                    }) as Box<dyn SampleWriter>
                })
                .collect(),
        )
    }
}

struct RemoteWriteSampleWriter {
    agent: ureq::Agent,
    url: String,
    encoder: RemoteWriteEncoder,
}

impl SampleWriter for RemoteWriteSampleWriter {
    fn write(&mut self, samples: &[Sample]) -> Result<(), EngineError> {
        let body = self.encoder.encode(samples);
        match send_to(&self.agent, &self.url, &body) {
            Ok(Sent::Accepted) => Ok(()),
            Ok(Sent::Busy) => Err(EngineError(format!(
                "{} answered busy; ASAPQuery's precompute engine is not expected to",
                self.url
            ))),
            Err(error) => Err(EngineError(format!("{error:#}"))),
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct AsapqueryPrecomputeEngineOptions {
    #[arg(long)]
    pub asapquery_precompute_engine_url: Option<String>,
    #[arg(long)]
    pub asapquery_precompute_engine_streaming_config: Option<PathBuf>,
    #[arg(long = "server-pid")]
    pub server_pids: Vec<u32>,
    #[arg(long, default_value = "unreported")]
    pub server_version: String,
    #[arg(long, default_value_t = 60)]
    pub request_timeout_seconds: u64,
}

pub fn write_throughput(matches: &clap::ArgMatches) -> anyhow::Result<Box<dyn EngineUnderTest>> {
    let options =
        <AsapqueryPrecomputeEngineOptions as clap::FromArgMatches>::from_arg_matches(matches)?;
    let (Some(url), Some(streaming_config), false) = (
        options.asapquery_precompute_engine_url,
        options.asapquery_precompute_engine_streaming_config,
        options.server_pids.is_empty(),
    ) else {
        anyhow::bail!("engine asapquery-precompute-engine needs --asapquery-precompute-engine-url, --asapquery-precompute-engine-streaming-config and --server-pid");
    };
    Ok(Box::new(AsapqueryPrecomputeEngine::new(
        AsapqueryPrecomputeEngineServer {
            url,
            pids: options.server_pids,
            streaming_config,
            version: options.server_version,
            request_timeout: Duration::from_secs(options.request_timeout_seconds),
        },
    )?))
}

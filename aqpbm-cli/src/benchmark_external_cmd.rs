//! `approxbench benchmark-external`: run one engine over a run directory, or join
//! every engine run's cells into the report.
//!
//! Example: `approxbench benchmark-external run --run-directory R --engine fake`, then
//! `approxbench benchmark-external report --run-directory R --truth fake/v1`.

use std::path::PathBuf;

use anyhow::{bail, Result};
use aqpbm_core::harness::benchmark_external::{
    read_workloads, report, run_engine, write_report, EngineRunSetup, ReportTruth,
};
use aqpbm_core::harness::cgroup::CgroupSource;
use aqpbm_core::harness::cpu_frequency::{governors, HeldGovernor, LINUX_CPU_ROOT};
use aqpbm_core::harness::engine::{ArmSpec, EngineUnderTest};
use aqpbm_core::harness::fake::FakeEngine;
use aqpbm_core::harness::one_shot::{
    run_one_shot_queries, OneShotQueryEngine, OneShotSchedule, OneShotSetup, LINUX_DROP_CACHES,
};
use aqpbm_core::harness::one_shot_report::{
    one_shot_report, write_one_shot_report, ConfigurationName,
};
use aqpbm_core::harness::protocol::{
    load_probe_for, MeasurementClocks, OsCpuClock, ProcessCpuFiles, Protocol,
};
use aqpbm_core::harness::run_directory::RunDirectory;
use aqpbm_core::harness::write_throughput::{run_write_throughput, WriteLoad};
use clap::{Parser, Subcommand};
use system_bench::engines_external::asapquery_backend_server::{
    AsapqueryBackendEngine, AsapqueryBackendServer,
};
use system_bench::engines_external::asapquery_precompute_engine_server::{
    AsapqueryPrecomputeEngine, AsapqueryPrecomputeEngineServer,
};
use system_bench::engines_external::clickhouse_connection::ClickHouseConnection;
use system_bench::engines_external::clickhouse_query_text::{
    ClickHouseQueryTextEngine, ClickHouseQueryTextServer,
};
use system_bench::engines_external::clickhouse_server::{ClickHouseEngine, ClickHouseServer};
use system_bench::engines_external::druid_sql::{context_value, DruidEngine, DruidServer};
use system_bench::engines_external::duckdb_command_line::{DuckDbCommandLine, DuckDbEngine};
use system_bench::engines_external::pilotdb::{PilotDbEngine, PilotDbOnDuckDb};
use system_bench::engines_external::postgresql_command_line::{
    PostgreSqlCommandLine, PostgreSqlEngine,
};
use system_bench::engines_external::victoriametrics_server::{
    VictoriaMetricsEngine, VictoriaMetricsServer,
};

/// The engines this binary can run.
pub const ENGINES: [&str; 4] = ["fake", "clickhouse", "victoriametrics", "asapquery-backend"];

pub const WRITE_THROUGHPUT_ENGINES: [&str; 1] = ["asapquery-precompute-engine"];

pub const ONE_SHOT_ENGINES: [&str; 5] = ["duckdb", "clickhouse", "pilotdb", "postgresql", "druid"];

#[derive(Parser, Debug)]
pub struct BenchmarkExternalArgs {
    #[command(subcommand)]
    pub command: BenchmarkExternalCmd,
}

#[derive(Subcommand, Debug)]
pub enum BenchmarkExternalCmd {
    /// Run every (workload, variant) cell of the run directory on one engine.
    Run(Box<RunArgs>),
    /// Join every engine run's cells into summary.csv, summary.md and accuracy.csv.
    Report(ReportArgs),
    #[command(
        about = "Load one engine through concurrent sample writers and append its acknowledged samples per second, CPU and memory to results/<engine>/write-throughput.jsonl"
    )]
    WriteThroughput(Box<WriteThroughputArgs>),
    #[command(
        about = "Run a workload's one-shot queries on one engine configuration and write results/<engine>/<workload>__<configuration>.one-shot.json"
    )]
    OneShotQueries(Box<OneShotQueriesArgs>),
    #[command(
        about = "Join the one-shot results into one-shot-summary.csv, one-shot-summary.md and one-shot-tables.csv"
    )]
    OneShotReport(OneShotReportArgs),
}

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// The run directory: workloads/, plans/<variant>/, results/.
    #[arg(long)]
    pub run_directory: PathBuf,
    /// The engine: `fake` (the harness's own fake engine), `clickhouse`,
    /// `victoriametrics` or `asapquery-backend`.
    #[arg(long)]
    pub engine: String,
    /// The ClickHouse server's HTTP interface, e.g. `http://10.0.0.1:8123`
    /// (engine `clickhouse`).
    #[arg(long)]
    pub clickhouse_url: Option<String>,
    /// The server's cgroup v2 directory, e.g.
    /// `/sys/fs/cgroup/system.slice/docker-<id>.scope` (engines `clickhouse`
    /// and `victoriametrics`).
    #[arg(long)]
    pub server_cgroup_directory: Option<PathBuf>,
    /// The SSH destination of the server host, e.g. `bench@10.0.0.2`; the
    /// cgroup directory is read over SSH. Omitted: it is read on this host.
    #[arg(long)]
    pub server_ssh_host: Option<String>,
    /// The SSH client used with `--server-ssh-host`.
    #[arg(long, default_value = "ssh")]
    pub ssh_program: String,
    /// `max_threads` of every ClickHouse statement; 1 for the main figures.
    #[arg(long, default_value_t = 1)]
    pub clickhouse_max_threads: u32,
    /// How long one ClickHouse statement may run, in seconds.
    #[arg(long, default_value_t = 600)]
    pub clickhouse_statement_timeout_seconds: u64,
    /// The VictoriaMetrics server's HTTP interface, e.g. `http://10.0.0.1:8428`
    /// (engine `victoriametrics`).
    #[arg(long)]
    pub victoriametrics_url: Option<String>,
    /// The server's `-streamAggr.config` file on the server host, replaced
    /// for each arm with a plan file (engine `victoriametrics`).
    #[arg(long)]
    pub victoriametrics_stream_aggregation_config: Option<PathBuf>,
    /// The ASAPQuery-backend server's HTTP interface, e.g.
    /// `http://10.0.0.1:8080` (engine `asapquery-backend`).
    #[arg(long)]
    pub asapquery_backend_url: Option<String>,
    /// Only these variants (repeatable); every variant under plans/ by default.
    #[arg(long = "variant")]
    pub variants: Vec<String>,
    /// The core the harness pins each step to.
    #[arg(long, default_value_t = 0)]
    pub core: usize,
    /// Runs discarded before measuring, per step.
    #[arg(long, default_value_t = Protocol::default().warmup_runs)]
    pub warmup: u32,
    /// Runs measured per step; odd.
    #[arg(long, default_value_t = Protocol::default().measured_runs)]
    pub runs: u32,
    /// A core busier than this fraction before a step refuses it.
    #[arg(long, default_value_t = Protocol::default().max_core_busy_fraction)]
    pub max_core_busy: f64,
}

#[derive(Parser, Debug)]
pub struct WriteThroughputArgs {
    #[arg(long)]
    pub run_directory: PathBuf,
    #[arg(long)]
    pub engine: String,
    #[arg(long)]
    pub workload: String,
    #[arg(long, default_value = "write-only")]
    pub arm: String,
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
    #[arg(long)]
    pub writers: usize,
    #[arg(long, default_value_t = 2000)]
    pub samples_per_request: usize,
    #[arg(long)]
    pub target_samples_per_second: Option<f64>,
    #[arg(long)]
    pub max_event_time_lead_ms: u64,
    #[arg(long, default_value_t = 15)]
    pub warmup_seconds: u64,
    #[arg(long, default_value_t = 30)]
    pub measure_seconds: u64,
    #[arg(long, default_value_t = 10)]
    pub stop_grace_seconds: u64,
    #[arg(long)]
    pub cpu_frequency_governor: Option<String>,
}

#[derive(Parser, Debug)]
pub struct OneShotQueriesArgs {
    #[arg(long)]
    pub run_directory: PathBuf,
    #[arg(long)]
    pub engine: String,
    #[arg(long)]
    pub workload: String,
    #[arg(long)]
    pub configuration: String,
    #[arg(long = "query")]
    pub queries: Vec<String>,
    #[arg(long)]
    pub query_variant: Option<String>,
    #[arg(long, default_value_t = 3)]
    pub runs: u32,
    #[arg(long)]
    pub cold_first_run: bool,
    #[arg(long, default_value = LINUX_DROP_CACHES)]
    pub drop_caches_file: PathBuf,
    #[arg(long)]
    pub data_directory: Option<PathBuf>,
    #[arg(long, default_value = "duckdb")]
    pub duckdb_program: PathBuf,
    #[arg(long)]
    pub duckdb_database: Option<PathBuf>,
    #[arg(long)]
    pub duckdb_threads: Option<u32>,
    #[arg(long)]
    pub duckdb_storage_version: Option<String>,
    #[arg(long)]
    pub clickhouse_url: Option<String>,
    #[arg(long, default_value = "default")]
    pub clickhouse_database: String,
    #[arg(long)]
    pub clickhouse_max_threads: Option<u32>,
    #[arg(long, default_value_t = 600)]
    pub clickhouse_statement_timeout_seconds: u64,
    #[arg(long)]
    pub clickhouse_restart_command: Option<String>,
    #[arg(long, default_value_t = 120)]
    pub clickhouse_ready_timeout_seconds: u64,
    #[arg(long, default_value_t = 3600)]
    pub clickhouse_idle_timeout_seconds: u64,
    #[arg(long = "clickhouse-setting", value_parser = parse_setting)]
    pub clickhouse_settings: Vec<(String, String)>,
    #[arg(long, default_value = "python3")]
    pub pilotdb_python: PathBuf,
    #[arg(long, default_value_t = 0.05)]
    pub pilotdb_relative_error: f64,
    #[arg(long, default_value_t = 0.05)]
    pub pilotdb_failure_probability: f64,
    #[arg(long, default_value = "psql")]
    pub postgresql_program: PathBuf,
    #[arg(long)]
    pub postgresql_connection: Option<String>,
    #[arg(long = "postgresql-setting", value_parser = parse_setting)]
    pub postgresql_settings: Vec<(String, String)>,
    #[arg(long)]
    pub postgresql_restart_command: Option<String>,
    #[arg(long, default_value_t = 120)]
    pub postgresql_ready_timeout_seconds: u64,
    #[arg(long)]
    pub druid_url: Option<String>,
    #[arg(long = "druid-context", value_parser = parse_setting)]
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
    #[arg(long)]
    pub cpu_frequency_governor: Option<String>,
}

#[derive(Parser, Debug)]
pub struct OneShotReportArgs {
    #[arg(long)]
    pub run_directory: PathBuf,
    #[arg(long)]
    pub truth: Option<String>,
    #[arg(long = "baseline")]
    pub baselines: Vec<String>,
}

#[derive(Parser, Debug)]
pub struct ReportArgs {
    #[arg(long)]
    pub run_directory: PathBuf,
    /// What answers are compared with: `declared` (each query's declared
    /// statistic, computed from the workload's data) or the cells of
    /// `<engine>/<variant>`.
    #[arg(long)]
    pub truth: String,
}

pub fn run(args: BenchmarkExternalArgs) -> Result<()> {
    match args.command {
        BenchmarkExternalCmd::Run(args) => run_engine_cmd(*args),
        BenchmarkExternalCmd::Report(args) => report_cmd(args),
        BenchmarkExternalCmd::WriteThroughput(args) => write_throughput_cmd(*args),
        BenchmarkExternalCmd::OneShotQueries(args) => one_shot_queries_cmd(*args),
        BenchmarkExternalCmd::OneShotReport(args) => one_shot_report_cmd(args),
    }
}

fn parse_setting(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name.to_string(), value.to_string())),
        _ => Err(format!("'{text}' is not name=value")),
    }
}

fn one_shot_engine(args: &OneShotQueriesArgs) -> Result<Box<dyn OneShotQueryEngine>> {
    let duckdb_database = || {
        args.duckdb_database
            .clone()
            .ok_or_else(|| anyhow::anyhow!("engine {} needs --duckdb-database", args.engine))
    };
    Ok(match args.engine.as_str() {
        "duckdb" => Box::new(DuckDbEngine::new(DuckDbCommandLine {
            program: args.duckdb_program.clone(),
            database: duckdb_database()?,
            threads: args.duckdb_threads,
            storage_version: args.duckdb_storage_version.clone(),
        })?),
        "clickhouse" => {
            let Some(url) = args.clickhouse_url.clone() else {
                bail!("engine clickhouse needs --clickhouse-url");
            };
            Box::new(ClickHouseQueryTextEngine::new(ClickHouseQueryTextServer {
                connection: ClickHouseConnection {
                    url,
                    statement_timeout: std::time::Duration::from_secs(
                        args.clickhouse_statement_timeout_seconds,
                    ),
                },
                database: args.clickhouse_database.clone(),
                max_threads: args.clickhouse_max_threads,
                restart_command: args.clickhouse_restart_command.clone(),
                ready_timeout: std::time::Duration::from_secs(
                    args.clickhouse_ready_timeout_seconds,
                ),
                idle_timeout: std::time::Duration::from_secs(args.clickhouse_idle_timeout_seconds),
                settings: args.clickhouse_settings.clone(),
            })?)
        }
        "pilotdb" => Box::new(PilotDbEngine::new(PilotDbOnDuckDb {
            python: args.pilotdb_python.clone(),
            database: duckdb_database()?,
            relative_error: args.pilotdb_relative_error,
            failure_probability: args.pilotdb_failure_probability,
            threads: args.duckdb_threads,
            scratch_directory: std::env::temp_dir(),
        })?),
        "postgresql" => {
            let Some(connection) = args.postgresql_connection.clone() else {
                bail!("engine postgresql needs --postgresql-connection");
            };
            Box::new(PostgreSqlEngine::new(PostgreSqlCommandLine {
                program: args.postgresql_program.clone(),
                connection,
                settings: args.postgresql_settings.clone(),
                restart_command: args.postgresql_restart_command.clone(),
                ready_timeout: std::time::Duration::from_secs(
                    args.postgresql_ready_timeout_seconds,
                ),
            })?)
        }
        "druid" => {
            let Some(url) = args.druid_url.clone() else {
                bail!("engine druid needs --druid-url");
            };
            Box::new(DruidEngine::new(DruidServer {
                url,
                context: args
                    .druid_context
                    .iter()
                    .map(|(name, value)| (name.clone(), context_value(value)))
                    .collect(),
                statement_timeout: std::time::Duration::from_secs(
                    args.druid_statement_timeout_seconds,
                ),
                load_timeout: std::time::Duration::from_secs(args.druid_load_timeout_seconds),
                restart_command: args.druid_restart_command.clone(),
                ready_timeout: std::time::Duration::from_secs(args.druid_ready_timeout_seconds),
                idle_timeout: std::time::Duration::from_secs(args.druid_idle_timeout_seconds),
            })?)
        }
        other => bail!(
            "unknown engine '{other}' for one-shot queries; known engines: {}",
            ONE_SHOT_ENGINES.join(", ")
        ),
    })
}

fn one_shot_queries_cmd(args: OneShotQueriesArgs) -> Result<()> {
    let run_directory = RunDirectory::open(&args.run_directory)?;
    let mut workloads = read_workloads(&run_directory)?;
    let Some(workload) = workloads.remove(&args.workload) else {
        bail!(
            "no workload {} under workloads/; known workloads: {}",
            args.workload,
            workloads.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };
    let mut engine = one_shot_engine(&args)?;
    let schedule = OneShotSchedule {
        runs: args.runs,
        cold_first_run: args.cold_first_run,
        queries: args.queries.clone(),
        query_variant: args.query_variant.clone(),
    };
    let setup = OneShotSetup {
        configuration: &args.configuration,
        schedule: &schedule,
        data_directory: args.data_directory.as_deref(),
        drop_caches: &args.drop_caches_file,
    };
    let cpu_root = std::path::Path::new(LINUX_CPU_ROOT);
    let held = args
        .cpu_frequency_governor
        .as_deref()
        .map(|governor| HeldGovernor::hold(cpu_root, governor))
        .transpose()?;
    if let Some(held) = &held {
        held.restore_on_interrupt_or_panic();
    }
    let in_effect = governors(cpu_root).unwrap_or_default();
    let measured = run_one_shot_queries(engine.as_mut(), &workload, &setup, &mut |line| {
        eprintln!("{line}")
    });
    let restored = held.map_or(Ok(()), HeldGovernor::restore);
    if let (Err(error), Err(_)) = (&restored, &measured) {
        eprintln!("restoring the CPU frequency governor: {error}");
    }
    let mut result = measured?;
    result.cpu_frequency_governors = in_effect;
    let path = run_directory.write_one_shot(&result)?;
    println!("{}", path.display());
    restored?;
    Ok(())
}

fn one_shot_report_cmd(args: OneShotReportArgs) -> Result<()> {
    let run_directory = RunDirectory::open(&args.run_directory)?;
    let results = run_directory.read_one_shot_results()?;
    let truth = args
        .truth
        .as_deref()
        .map(ConfigurationName::parse)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let baselines = args
        .baselines
        .iter()
        .map(|baseline| ConfigurationName::parse(baseline))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::msg)?;
    let report =
        one_shot_report(&results, truth.as_ref(), &baselines).map_err(anyhow::Error::msg)?;
    write_one_shot_report(run_directory.path(), &report)?;
    print!("{}", report.summary_markdown());
    Ok(())
}

fn write_throughput_cmd(args: WriteThroughputArgs) -> Result<()> {
    let engine: Box<dyn EngineUnderTest> = match args.engine.as_str() {
        "asapquery-precompute-engine" => {
            let (Some(url), Some(streaming_config), false) = (
                args.asapquery_precompute_engine_url.clone(),
                args.asapquery_precompute_engine_streaming_config.clone(),
                args.server_pids.is_empty(),
            ) else {
                bail!("engine asapquery-precompute-engine needs --asapquery-precompute-engine-url, --asapquery-precompute-engine-streaming-config and --server-pid");
            };
            Box::new(AsapqueryPrecomputeEngine::new(
                AsapqueryPrecomputeEngineServer {
                    url,
                    pids: args.server_pids.clone(),
                    streaming_config,
                    version: args.server_version.clone(),
                    request_timeout: std::time::Duration::from_secs(args.request_timeout_seconds),
                },
            )?)
        }
        other => bail!(
            "unknown engine '{other}' for write throughput; known engines: {}",
            WRITE_THROUGHPUT_ENGINES.join(", ")
        ),
    };
    let run_directory = RunDirectory::open(&args.run_directory)?;
    let mut workloads = read_workloads(&run_directory)?;
    let Some(workload) = workloads.remove(&args.workload) else {
        bail!(
            "no workload {} under workloads/; known workloads: {}",
            args.workload,
            workloads.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };
    let arm = ArmSpec {
        name: args.arm.clone(),
        raw_data_retained: true,
        engine_options: serde_json::json!({}),
    };
    let load = WriteLoad {
        writers: args.writers,
        samples_per_request: args.samples_per_request,
        target_samples_per_second: args.target_samples_per_second,
        max_event_time_lead_ms: args.max_event_time_lead_ms,
        warmup_ms: args.warmup_seconds * 1_000,
        measure_ms: args.measure_seconds * 1_000,
        stop_grace_ms: args.stop_grace_seconds * 1_000,
    };
    let cpu_root = std::path::Path::new(LINUX_CPU_ROOT);
    let held = args
        .cpu_frequency_governor
        .as_deref()
        .map(|governor| HeldGovernor::hold(cpu_root, governor))
        .transpose()?;
    if let Some(held) = &held {
        held.restore_on_interrupt_or_panic();
    }
    let in_effect = governors(cpu_root).unwrap_or_default();
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    let measured = run_write_throughput(engine.as_ref(), &workload, &arm, &load, &clocks);
    let restored = held.map_or(Ok(()), HeldGovernor::restore);
    if let (Err(error), Err(_)) = (&restored, &measured) {
        eprintln!("restoring the CPU frequency governor: {error}");
    }
    let mut result = measured?;
    result.cpu_frequency_governors = in_effect;
    run_directory.append_write_throughput(&result)?;
    println!("{}", serde_json::to_string(&result)?);
    restored?;
    Ok(())
}

fn run_engine_cmd(args: RunArgs) -> Result<()> {
    let engine: Box<dyn EngineUnderTest> = match args.engine.as_str() {
        "fake" => {
            if args.clickhouse_url.is_some()
                || args.asapquery_backend_url.is_some()
                || args.victoriametrics_url.is_some()
                || args.victoriametrics_stream_aggregation_config.is_some()
                || args.server_cgroup_directory.is_some()
                || args.server_ssh_host.is_some()
            {
                bail!(
                    "--clickhouse-url, --victoriametrics-url, --victoriametrics-stream-aggregation-config, --asapquery-backend-url, --server-cgroup-directory and --server-ssh-host are for the server engines only"
                );
            }
            Box::new(FakeEngine::default())
        }
        "clickhouse" => {
            let (Some(url), Some(cgroup)) = (args.clickhouse_url.clone(), server_cgroup(&args))
            else {
                bail!("engine clickhouse needs --clickhouse-url and --server-cgroup-directory");
            };
            Box::new(ClickHouseEngine::new(ClickHouseServer {
                connection: ClickHouseConnection {
                    url,
                    statement_timeout: std::time::Duration::from_secs(
                        args.clickhouse_statement_timeout_seconds,
                    ),
                },
                cgroup,
                max_threads: args.clickhouse_max_threads,
            })?)
        }
        "victoriametrics" => {
            let (Some(url), Some(cgroup)) =
                (args.victoriametrics_url.clone(), server_cgroup(&args))
            else {
                bail!("engine victoriametrics needs --victoriametrics-url and --server-cgroup-directory");
            };
            // Wall-clock data runs one arm at a time.
            if args.warmup != 0 || args.runs != 1 {
                bail!("engine victoriametrics runs one arm at a time: pass --warmup 0 --runs 1");
            }
            Box::new(VictoriaMetricsEngine::new(VictoriaMetricsServer {
                url,
                cgroup,
                stream_aggregation_config: args.victoriametrics_stream_aggregation_config.clone(),
                request_timeout: std::time::Duration::from_secs(600),
            })?)
        }
        "asapquery-backend" => {
            let (Some(url), Some(cgroup)) =
                (args.asapquery_backend_url.clone(), server_cgroup(&args))
            else {
                bail!(
                    "engine asapquery-backend needs --asapquery-backend-url and --server-cgroup-directory"
                );
            };
            if args.warmup != 0 || args.runs != 1 {
                bail!("engine asapquery-backend runs one arm per server start: pass --warmup 0 --runs 1");
            }
            Box::new(AsapqueryBackendEngine::new(AsapqueryBackendServer {
                url,
                cgroup,
                request_timeout: std::time::Duration::from_secs(600),
            })?)
        }
        other => bail!(
            "unknown engine '{other}'; known engines: {}",
            ENGINES.join(", ")
        ),
    };
    let run_directory = RunDirectory::open(&args.run_directory)?;
    let protocol = Protocol {
        warmup_runs: args.warmup,
        measured_runs: args.runs,
        max_core_busy_fraction: args.max_core_busy,
    };
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    // The idle check watches the server's cores for an external server, the
    // harness core for an in-process engine.
    let mut probe = load_probe_for(&engine.cpu_accounting(), &ProcessCpuFiles::os())?;
    let only = (!args.variants.is_empty()).then_some(args.variants.as_slice());
    let cells = run_engine(
        &run_directory,
        engine.as_ref(),
        only,
        EngineRunSetup {
            protocol: &protocol,
            core: args.core,
            probe: probe.as_mut(),
            clocks: &clocks,
            args: std::env::args().collect(),
        },
    )?;
    for cell in cells {
        println!("{} {} {:?}", cell.workload, cell.variant, cell.outcome);
    }
    Ok(())
}

fn server_cgroup(args: &RunArgs) -> Option<CgroupSource> {
    let cgroup_directory = args.server_cgroup_directory.clone()?;
    Some(match args.server_ssh_host.clone() {
        Some(host) => CgroupSource::Remote {
            ssh_program: args.ssh_program.clone(),
            host,
            cgroup_directory,
        },
        None => CgroupSource::Local(cgroup_directory),
    })
}

fn report_cmd(args: ReportArgs) -> Result<()> {
    let truth = ReportTruth::parse(&args.truth).map_err(anyhow::Error::msg)?;
    let run_directory = RunDirectory::open(&args.run_directory)?;
    let report = report(&run_directory, &truth)?;
    write_report(&run_directory, &report)?;
    print!("{}", report.summary_markdown());
    Ok(())
}

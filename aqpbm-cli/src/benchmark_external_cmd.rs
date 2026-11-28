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
use aqpbm_core::harness::cpu_frequency::{governors, HeldGovernor, LINUX_CPU_ROOT};
use aqpbm_core::harness::engine::ArmSpec;
use aqpbm_core::harness::one_shot::{
    run_one_shot_queries, OneShotSchedule, OneShotSetup, LINUX_DROP_CACHES,
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
use system_bench::registry::{
    self, OneShotQueries, PeriodicQueries, SystemOptions, WriteThroughput,
};

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
    #[command(flatten)]
    pub systems: SystemOptions<PeriodicQueries>,
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
    #[command(flatten)]
    pub systems: SystemOptions<WriteThroughput>,
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
    #[arg(long)]
    pub cpu_frequency_governor: Option<String>,
    #[command(flatten)]
    pub systems: SystemOptions<OneShotQueries>,
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
    let Some(system) = registry::find::<OneShotQueries>(&args.engine) else {
        bail!(
            "unknown engine '{}' for one-shot queries; known engines: {}",
            args.engine,
            registry::names::<OneShotQueries>().join(", ")
        );
    };
    let mut engine = (system.build)(&args.systems.matches)?;
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
    let Some(system) = registry::find::<WriteThroughput>(&args.engine) else {
        bail!(
            "unknown engine '{}' for write throughput; known engines: {}",
            args.engine,
            registry::names::<WriteThroughput>().join(", ")
        );
    };
    let engine = (system.build)(&args.systems.matches)?;
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
    let protocol = Protocol {
        warmup_runs: args.warmup,
        measured_runs: args.runs,
        max_core_busy_fraction: args.max_core_busy,
    };
    let Some(system) = registry::find::<PeriodicQueries>(&args.engine) else {
        bail!(
            "unknown engine '{}'; known engines: {}",
            args.engine,
            registry::names::<PeriodicQueries>().join(", ")
        );
    };
    let engine = (system.build)(&args.systems.matches, &protocol)?;
    let run_directory = RunDirectory::open(&args.run_directory)?;
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

fn report_cmd(args: ReportArgs) -> Result<()> {
    let truth = ReportTruth::parse(&args.truth).map_err(anyhow::Error::msg)?;
    let run_directory = RunDirectory::open(&args.run_directory)?;
    let report = report(&run_directory, &truth)?;
    write_report(&run_directory, &report)?;
    print!("{}", report.summary_markdown());
    Ok(())
}

//! `approxbench benchmark-external`: run one engine over a run directory, or join
//! every engine run's cells into the report.
//!
//! Example: `approxbench benchmark-external run --run-directory R --engine fake`, then
//! `approxbench benchmark-external report --run-directory R --truth fake/v1`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use aqpbm_core::harness::benchmark_external::{report, run_engine, write_report, EngineRunSetup};
use aqpbm_core::harness::cgroup::CgroupSource;
use aqpbm_core::harness::engine::EngineUnderTest;
use aqpbm_core::harness::fake::FakeEngine;
use aqpbm_core::harness::protocol::{
    load_probe_for, MeasurementClocks, OsCpuClock, ProcessCpuFiles, Protocol,
};
use aqpbm_core::harness::run_directory::RunDirectory;
use clap::{Parser, Subcommand};
use system_bench::engines_external::clickhouse_server::{ClickHouseEngine, ClickHouseServer};

/// The engines this binary can run.
pub const ENGINES: [&str; 2] = ["fake", "clickhouse"];

#[derive(Parser, Debug)]
pub struct BenchmarkExternalArgs {
    #[command(subcommand)]
    pub command: BenchmarkExternalCmd,
}

#[derive(Subcommand, Debug)]
pub enum BenchmarkExternalCmd {
    /// Run every (workload, variant) cell of the run directory on one engine.
    Run(RunArgs),
    /// Join every engine run's cells into summary.csv, summary.md and accuracy.csv.
    Report(ReportArgs),
}

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// The run directory: workloads/, plans/<variant>/, results/.
    #[arg(long)]
    pub run_directory: PathBuf,
    /// The engine: `fake` (the harness's own fake engine) or `clickhouse`.
    #[arg(long)]
    pub engine: String,
    /// The ClickHouse server's HTTP interface, e.g. `http://10.0.0.1:8123`
    /// (engine `clickhouse`).
    #[arg(long)]
    pub clickhouse_url: Option<String>,
    /// The server's cgroup v2 directory, e.g.
    /// `/sys/fs/cgroup/system.slice/docker-<id>.scope` (engine `clickhouse`).
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
pub struct ReportArgs {
    #[arg(long)]
    pub run_directory: PathBuf,
    /// The cells answers are compared with: `<engine>/<variant>`.
    #[arg(long)]
    pub truth: String,
}

pub fn run(args: BenchmarkExternalArgs) -> Result<()> {
    match args.command {
        BenchmarkExternalCmd::Run(args) => run_engine_cmd(args),
        BenchmarkExternalCmd::Report(args) => report_cmd(args),
    }
}

fn run_engine_cmd(args: RunArgs) -> Result<()> {
    let engine: Box<dyn EngineUnderTest> = match args.engine.as_str() {
        "fake" => {
            if args.clickhouse_url.is_some()
                || args.server_cgroup_directory.is_some()
                || args.server_ssh_host.is_some()
            {
                bail!(
                    "--clickhouse-url, --server-cgroup-directory and --server-ssh-host are for engine clickhouse only"
                );
            }
            Box::new(FakeEngine::default())
        }
        "clickhouse" => {
            let (Some(url), Some(cgroup_directory)) = (
                args.clickhouse_url.clone(),
                args.server_cgroup_directory.clone(),
            ) else {
                bail!("engine clickhouse needs --clickhouse-url and --server-cgroup-directory");
            };
            let cgroup = match args.server_ssh_host.clone() {
                Some(host) => CgroupSource::Remote {
                    ssh_program: args.ssh_program.clone(),
                    host,
                    cgroup_directory,
                },
                None => CgroupSource::Local(cgroup_directory),
            };
            Box::new(ClickHouseEngine::new(ClickHouseServer {
                url,
                cgroup,
                max_threads: args.clickhouse_max_threads,
                statement_timeout: std::time::Duration::from_secs(
                    args.clickhouse_statement_timeout_seconds,
                ),
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

fn report_cmd(args: ReportArgs) -> Result<()> {
    let (engine, variant) = args
        .truth
        .split_once('/')
        .context("--truth must be <engine>/<variant>")?;
    let run_directory = RunDirectory::open(&args.run_directory)?;
    let report = report(&run_directory, engine, variant)?;
    write_report(&run_directory, &report)?;
    print!("{}", report.summary_markdown());
    Ok(())
}

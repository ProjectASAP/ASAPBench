//! `approxbench benchmark-external`: run one engine over a run directory, or join
//! every engine run's cells into the report.
//!
//! Example: `approxbench benchmark-external run --run-directory R --engine fake`, then
//! `approxbench benchmark-external report --run-directory R --truth fake/v1`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use aqpbm_core::harness::benchmark_external::{report, run_engine, write_report, EngineRunSetup};
use aqpbm_core::harness::engine::EngineUnderTest;
use aqpbm_core::harness::fake::FakeEngine;
use aqpbm_core::harness::protocol::{
    load_probe_for, MeasurementClocks, OsCpuClock, ProcessCpuFiles, Protocol,
};
use aqpbm_core::harness::run_directory::RunDirectory;
use clap::{Parser, Subcommand};

/// The engines this binary can run.
pub const ENGINES: [&str; 1] = ["fake"];

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
    /// The engine: `fake` (the harness's own fake engine).
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
        "fake" => Box::new(FakeEngine::default()),
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

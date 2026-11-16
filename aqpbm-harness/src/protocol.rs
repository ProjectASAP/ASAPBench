//! The measurement protocol: pin the job to one core, check the core is idle
//! before and after, discard warm-up runs, take the median of an odd number
//! of measured runs, and call differences inside the noise band ties.
//!
//! Example: with warm-up 1 and 5 runs, a job whose calls cost 100, 50, 70,
//! 60, 90 and 80 ns records the samples 50, 70, 60, 90, 80 and the median 70.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::{EngineError, StepCost, StepReport};

/// The clocks the harness reads around a step.
pub trait CpuClock {
    /// CPU of the calling thread, in ns.
    fn thread_cpu_ns(&self) -> u64;
    /// CPU of the measured processes, in ns.
    fn process_cpu_ns(&self) -> u64;
    /// Wall time, in ns from an arbitrary origin. On the clock so a fake clock
    /// makes whole runs deterministic.
    fn wall_ns(&self) -> u64;
}

/// The OS clocks: `aqpbm_core::metrics::time::ThreadCpuTime` for the thread,
/// `CpuTimeSampler` (`getrusage`) for the process, `Instant` for wall time.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsCpuClock;

impl CpuClock for OsCpuClock {
    fn thread_cpu_ns(&self) -> u64 {
        todo!("implemented after test approval")
    }
    fn process_cpu_ns(&self) -> u64 {
        todo!("implemented after test approval")
    }
    fn wall_ns(&self) -> u64 {
        todo!("implemented after test approval")
    }
}

/// Pins the calling thread to `core` with `sched_setaffinity`. A core outside
/// the allowed set is refused, naming the set (for example `0-55`).
pub fn pin_current_thread(core: usize) -> Result<(), ProtocolError> {
    let _ = core;
    todo!("implemented after test approval")
}

/// Busy fraction of `core` between two `/proc/stat` texts:
/// `1 - (Δidle + Δiowait) / Δtotal` over the `cpu<core>` line.
pub fn core_busy_fraction(core: usize, before: &str, after: &str) -> Result<f64, ProtocolError> {
    let _ = (core, before, after);
    todo!("implemented after test approval")
}

/// `utime + stime` (fields 14 and 15) of one `/proc/<pid>/stat` line, in ns
/// at `ticks_per_second`. The command name in parentheses may hold spaces and
/// parentheses.
pub fn process_cpu_ns_from_stat(stat: &str, ticks_per_second: u64) -> Result<u64, ProtocolError> {
    let _ = (stat, ticks_per_second);
    todo!("implemented after test approval")
}

/// The summed CPU of `pids`, read from `<proc_root>/<pid>/stat`. A missing
/// pid is an error naming it.
pub fn processes_cpu_ns(
    proc_root: &Path,
    pids: &[u32],
    ticks_per_second: u64,
) -> Result<u64, ProtocolError> {
    let _ = (proc_root, pids, ticks_per_second);
    todo!("implemented after test approval")
}

/// How busy the machine is; the OS form reads `/proc/stat` and `/proc/loadavg`.
pub trait LoadProbe {
    /// The busy fraction of `core` over a short sampling interval.
    fn core_busy_fraction(&mut self, core: usize) -> Result<f64, ProtocolError>;
    /// The 1-minute load average.
    fn loadavg(&mut self) -> f64;
}

/// The protocol of a run, recorded in `run.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Protocol {
    /// Runs discarded before measuring.
    pub warmup_runs: u32,
    /// Runs measured; odd, so the median is one measured value. Default 5.
    pub measured_runs: u32,
    /// A core busier than this before the job refuses it; after the job it
    /// marks the job. Default 0.10.
    pub max_core_busy_fraction: f64,
}

impl Default for Protocol {
    fn default() -> Self {
        Protocol {
            warmup_runs: 1,
            measured_runs: 5,
            max_core_busy_fraction: 0.10,
        }
    }
}

/// One measured job: where it ran, the load around it, and its samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRecord {
    pub core: usize,
    pub busy_before: f64,
    pub busy_after: f64,
    /// The load average before and after.
    pub loadavg: [f64; 2],
    pub marks: Vec<JobMark>,
    /// One cost per measured run, warm-up excluded.
    pub samples: Vec<StepCost>,
    /// The sample with the median thread CPU (process CPU for a `Processes`
    /// engine).
    pub median: StepCost,
}

/// Something that happened around a job that a reader must know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobMark {
    /// The core was busier than the limit after the job.
    BusyAfter { busy_fraction: f64 },
}

impl Protocol {
    /// Pin, check idle, warm up, run `measured_runs` times, check idle again.
    pub fn run_job(
        &self,
        core: usize,
        probe: &mut dyn LoadProbe,
        clock: &dyn CpuClock,
        job: &mut dyn FnMut() -> Result<StepReport, EngineError>,
    ) -> Result<JobRecord, ProtocolError> {
        let _ = (core, probe, clock, job);
        todo!("implemented after test approval")
    }
}

/// The median of an odd number of values: the middle one, never a mean.
pub fn median(values: &[f64]) -> Result<f64, ProtocolError> {
    let _ = values;
    todo!("implemented after test approval")
}

/// `|m_a - m_b| / min(m_a, m_b)` of the medians of two arms that ran the same
/// plan.
pub fn noise_band(arm_a: &[f64], arm_b: &[f64]) -> Result<f64, ProtocolError> {
    let _ = (arm_a, arm_b);
    todo!("implemented after test approval")
}

/// How a candidate's cost compares with a reference's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Comparison {
    Cheaper,
    Tie,
    Costlier,
}

/// `candidate` against `reference`: a tie when the larger is at most
/// `(1 + band)` times the smaller.
pub fn compare(candidate: f64, reference: f64, band: f64) -> Comparison {
    let _ = (candidate, reference, band);
    todo!("implemented after test approval")
}

/// A protocol step that refused or failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    /// The core is outside this process's allowed set.
    #[error("core {core} is outside the allowed cores {allowed}")]
    CoreNotAllowed { core: usize, allowed: String },
    /// The core was busy before the job; the job did not run.
    #[error("core {core} is {:.0}% busy (load average {loadavg}), above the {:.0}% limit: pick an idle core (taskset) or wait", busy_fraction * 100.0, limit * 100.0)]
    CoreBusy {
        core: usize,
        busy_fraction: f64,
        loadavg: f64,
        limit: f64,
    },
    /// Two `/proc/stat` samples with no ticks between them.
    #[error("core {core}: /proc/stat did not advance between the two samples, so idleness is unmeasured")]
    IdleUnmeasurable { core: usize },
    /// An even or zero run count.
    #[error("measured_runs must be odd so the median is one measured value, got {0}")]
    RunCountNotOdd(u32),
    /// A median of zero or less, which cannot scale a noise band.
    #[error("a noise band needs positive medians, got {median}")]
    NonPositiveMedian { median: f64 },
    /// `/proc/stat` or `/proc/<pid>/stat` text that does not parse.
    #[error("cannot parse {what}: {message}")]
    Parse { what: String, message: String },
    /// A process whose stat file is missing.
    #[error("process {pid} has no {path}")]
    MissingProcess { pid: u32, path: String },
    /// Pinning failed in the OS.
    #[error("pinning to core {core} failed: {message}")]
    Pin { core: usize, message: String },
    /// The job's engine call failed.
    #[error("the job failed: {0}")]
    Job(#[from] EngineError),
}

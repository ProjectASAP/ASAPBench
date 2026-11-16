//! The measurement protocol: pin the job to one core, check the core is idle
//! before and after, discard warm-up runs, take the median of an odd number
//! of measured runs, and call differences inside the noise band ties.
//!
//! Example: with warm-up 1 and 5 runs, a job whose calls cost 100, 50, 70,
//! 60, 90 and 80 ns records the samples 50, 70, 60, 90, 80 and the median 70.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use aqpbm_core::metrics::{CpuTimeSampler, ThreadCpuTime};

use serde::{Deserialize, Serialize};

use crate::engine::{CpuAccounting, EngineError, StepCost, StepReport};

/// The clocks the harness reads around a step.
pub trait CpuClock {
    /// CPU of the calling thread, in ns.
    fn thread_cpu_ns(&self) -> u64;
    /// CPU of the measured processes, in ns.
    fn process_cpu_ns(&self) -> u64;
    /// Wall time, in ns from an arbitrary origin. On the clock so a fake clock
    /// makes whole runs deterministic.
    fn wall_ns(&self) -> u64;
    /// The first failed reading since the last call, if any. A clock whose
    /// source can fail (a server process that exited) keeps returning its
    /// last good reading and reports the failure here; the protocol checks
    /// after every measured run, so a failed reading never becomes a sample.
    fn take_read_error(&self) -> Option<ProtocolError> {
        None
    }
}

/// The OS clocks: `aqpbm_core::metrics::time::ThreadCpuTime` for the thread,
/// `CpuTimeSampler` (`getrusage`) for the process, `Instant` for wall time.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsCpuClock;

/// The process CPU and wall origins [`OsCpuClock`] reads from: both clocks
/// count from the first reading in the process.
fn os_clock_origin() -> &'static (CpuTimeSampler, Instant) {
    static ORIGIN: OnceLock<(CpuTimeSampler, Instant)> = OnceLock::new();
    ORIGIN.get_or_init(|| (CpuTimeSampler::start(), Instant::now()))
}

impl CpuClock for OsCpuClock {
    fn thread_cpu_ns(&self) -> u64 {
        ThreadCpuTime::now_ns()
    }
    fn process_cpu_ns(&self) -> u64 {
        let used = os_clock_origin().0.finish();
        used.user_ns + used.sys_ns
    }
    fn wall_ns(&self) -> u64 {
        os_clock_origin().1.elapsed().as_nanos() as u64
    }
}

/// Where a `Processes` engine's CPU is read: a `/proc` directory and the
/// clock tick rate of its stat counters. Example: on Linux, `/proc` at 100
/// ticks per second (`getconf CLK_TCK`), so a `utime` of 250 is 2.5 s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcFs {
    pub root: PathBuf,
    pub ticks_per_second: u64,
}

impl ProcFs {
    /// The machine's `/proc` and `sysconf(_SC_CLK_TCK)` (100 when unknown).
    pub fn os() -> Self {
        #[cfg(unix)]
        // SAFETY: `sysconf` reads a constant of the C library and has no
        // preconditions.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        #[cfg(not(unix))]
        let ticks = 100;
        ProcFs {
            root: PathBuf::from("/proc"),
            ticks_per_second: if ticks > 0 { ticks as u64 } else { 100 },
        }
    }
}

/// The clock of a `Processes` engine (an external server): process CPU is the
/// summed `utime + stime` of the server pids in `<proc>/<pid>/stat`, thread
/// CPU is always 0 (the harness thread does none of the engine's work), and
/// wall time is the OS monotonic clock. Example: servers 101 and 102 at
/// 100 ticks per second with 80 + 20 and 150 + 50 ticks read 3 s.
#[derive(Debug)]
pub struct ProcessesCpuClock {
    pids: Vec<u32>,
    proc_fs: ProcFs,
    /// The last successful reading, returned after a failed one.
    last_process_cpu_ns: Cell<u64>,
    /// The first failed reading, until [`CpuClock::take_read_error`] takes it.
    read_error: RefCell<Option<ProtocolError>>,
}

impl ProcessesCpuClock {
    /// Reads the pids once, so a missing server fails here, before any job.
    pub fn new(pids: Vec<u32>, proc_fs: ProcFs) -> Result<Self, ProtocolError> {
        let first = processes_cpu_ns(&proc_fs.root, &pids, proc_fs.ticks_per_second)?;
        Ok(ProcessesCpuClock {
            pids,
            proc_fs,
            last_process_cpu_ns: Cell::new(first),
            read_error: RefCell::new(None),
        })
    }
}

impl CpuClock for ProcessesCpuClock {
    fn thread_cpu_ns(&self) -> u64 {
        0
    }
    fn process_cpu_ns(&self) -> u64 {
        match processes_cpu_ns(
            &self.proc_fs.root,
            &self.pids,
            self.proc_fs.ticks_per_second,
        ) {
            Ok(ns) => {
                self.last_process_cpu_ns.set(ns);
                ns
            }
            Err(error) => {
                self.read_error.borrow_mut().get_or_insert(error);
                self.last_process_cpu_ns.get()
            }
        }
    }
    fn wall_ns(&self) -> u64 {
        OsCpuClock.wall_ns()
    }
    fn take_read_error(&self) -> Option<ProtocolError> {
        self.read_error.borrow_mut().take()
    }
}

/// What the harness reads CPU from, per engine accounting: the caller's
/// clock for a `CallingThread` engine ([`OsCpuClock`], or a fake clock in
/// tests), and a [`ProcessesCpuClock`] it builds itself over `proc_fs` for a
/// `Processes` engine, so an external server is never timed on the harness
/// process.
pub struct MeasurementClocks<'a> {
    pub calling_thread: &'a dyn CpuClock,
    pub proc_fs: ProcFs,
}

impl<'a> MeasurementClocks<'a> {
    /// [`OsCpuClock`] and the machine's `/proc`.
    pub fn os() -> MeasurementClocks<'static> {
        MeasurementClocks {
            calling_thread: &OsCpuClock,
            proc_fs: ProcFs::os(),
        }
    }

    /// `calling_thread` and the machine's `/proc`.
    pub fn with_calling_thread(calling_thread: &'a dyn CpuClock) -> Self {
        MeasurementClocks {
            calling_thread,
            proc_fs: ProcFs::os(),
        }
    }

    /// The clock an engine with `accounting` is measured on. For
    /// `Processes(pids)` this reads every pid once and fails on a missing one.
    pub fn clock_for(&self, accounting: &CpuAccounting) -> Result<EngineClock<'a>, ProtocolError> {
        Ok(match accounting {
            CpuAccounting::CallingThread => EngineClock::CallingThread(self.calling_thread),
            CpuAccounting::Processes(pids) => {
                EngineClock::Processes(ProcessesCpuClock::new(pids.clone(), self.proc_fs.clone())?)
            }
        })
    }
}

/// The clock [`MeasurementClocks::clock_for`] picked for one engine.
pub enum EngineClock<'a> {
    CallingThread(&'a dyn CpuClock),
    Processes(ProcessesCpuClock),
}

impl EngineClock<'_> {
    pub fn as_clock(&self) -> &dyn CpuClock {
        match self {
            EngineClock::CallingThread(clock) => *clock,
            EngineClock::Processes(clock) => clock,
        }
    }

    /// The clock the median of a job is taken on.
    pub(crate) fn median_clock(&self) -> MedianClock {
        match self {
            EngineClock::CallingThread(_) => MedianClock::Thread,
            EngineClock::Processes(_) => MedianClock::Process,
        }
    }
}

/// Pins the calling thread to `core` with `sched_setaffinity`. A core outside
/// the allowed set is refused, naming the set (for example `0-55`).
#[cfg(target_os = "linux")]
pub fn pin_current_thread(core: usize) -> Result<(), ProtocolError> {
    let set_bytes = std::mem::size_of::<libc::cpu_set_t>();
    // SAFETY: `allowed` is a plain bit mask owned by this frame;
    // `sched_getaffinity` writes at most `set_bytes` into it and the return
    // value is checked. The process id reads the process's (main thread's)
    // mask, so a thread pinned earlier does not narrow the allowed set.
    let allowed = unsafe {
        let mut allowed: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(libc::getpid(), set_bytes, &mut allowed) != 0 {
            return Err(ProtocolError::Pin {
                core,
                message: std::io::Error::last_os_error().to_string(),
            });
        }
        allowed
    };
    let max_cores = 8 * set_bytes;
    // SAFETY: `CPU_ISSET` reads one bit of `allowed` for a core below the set size.
    let is_allowed = |c: usize| c < max_cores && unsafe { libc::CPU_ISSET(c, &allowed) };
    if !is_allowed(core) {
        let cores: Vec<usize> = (0..max_cores).filter(|&c| is_allowed(c)).collect();
        return Err(ProtocolError::CoreNotAllowed {
            core,
            allowed: core_ranges(&cores),
        });
    }
    // SAFETY: as above; `sched_setaffinity(0, ..)` changes only the calling thread.
    let status = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, set_bytes, &set)
    };
    if status != 0 {
        return Err(ProtocolError::Pin {
            core,
            message: std::io::Error::last_os_error().to_string(),
        });
    }
    Ok(())
}

/// Pinning needs Linux's `sched_setaffinity`; elsewhere it is refused.
#[cfg(not(target_os = "linux"))]
pub fn pin_current_thread(core: usize) -> Result<(), ProtocolError> {
    Err(ProtocolError::Pin {
        core,
        message: "thread pinning is supported on Linux only".into(),
    })
}

/// Ascending cores as ranges, as `taskset` writes them: `[0, 1, 2, 5]` is `0-2,5`.
#[cfg(target_os = "linux")]
fn core_ranges(cores: &[usize]) -> String {
    let mut ranges: Vec<String> = Vec::new();
    let mut index = 0;
    while index < cores.len() {
        let first = cores[index];
        let mut last = first;
        while index + 1 < cores.len() && cores[index + 1] == last + 1 {
            index += 1;
            last = cores[index];
        }
        ranges.push(if first == last {
            first.to_string()
        } else {
            format!("{first}-{last}")
        });
        index += 1;
    }
    ranges.join(",")
}

/// Busy fraction of `core` between two `/proc/stat` texts:
/// `1 - (Δidle + Δiowait) / Δtotal` over the `cpu<core>` line.
pub fn core_busy_fraction(core: usize, before: &str, after: &str) -> Result<f64, ProtocolError> {
    let before = core_ticks(core, before)?;
    let after = core_ticks(core, after)?;
    let delta = |field: usize| after[field].saturating_sub(before[field]);
    // user nice system idle iowait irq softirq steal; guest and guest_nice
    // are already inside user and nice.
    let total: u64 = (0..8).map(delta).sum();
    if total == 0 {
        return Err(ProtocolError::IdleUnmeasurable { core });
    }
    let idle = delta(3) + delta(4);
    Ok(1.0 - idle as f64 / total as f64)
}

/// The first 8 tick counters of the `cpu<core>` line of a `/proc/stat` text.
fn core_ticks(core: usize, stat: &str) -> Result<[u64; 8], ProtocolError> {
    let name = format!("cpu{core}");
    let parse_error = |message: String| ProtocolError::Parse {
        what: format!("/proc/stat line {name}"),
        message,
    };
    let line = stat
        .lines()
        .find(|line| line.split_whitespace().next() == Some(name.as_str()))
        .ok_or_else(|| parse_error("no such line".into()))?;
    let mut ticks = [0_u64; 8];
    let mut fields = line.split_whitespace().skip(1);
    for tick in &mut ticks {
        let field = fields
            .next()
            .ok_or_else(|| parse_error("fewer than 8 counters".into()))?;
        *tick = field
            .parse()
            .map_err(|_| parse_error(format!("counter {field:?} is not a number")))?;
    }
    Ok(ticks)
}

/// `utime + stime` (fields 14 and 15) of one `/proc/<pid>/stat` line, in ns
/// at `ticks_per_second`. The command name in parentheses may hold spaces and
/// parentheses.
pub fn process_cpu_ns_from_stat(stat: &str, ticks_per_second: u64) -> Result<u64, ProtocolError> {
    let parse_error = |message: &str| ProtocolError::Parse {
        what: "/proc/<pid>/stat".into(),
        message: message.into(),
    };
    // The command name may hold ") ", so the fields start after the last ')'.
    let after_comm = stat
        .rfind(')')
        .map(|end| &stat[end + 1..])
        .ok_or_else(|| parse_error("no ')' closing the command name"))?;
    // Field 3 (state) is the first after the command name, so field n is at
    // index n - 3.
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let ticks = |field: usize| -> Result<u64, ProtocolError> {
        fields
            .get(field - 3)
            .ok_or_else(|| parse_error("fewer than 15 fields"))?
            .parse::<u64>()
            .map_err(|_| parse_error("utime or stime is not a number"))
    };
    if ticks_per_second == 0 {
        return Err(parse_error("ticks_per_second is 0"));
    }
    let cpu_ticks = u128::from(ticks(14)?) + u128::from(ticks(15)?);
    Ok((cpu_ticks * 1_000_000_000 / u128::from(ticks_per_second)) as u64)
}

/// The summed CPU of `pids`, read from `<proc_root>/<pid>/stat`. A missing
/// pid is an error naming it.
pub fn processes_cpu_ns(
    proc_root: &Path,
    pids: &[u32],
    ticks_per_second: u64,
) -> Result<u64, ProtocolError> {
    let mut total_ns = 0;
    for &pid in pids {
        let path = proc_root.join(pid.to_string()).join("stat");
        let stat = std::fs::read_to_string(&path).map_err(|_| ProtocolError::MissingProcess {
            pid,
            path: path.display().to_string(),
        })?;
        total_ns += process_cpu_ns_from_stat(&stat, ticks_per_second)?;
    }
    Ok(total_ns)
}

/// How busy the machine is; the OS form reads `/proc/stat` and `/proc/loadavg`.
pub trait LoadProbe {
    /// The busy fraction of `core` over a short sampling interval.
    fn core_busy_fraction(&mut self, core: usize) -> Result<f64, ProtocolError>;
    /// The 1-minute load average.
    fn loadavg(&mut self) -> f64;
}

/// The OS load probe: two `/proc/stat` reads `sample_interval` apart, and the
/// first field of `/proc/loadavg` (`NaN` when unreadable; recorded as `None`).
#[derive(Debug, Clone, Copy)]
pub struct OsLoadProbe {
    pub sample_interval: std::time::Duration,
}

impl Default for OsLoadProbe {
    /// 100 ms: `/proc/stat` counts in 10 ms ticks (100 Hz), so 10 ticks per
    /// sample resolve the 10% busy limit to one tick. A shorter interval
    /// cannot: at 50 ms one stray tick already reads 20% busy. The cost is
    /// cut instead by sampling once per job, not twice: in a run of jobs each
    /// job's after-sample is the next job's before-sample (see
    /// [`Protocol::run_job`]).
    fn default() -> Self {
        OsLoadProbe {
            sample_interval: std::time::Duration::from_millis(100),
        }
    }
}

impl LoadProbe for OsLoadProbe {
    fn core_busy_fraction(&mut self, core: usize) -> Result<f64, ProtocolError> {
        let read = || {
            std::fs::read_to_string("/proc/stat").map_err(|error| ProtocolError::Parse {
                what: "/proc/stat".into(),
                message: error.to_string(),
            })
        };
        let before = read()?;
        std::thread::sleep(self.sample_interval);
        core_busy_fraction(core, &before, &read()?)
    }

    fn loadavg(&mut self) -> f64 {
        std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|text| text.split_whitespace().next()?.parse().ok())
            .unwrap_or(f64::NAN)
    }
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
    /// The 1-minute load average before and after; `None` when unreadable.
    pub loadavg: [Option<f64>; 2],
    pub marks: Vec<JobMark>,
    /// One cost per measured run, warm-up excluded.
    pub samples: Vec<StepCost>,
    /// The sample with the median thread CPU (process CPU for a `Processes`
    /// engine).
    pub median: StepCost,
}

impl JobRecord {
    /// The after-sample of this job, to stand in for the next job's
    /// before-sample on the same core.
    pub(crate) fn load_after(&self) -> CoreLoad {
        CoreLoad {
            busy_fraction: self.busy_after,
            loadavg: self.loadavg[1],
        }
    }
}

/// Something that happened around a job that a reader must know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobMark {
    /// The core was busier than the limit after the job.
    BusyAfter { busy_fraction: f64 },
}

/// One idle-check sample of a core: its busy fraction over the sampling
/// interval and the load average read after it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CoreLoad {
    pub busy_fraction: f64,
    pub loadavg: Option<f64>,
}

impl CoreLoad {
    fn sample(probe: &mut dyn LoadProbe, core: usize) -> Result<Self, ProtocolError> {
        let busy_fraction = probe.core_busy_fraction(core)?;
        let loadavg = Some(probe.loadavg()).filter(|value| value.is_finite());
        Ok(CoreLoad {
            busy_fraction,
            loadavg,
        })
    }
}

impl Protocol {
    /// Check the core is idle, pin, warm up, run `measured_runs` times, check
    /// idle again. The calling thread stays pinned to `core` afterwards, so
    /// the jobs of one run all stay on it; a caller that needs another core
    /// pins again.
    pub fn run_job(
        &self,
        core: usize,
        probe: &mut dyn LoadProbe,
        clock: &dyn CpuClock,
        job: &mut dyn FnMut() -> Result<StepReport, EngineError>,
    ) -> Result<JobRecord, ProtocolError> {
        self.run_job_accounted(core, probe, None, clock, MedianClock::Thread, job)
    }

    /// [`Protocol::run_job`] for either accounting: with
    /// [`MedianClock::Process`] (an engine measured as processes) the samples
    /// carry no thread CPU and the median is taken over process CPU.
    ///
    /// `load_before` is the previous job's after-sample of the same core, when
    /// the caller runs jobs back to back: nothing but the harness ran on the
    /// core between the two, so it stands in for a fresh before-sample and
    /// each job costs one sampling interval instead of two.
    pub(crate) fn run_job_accounted(
        &self,
        core: usize,
        probe: &mut dyn LoadProbe,
        load_before: Option<CoreLoad>,
        clock: &dyn CpuClock,
        median_clock: MedianClock,
        job: &mut dyn FnMut() -> Result<StepReport, EngineError>,
    ) -> Result<JobRecord, ProtocolError> {
        self.check_run_count()?;
        let before = match load_before {
            Some(load) => load,
            None => CoreLoad::sample(probe, core)?,
        };
        if before.busy_fraction > self.max_core_busy_fraction {
            return Err(ProtocolError::CoreBusy {
                core,
                busy_fraction: before.busy_fraction,
                loadavg: before.loadavg.unwrap_or(f64::NAN),
                limit: self.max_core_busy_fraction,
            });
        }
        pin_current_thread(core)?;
        for _ in 0..self.warmup_runs {
            job()?;
        }
        let mut samples = Vec::with_capacity(self.measured_runs as usize);
        for _ in 0..self.measured_runs {
            let (thread_before, process_before, wall_before) = (
                clock.thread_cpu_ns(),
                clock.process_cpu_ns(),
                clock.wall_ns(),
            );
            let report = job()?;
            let thread_cpu_ns = clock.thread_cpu_ns().saturating_sub(thread_before);
            let process_cpu_ns = clock.process_cpu_ns().saturating_sub(process_before);
            if let Some(error) = clock.take_read_error() {
                return Err(error);
            }
            samples.push(StepCost {
                thread_cpu_ns: (median_clock == MedianClock::Thread).then_some(thread_cpu_ns),
                process_cpu_ns,
                wall_ns: clock.wall_ns().saturating_sub(wall_before),
                report,
            });
        }
        let after = CoreLoad::sample(probe, core)?;
        let mut marks = Vec::new();
        if after.busy_fraction > self.max_core_busy_fraction {
            marks.push(JobMark::BusyAfter {
                busy_fraction: after.busy_fraction,
            });
        }
        let median = median_sample(&samples, median_clock);
        Ok(JobRecord {
            core,
            busy_before: before.busy_fraction,
            busy_after: after.busy_fraction,
            loadavg: [before.loadavg, after.loadavg],
            marks,
            samples,
            median,
        })
    }

    /// Refuses an even or zero `measured_runs`.
    pub fn check_run_count(&self) -> Result<(), ProtocolError> {
        if self.measured_runs.is_multiple_of(2) {
            return Err(ProtocolError::RunCountNotOdd(self.measured_runs));
        }
        Ok(())
    }
}

/// The clock a job's median is taken on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MedianClock {
    /// The calling thread's CPU (`CpuAccounting::CallingThread`).
    Thread,
    /// The measured processes' CPU (`CpuAccounting::Processes`).
    Process,
}

/// The sample in the middle of an odd number of samples, ordered by the
/// median clock; ties keep run order, so the choice is deterministic.
fn median_sample(samples: &[StepCost], median_clock: MedianClock) -> StepCost {
    let mut order: Vec<&StepCost> = samples.iter().collect();
    order.sort_by_key(|sample| match median_clock {
        MedianClock::Thread => sample.thread_cpu_ns.unwrap_or(0),
        MedianClock::Process => sample.process_cpu_ns,
    });
    order[order.len() / 2].clone()
}

/// The median of an odd number of values: the middle one, never a mean.
pub fn median(values: &[f64]) -> Result<f64, ProtocolError> {
    if values.len().is_multiple_of(2) {
        return Err(ProtocolError::RunCountNotOdd(values.len() as u32));
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Ok(sorted[sorted.len() / 2])
}

/// `|m_a - m_b| / min(m_a, m_b)` of the medians of two arms that ran the same
/// plan.
// TODO(S2, the harness CLI): record the band in the protocol record (a
// `noise_band` field of `Protocol`, written to `run.json`); until then the
// caller reports the value.
pub fn noise_band(arm_a: &[f64], arm_b: &[f64]) -> Result<f64, ProtocolError> {
    let (median_a, median_b) = (median(arm_a)?, median(arm_b)?);
    let smaller = median_a.min(median_b);
    if smaller <= 0.0 || smaller.is_nan() {
        return Err(ProtocolError::NonPositiveMedian { median: smaller });
    }
    Ok((median_a - median_b).abs() / smaller)
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
    let (smaller, larger) = (candidate.min(reference), candidate.max(reference));
    if larger <= (1.0 + band) * smaller {
        Comparison::Tie
    } else if candidate < reference {
        Comparison::Cheaper
    } else {
        Comparison::Costlier
    }
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

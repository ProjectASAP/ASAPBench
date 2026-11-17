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

use crate::metrics::{CpuTimeSampler, ThreadCpuTime};

use serde::{Deserialize, Serialize};

use crate::harness::engine::{CpuAccounting, EngineError, StepCost, StepReport};
use crate::harness::fit::FitPoint;

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

/// The OS clocks: [`ThreadCpuTime`] for the thread,
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

/// Where a `Processes` engine's CPU is read: the `/proc` root and the
/// cgroup v2 mount. Example: on Linux, `/proc` and `/sys/fs/cgroup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCpuFiles {
    pub proc_root: PathBuf,
    pub cgroup_root: PathBuf,
}

impl ProcessCpuFiles {
    /// The machine's `/proc` and `/sys/fs/cgroup`.
    pub fn os() -> Self {
        ProcessCpuFiles {
            proc_root: PathBuf::from("/proc"),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
        }
    }
}

/// The clock of a `Processes` engine (an external server). Thread CPU is
/// always 0 (the harness thread does none of the engine's work), and wall
/// time is the OS monotonic clock. Process CPU is read at µs or ns
/// resolution, never from the 10 ms ticks of `/proc/<pid>/stat`:
///
/// - when every server pid is in one cgroup v2 group of its own (not the
///   harness's, not the root), the group's `cpu.stat` `usage_usec`, which
///   also counts threads and child processes that exited during a step.
///   Example: a server in a container, group `/system.slice/docker-1a2b.scope`,
///   with `usage_usec 2500` reads 2,500,000 ns;
/// - otherwise the sum over the pids' live threads of the first field of
///   `/proc/<pid>/task/<tid>/schedstat` (ns on CPU). A thread that exits
///   during a step takes its CPU with it. Example: threads at 2,000 and
///   3,000 ns read 5,000 ns.
///
/// Every reading first checks that each pid still runs: a missing pid is
/// [`ProtocolError::MissingProcess`], a zombie (state `Z` or `X`)
/// [`ProtocolError::ProcessExited`].
#[derive(Debug)]
pub struct ProcessesCpuClock {
    pids: Vec<u32>,
    proc_root: PathBuf,
    /// The `cpu.stat` of the pids' own cgroup; `None` reads `schedstat`.
    cgroup_cpu_stat: Option<PathBuf>,
    /// The last successful reading, returned after a failed one.
    last_process_cpu_ns: Cell<u64>,
    /// The first failed reading, until [`CpuClock::take_read_error`] takes it.
    read_error: RefCell<Option<ProtocolError>>,
}

impl ProcessesCpuClock {
    /// Picks the source and reads the pids once, so a missing or exited
    /// server fails here, before any job.
    pub fn new(pids: Vec<u32>, files: ProcessCpuFiles) -> Result<Self, ProtocolError> {
        let cgroup_cpu_stat = own_cgroup_cpu_stat(&files, &pids);
        let clock = ProcessesCpuClock {
            pids,
            proc_root: files.proc_root,
            cgroup_cpu_stat,
            last_process_cpu_ns: Cell::new(0),
            read_error: RefCell::new(None),
        };
        clock.last_process_cpu_ns.set(clock.read_cpu_ns()?);
        Ok(clock)
    }

    fn read_cpu_ns(&self) -> Result<u64, ProtocolError> {
        match &self.cgroup_cpu_stat {
            Some(cpu_stat) => {
                for &pid in &self.pids {
                    check_running(&self.proc_root, pid)?;
                }
                cgroup_usage_ns(cpu_stat)
            }
            None => processes_cpu_ns(&self.proc_root, &self.pids),
        }
    }
}

impl CpuClock for ProcessesCpuClock {
    fn thread_cpu_ns(&self) -> u64 {
        0
    }
    fn process_cpu_ns(&self) -> u64 {
        match self.read_cpu_ns() {
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

/// The cgroup v2 path of `<proc_root>/<pid>/cgroup` (its `0::` line), such
/// as `/system.slice/docker-1a2b.scope`; `None` when unreadable or v1 only.
fn cgroup_v2_path(proc_root: &Path, pid: &str) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid).join("cgroup")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_string())
}

/// The `cpu.stat` of the cgroup all of `pids` are in, when that group is
/// their own: one group for every pid, not the root, not the harness's own
/// group (`<proc_root>/self`), and its `cpu.stat` readable.
fn own_cgroup_cpu_stat(files: &ProcessCpuFiles, pids: &[u32]) -> Option<PathBuf> {
    let group = cgroup_v2_path(&files.proc_root, &pids.first()?.to_string())?;
    let shared = pids
        .iter()
        .all(|pid| cgroup_v2_path(&files.proc_root, &pid.to_string()).as_ref() == Some(&group));
    if !shared || group == "/" || cgroup_v2_path(&files.proc_root, "self") == Some(group.clone()) {
        return None;
    }
    let cpu_stat = files
        .cgroup_root
        .join(group.trim_start_matches('/'))
        .join("cpu.stat");
    cgroup_usage_ns(&cpu_stat).ok().map(|_| cpu_stat)
}

/// `usage_usec` of a cgroup v2 `cpu.stat`, in ns.
fn cgroup_usage_ns(cpu_stat: &Path) -> Result<u64, ProtocolError> {
    let parse_error = |message: String| ProtocolError::Parse {
        what: cpu_stat.display().to_string(),
        message,
    };
    let text = std::fs::read_to_string(cpu_stat).map_err(|error| parse_error(error.to_string()))?;
    let usage_usec: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .ok_or_else(|| parse_error("no usage_usec line".into()))?
        .trim()
        .parse()
        .map_err(|_| parse_error("usage_usec is not a number".into()))?;
    Ok(usage_usec * 1_000)
}

/// The state (field 3) of one `/proc/<pid>/stat` line, such as `S` or `Z`.
/// The command name in parentheses may hold spaces and parentheses.
pub fn process_state_from_stat(stat: &str) -> Result<char, ProtocolError> {
    // The command name may hold ") ", so the fields start after the last ')'.
    stat.rfind(')')
        .and_then(|end| stat[end + 1..].split_whitespace().next())
        .and_then(|state| state.chars().next())
        .ok_or_else(|| ProtocolError::Parse {
            what: "/proc/<pid>/stat".into(),
            message: "no state after the command name".into(),
        })
}

/// `Ok` while `pid` runs: its stat file exists and its state is neither
/// zombie (`Z`) nor dead (`X`).
fn check_running(proc_root: &Path, pid: u32) -> Result<(), ProtocolError> {
    let path = proc_root.join(pid.to_string()).join("stat");
    let stat = std::fs::read_to_string(&path).map_err(|_| ProtocolError::MissingProcess {
        pid,
        path: path.display().to_string(),
    })?;
    match process_state_from_stat(&stat)? {
        state @ ('Z' | 'X') => Err(ProtocolError::ProcessExited { pid, state }),
        _ => Ok(()),
    }
}

/// The summed CPU of `pids` in ns: per pid, the first field of every
/// `<proc_root>/<pid>/task/<tid>/schedstat`. A missing pid is
/// [`ProtocolError::MissingProcess`], an exited one
/// [`ProtocolError::ProcessExited`]. A thread that exits between listing
/// and reading is skipped.
pub fn processes_cpu_ns(proc_root: &Path, pids: &[u32]) -> Result<u64, ProtocolError> {
    let mut total_ns = 0;
    for &pid in pids {
        check_running(proc_root, pid)?;
        let tasks = proc_root.join(pid.to_string()).join("task");
        let missing = || ProtocolError::MissingProcess {
            pid,
            path: tasks.display().to_string(),
        };
        for task in std::fs::read_dir(&tasks).map_err(|_| missing())? {
            let schedstat = task.map_err(|_| missing())?.path().join("schedstat");
            let Ok(text) = std::fs::read_to_string(&schedstat) else {
                continue;
            };
            total_ns += text
                .split_whitespace()
                .next()
                .and_then(|ns| ns.parse::<u64>().ok())
                .ok_or_else(|| ProtocolError::Parse {
                    what: schedstat.display().to_string(),
                    message: "the first field is not a number of ns".into(),
                })?;
        }
    }
    Ok(total_ns)
}

/// What the harness reads CPU from, per engine accounting: the caller's
/// clock for a `CallingThread` engine ([`OsCpuClock`], or a fake clock in
/// tests), and a [`ProcessesCpuClock`] it builds itself over
/// `process_cpu_files` for a `Processes` engine, so an external server is
/// never timed on the harness process.
pub struct MeasurementClocks<'a> {
    pub calling_thread: &'a dyn CpuClock,
    pub process_cpu_files: ProcessCpuFiles,
}

impl<'a> MeasurementClocks<'a> {
    /// `calling_thread`, and the machine's `/proc` and cgroup mount.
    pub fn with_calling_thread(calling_thread: &'a dyn CpuClock) -> Self {
        MeasurementClocks {
            calling_thread,
            process_cpu_files: ProcessCpuFiles::os(),
        }
    }

    /// The clock an engine with `accounting` is measured on. For
    /// `Processes(pids)` this reads every pid once and fails on a missing or
    /// exited one.
    pub fn clock_for(&self, accounting: &CpuAccounting) -> Result<EngineClock<'a>, ProtocolError> {
        Ok(match accounting {
            CpuAccounting::CallingThread => EngineClock::CallingThread(self.calling_thread),
            CpuAccounting::Processes(pids) => EngineClock::Processes(ProcessesCpuClock::new(
                pids.clone(),
                self.process_cpu_files.clone(),
            )?),
        })
    }
}

/// The clock one engine is measured on, and so the CPU its medians are
/// taken over: the calling thread's for `CallingThread`, the server
/// processes' for `Processes`.
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

    /// The CPU of `cost` this clock measures the engine by: thread CPU for
    /// `CallingThread`, process CPU for `Processes`.
    pub fn engine_cpu_ns(&self, cost: &StepCost) -> u64 {
        match self {
            EngineClock::CallingThread(_) => cost.thread_cpu_ns.unwrap_or(0),
            EngineClock::Processes(_) => cost.process_cpu_ns,
        }
    }
}

/// The cores this process may run on, read once from the calling thread's
/// affinity before this module pins any thread. A pinned thread (the main
/// thread after a job on it) narrows its own mask, so a later read would
/// refuse every other core.
#[cfg(target_os = "linux")]
fn allowed_cores() -> Result<&'static [usize], String> {
    static ALLOWED: OnceLock<Result<Vec<usize>, String>> = OnceLock::new();
    ALLOWED
        .get_or_init(|| {
            let set_bytes = std::mem::size_of::<libc::cpu_set_t>();
            // SAFETY: `allowed` is a plain bit mask owned by this frame;
            // `sched_getaffinity` writes at most `set_bytes` into it and the
            // return value is checked; `CPU_ISSET` reads one bit below the
            // set size.
            unsafe {
                let mut allowed: libc::cpu_set_t = std::mem::zeroed();
                if libc::sched_getaffinity(0, set_bytes, &mut allowed) != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                Ok((0..8 * set_bytes)
                    .filter(|&core| libc::CPU_ISSET(core, &allowed))
                    .collect())
            }
        })
        .as_deref()
        .map_err(Clone::clone)
}

/// Refuses a core outside the allowed set (one that does not exist, or one
/// `taskset` excluded), naming the set, for example `0-55`.
#[cfg(target_os = "linux")]
fn check_core_allowed(core: usize) -> Result<(), ProtocolError> {
    let allowed = allowed_cores().map_err(|message| ProtocolError::Pin { core, message })?;
    if allowed.binary_search(&core).is_err() {
        return Err(ProtocolError::CoreNotAllowed {
            core,
            allowed: core_ranges(allowed),
        });
    }
    Ok(())
}

/// Pinning needs Linux's `sched_setaffinity`; elsewhere it is refused.
#[cfg(not(target_os = "linux"))]
fn check_core_allowed(core: usize) -> Result<(), ProtocolError> {
    Err(ProtocolError::Pin {
        core,
        message: "thread pinning is supported on Linux only".into(),
    })
}

/// Pins the calling thread to `core` with `sched_setaffinity`. A core outside
/// the process's allowed set is refused, naming the set (for example `0-55`).
#[cfg(target_os = "linux")]
pub fn pin_current_thread(core: usize) -> Result<(), ProtocolError> {
    check_core_allowed(core)?;
    let set_bytes = std::mem::size_of::<libc::cpu_set_t>();
    // SAFETY: `set` is a plain bit mask owned by this frame and `core` is
    // below its size (it is in the allowed set); `sched_setaffinity(0, ..)`
    // changes only the calling thread.
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
    check_core_allowed(core)
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
    /// Check the core exists, check it is idle, pin, warm up, run
    /// `measured_runs` times, check idle again. The calling thread stays
    /// pinned to `core` afterwards, so the jobs of one run all stay on it; a
    /// caller that needs another core pins again.
    ///
    /// Each run is timed on `clock`, and the median is the sample in the
    /// middle by the CPU the clock measures the engine by
    /// ([`EngineClock::engine_cpu_ns`]): thread CPU for `CallingThread`,
    /// process CPU for `Processes`, whose samples carry no thread CPU.
    pub fn run_job(
        &self,
        core: usize,
        probe: &mut dyn LoadProbe,
        clock: &EngineClock,
        job: &mut dyn FnMut() -> Result<StepReport, EngineError>,
    ) -> Result<JobRecord, ProtocolError> {
        self.run_next_job(core, probe, None, clock, job)
    }

    /// [`Protocol::run_job`] after another job on the same core.
    /// `load_before` is the previous job's after-sample: nothing but the
    /// harness ran on the core between the two, so it stands in for a fresh
    /// before-sample and each job costs one sampling interval instead of two.
    pub(crate) fn run_next_job(
        &self,
        core: usize,
        probe: &mut dyn LoadProbe,
        load_before: Option<CoreLoad>,
        clock: &EngineClock,
        job: &mut dyn FnMut() -> Result<StepReport, EngineError>,
    ) -> Result<JobRecord, ProtocolError> {
        self.check_run_count()?;
        check_core_allowed(core)?;
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
        let records_thread_cpu = matches!(clock, EngineClock::CallingThread(_));
        let cpu = clock.as_clock();
        let mut samples = Vec::with_capacity(self.measured_runs as usize);
        for _ in 0..self.measured_runs {
            // The wall clock is read last before the call and first after
            // it, so it does not include the CPU clocks' own reads (file
            // reads for a `Processes` engine).
            let thread_before = cpu.thread_cpu_ns();
            let process_before = cpu.process_cpu_ns();
            let wall_before = cpu.wall_ns();
            let report = job()?;
            let wall_ns = cpu.wall_ns().saturating_sub(wall_before);
            let thread_cpu_ns = cpu.thread_cpu_ns().saturating_sub(thread_before);
            let process_cpu_ns = cpu.process_cpu_ns().saturating_sub(process_before);
            if let Some(error) = cpu.take_read_error() {
                return Err(error);
            }
            samples.push(StepCost {
                thread_cpu_ns: records_thread_cpu.then_some(thread_cpu_ns),
                process_cpu_ns,
                wall_ns,
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
        let median = median_sample(&samples, clock);
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

/// The sample in the middle of an odd number of samples, ordered by the CPU
/// `clock` measures the engine by; ties keep run order, so the choice is
/// deterministic.
fn median_sample(samples: &[StepCost], clock: &EngineClock) -> StepCost {
    let mut order: Vec<&StepCost> = samples.iter().collect();
    order.sort_by_key(|sample| clock.engine_cpu_ns(sample));
    order[order.len() / 2].clone()
}

/// Times `run(size)` at each size as one protocol job pinned to `core`
/// ([`Protocol::run_job`]: idle check, warm-up, the median of
/// `measured_runs` times on `clock`). `run` returns the units it processed;
/// the point takes the units of the median run. One point per size:
/// setting = the size, terms = `[units, 1]`, measured = the median's engine
/// CPU ([`EngineClock::engine_cpu_ns`]). Consecutive sizes share an idle
/// sample, as the steps of [`crate::harness::arm_run::run_arm`] do.
///
/// An engine's calibration plugs in as
/// `|size| calibratable.prepare_cost_target(&target, size)?()`.
pub fn measure_sizes(
    protocol: &Protocol,
    core: usize,
    probe: &mut dyn LoadProbe,
    clock: &EngineClock,
    sizes: &[u64],
    run: &mut dyn FnMut(u64) -> Result<u64, EngineError>,
) -> Result<Vec<FitPoint>, ProtocolError> {
    let mut load: Option<CoreLoad> = None;
    let mut points = Vec::with_capacity(sizes.len());
    for &size in sizes {
        // Units of every call, warm-up first; the measured ones are the last
        // `measured_runs`.
        let mut units_per_call = Vec::new();
        let job = protocol.run_next_job(core, probe, load, clock, &mut || {
            units_per_call.push(run(size)?);
            Ok(StepReport::default())
        })?;
        load = Some(job.load_after());
        let measured_units = &units_per_call[protocol.warmup_runs as usize..];
        let median_run = job
            .samples
            .iter()
            .position(|sample| *sample == job.median)
            .expect("the median is one of the samples");
        points.push(FitPoint {
            setting: size.to_string(),
            terms: vec![measured_units[median_run] as f64, 1.0],
            measured: clock.engine_cpu_ns(&job.median) as f64,
        });
    }
    Ok(points)
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
// TODO: record the band in the protocol record (a `noise_band` field of
// `Protocol`, written to `run.json`) once the harness CLI writes it; until
// then the caller reports the value.
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
    /// `/proc/stat`, `/proc/<pid>/stat`, `schedstat` or `cpu.stat` text that
    /// does not parse.
    #[error("cannot parse {what}: {message}")]
    Parse { what: String, message: String },
    /// A process whose stat file is missing.
    #[error("process {pid} has no {path}")]
    MissingProcess { pid: u32, path: String },
    /// A process that exited but is not yet reaped (state `Z`) or is dead
    /// (`X`): it runs no more, so its CPU no longer grows.
    #[error("process {pid} has exited (state {state})")]
    ProcessExited { pid: u32, state: char },
    /// Pinning failed in the OS.
    #[error("pinning to core {core} failed: {message}")]
    Pin { core: usize, message: String },
    /// The job's engine call failed.
    #[error("the job failed: {0}")]
    Job(#[from] EngineError),
}

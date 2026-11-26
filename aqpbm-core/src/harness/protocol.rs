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

use crate::harness::cgroup::{CgroupCpu, CgroupError, CgroupSource, PROC_STAT};
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
    /// Where the CPU is read: `cgroup <cpu.stat path>` or `schedstat`.
    pub fn cpu_source(&self) -> String {
        match &self.cgroup_cpu_stat {
            Some(cpu_stat) => format!("cgroup {}", cpu_stat.display()),
            None => "schedstat".into(),
        }
    }

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

/// The clock of a [`CpuAccounting::Cgroup`] engine: an external server in
/// its own cgroup v2 group, read locally or over SSH on the server host. Process CPU is the group's `cpu.stat` `usage_usec` (µs resolution,
/// every thread and child of the group); thread CPU is 0; wall time is the
/// OS monotonic clock. A reading whose `nr_throttled` is above the one at
/// creation is [`ProtocolError::CgroupThrottled`]: a CPU quota stopped the
/// server, so the step's CPU is not the work's.
#[derive(Debug)]
pub struct CgroupCpuClock {
    source: CgroupSource,
    nr_throttled_at_start: u64,
    last_process_cpu_ns: Cell<u64>,
    read_error: RefCell<Option<ProtocolError>>,
}

impl CgroupCpuClock {
    /// Reads `cpu.stat` once, so an unreachable group fails before any job.
    pub fn new(source: CgroupSource) -> Result<Self, ProtocolError> {
        let cpu = source.cpu()?;
        Ok(CgroupCpuClock {
            source,
            nr_throttled_at_start: cpu.nr_throttled,
            last_process_cpu_ns: Cell::new(cpu.usage_ns),
            read_error: RefCell::new(None),
        })
    }

    fn read_cpu_ns(&self) -> Result<u64, ProtocolError> {
        let cpu = self.source.cpu()?;
        if cpu.nr_throttled > self.nr_throttled_at_start {
            return Err(ProtocolError::CgroupThrottled {
                nr_throttled: cpu.nr_throttled - self.nr_throttled_at_start,
            });
        }
        Ok(cpu.usage_ns)
    }
}

impl CpuClock for CgroupCpuClock {
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
/// group (`<proc_root>/self`), every process in its `cgroup.procs` one of
/// `pids` or a descendant of one, and its `cpu.stat` readable. Otherwise the
/// group's CPU would count a foreign process, so the clock reads `schedstat`.
/// Example: a container with the server (pid 40) and its child (pid 41,
/// parent 40) is the server's own; with an unrelated pid 99 (parent 1) it
/// is not.
fn own_cgroup_cpu_stat(files: &ProcessCpuFiles, pids: &[u32]) -> Option<PathBuf> {
    let group = cgroup_v2_path(&files.proc_root, &pids.first()?.to_string())?;
    let shared = pids
        .iter()
        .all(|pid| cgroup_v2_path(&files.proc_root, &pid.to_string()).as_ref() == Some(&group));
    if !shared || group == "/" || cgroup_v2_path(&files.proc_root, "self") == Some(group.clone()) {
        return None;
    }
    let group_dir = files.cgroup_root.join(group.trim_start_matches('/'));
    if !group_holds_only(&group_dir, &files.proc_root, pids)? {
        return None;
    }
    let cpu_stat = group_dir.join("cpu.stat");
    cgroup_usage_ns(&cpu_stat).ok().map(|_| cpu_stat)
}

/// Whether every process in `group_dir` and its child groups (whose CPU
/// the group's `cpu.stat` also counts) is one of `pids` or a descendant;
/// `None` when a `cgroup.procs` cannot be read.
fn group_holds_only(group_dir: &Path, proc_root: &Path, pids: &[u32]) -> Option<bool> {
    let procs = std::fs::read_to_string(group_dir.join("cgroup.procs")).ok()?;
    for member in procs.split_whitespace() {
        let member: u32 = member.parse().ok()?;
        if !descends_from(proc_root, member, pids) {
            return Some(false);
        }
    }
    for entry in std::fs::read_dir(group_dir).ok()? {
        let child = entry.ok()?.path();
        if child.is_dir() && !group_holds_only(&child, proc_root, pids)? {
            return Some(false);
        }
    }
    Some(true)
}

/// Whether `pid` is one of `ancestors` or a descendant of one, by its parent
/// pids (field 4 of `<proc_root>/<pid>/stat`). An unreadable stat (a process
/// that just exited) is not a descendant.
fn descends_from(proc_root: &Path, pid: u32, ancestors: &[u32]) -> bool {
    let mut current = pid;
    // A pid chain is at most as long as the pid space; 4096 steps is far
    // above any real process tree and stops a malformed (cyclic) fake.
    for _ in 0..4096 {
        if ancestors.contains(&current) {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(proc_root.join(current.to_string()).join("stat"))
        else {
            return false;
        };
        let parent = stat
            .rfind(')')
            .and_then(|end| stat[end + 1..].split_whitespace().nth(1))
            .and_then(|ppid| ppid.parse::<u32>().ok());
        match parent {
            Some(parent) if parent > 0 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// `usage_usec` of a cgroup v2 `cpu.stat`, in ns ([`CgroupCpu::parse`]).
fn cgroup_usage_ns(cpu_stat: &Path) -> Result<u64, ProtocolError> {
    let text = std::fs::read_to_string(cpu_stat).map_err(|error| ProtocolError::Parse {
        what: cpu_stat.display().to_string(),
        message: error.to_string(),
    })?;
    Ok(CgroupCpu::parse(&text)?.usage_ns)
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessesMemory {
    pub proportional_set_size_bytes: u64,
    pub peak_resident_set_size_bytes: Option<u64>,
}

pub fn processes_memory(proc_root: &Path, pids: &[u32]) -> Result<ProcessesMemory, ProtocolError> {
    let mut memory = ProcessesMemory::default();
    for &pid in pids {
        check_running(proc_root, pid)?;
        let process = proc_root.join(pid.to_string());
        memory.proportional_set_size_bytes +=
            proc_file_bytes(pid, &process.join("smaps_rollup"), "Pss:")?;
        if pids.len() == 1 {
            memory.peak_resident_set_size_bytes =
                Some(proc_file_bytes(pid, &process.join("status"), "VmHWM:")?);
        }
    }
    Ok(memory)
}

fn proc_file_bytes(pid: u32, path: &Path, field: &str) -> Result<u64, ProtocolError> {
    let text = std::fs::read_to_string(path).map_err(|_| ProtocolError::MissingProcess {
        pid,
        path: path.display().to_string(),
    })?;
    text.lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|value| value.trim().strip_suffix("kB"))
        .and_then(|kilobytes| kilobytes.trim().parse::<u64>().ok())
        .map(|kilobytes| kilobytes * 1024)
        .ok_or_else(|| ProtocolError::Parse {
            what: path.display().to_string(),
            message: format!("no {field} line in kB"),
        })
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
            CpuAccounting::Cgroup(source) => {
                EngineClock::Cgroup(CgroupCpuClock::new(source.clone())?)
            }
        })
    }
}

/// The clock one engine is measured on, and so the CPU its medians are
/// taken over: the calling thread's for `CallingThread`, the server
/// processes' for `Processes`.
pub enum EngineClock<'a> {
    CallingThread(&'a dyn CpuClock),
    Processes(ProcessesCpuClock),
    Cgroup(CgroupCpuClock),
}

impl EngineClock<'_> {
    pub fn as_clock(&self) -> &dyn CpuClock {
        match self {
            EngineClock::CallingThread(clock) => *clock,
            EngineClock::Processes(clock) => clock,
            EngineClock::Cgroup(clock) => clock,
        }
    }

    /// Where this clock reads CPU, recorded in every arm result:
    /// `calling-thread`, `cgroup <cpu.stat path>`, `schedstat` or
    /// `cgroup <host>:<cpu.stat path> over SSH`.
    pub fn cpu_source(&self) -> String {
        match self {
            EngineClock::CallingThread(_) => "calling-thread".into(),
            EngineClock::Processes(clock) => clock.cpu_source(),
            EngineClock::Cgroup(clock) => match &clock.source {
                CgroupSource::Local(dir) => format!("cgroup {}", dir.join("cpu.stat").display()),
                CgroupSource::Remote {
                    host,
                    cgroup_directory,
                    ..
                } => format!(
                    "cgroup {host}:{} over SSH",
                    cgroup_directory.join("cpu.stat").display()
                ),
            },
        }
    }

    /// The CPU of `cost` this clock measures the engine by: thread CPU for
    /// `CallingThread`, process CPU for `Processes`.
    pub fn engine_cpu_ns(&self, cost: &StepCost) -> u64 {
        match self {
            EngineClock::CallingThread(_) => cost.thread_cpu_ns.unwrap_or(0),
            EngineClock::Processes(_) | EngineClock::Cgroup(_) => cost.process_cpu_ns,
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

/// The cores of a cpu list such as `2,22` or `0-3,8` (cgroup
/// `cpuset.cpus.effective`, `/proc/<pid>/status` `Cpus_allowed_list`).
pub fn parse_cpu_list(text: &str) -> Result<Vec<usize>, ProtocolError> {
    let parse_error = || ProtocolError::Parse {
        what: "cpu list".into(),
        message: format!("'{}' is not a list like 2,22 or 0-3,8", text.trim()),
    };
    let mut cores = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let first: usize = first.parse().map_err(|_| parse_error())?;
        let last: usize = last.parse().map_err(|_| parse_error())?;
        if last < first {
            return Err(parse_error());
        }
        cores.extend(first..=last);
    }
    if cores.is_empty() {
        return Err(parse_error());
    }
    Ok(cores)
}

/// The busy fraction of `cores` between two `/proc/stat` texts that is not
/// the server's own work: busy ticks over the cores, minus the server's CPU
/// over the same interval (`server_cpu_ns`), over all ticks of the cores.
/// A server pinned to these cores runs only on them, so its own merges and
/// flushes never count as foreign load.
/// `/proc/stat` counts whole ticks while the server's CPU is read in µs, and
/// the two are read by separate requests, so up to two ticks per core of
/// the busy time are rounding, not load: they are forgiven.
/// Example: cores 2 and 22 over 10 ticks each (`tick_ns` 10 ms, 200 ms in
/// all), 10 busy ticks (100 ms) of which the server used 40 ms: 60 ms, less
/// 40 ms of rounding allowance, is 20 ms of 200 ms, 0.1.
pub fn foreign_busy_fraction(
    cores: &[usize],
    before: &str,
    after: &str,
    server_cpu_ns: u64,
    tick_ns: u64,
) -> Result<f64, ProtocolError> {
    let (mut busy_ticks, mut total_ticks) = (0_u64, 0_u64);
    for &core in cores {
        let before = core_ticks(core, before)?;
        let after = core_ticks(core, after)?;
        let delta = |field: usize| after[field].saturating_sub(before[field]);
        let total: u64 = (0..8).map(delta).sum();
        total_ticks += total;
        busy_ticks += total - (delta(3) + delta(4));
    }
    if total_ticks == 0 {
        return Err(ProtocolError::IdleUnmeasurable {
            core: cores.first().copied().unwrap_or(0),
        });
    }
    let rounding_allowance_ns = 2 * tick_ns * cores.len() as u64;
    let foreign_ns = (busy_ticks * tick_ns)
        .saturating_sub(server_cpu_ns)
        .saturating_sub(rounding_allowance_ns);
    Ok(foreign_ns as f64 / (total_ticks * tick_ns) as f64)
}

/// The length of one `/proc/stat` tick, in ns (`USER_HZ`, 100 per second on
/// Linux).
fn proc_stat_tick_ns() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf` reads a constant and takes no pointers.
        let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if per_second > 0 {
            return 1_000_000_000 / per_second as u64;
        }
    }
    10_000_000
}

/// Where a [`ServerCoresProbe`] reads `/proc/stat`, the load average and
/// the server's own CPU.
#[derive(Debug, Clone)]
enum ServerHost {
    /// This host: its proc root, and the server's pids (their threads'
    /// `schedstat`).
    Local { proc_root: PathBuf, pids: Vec<u32> },
    /// The server host's cgroup source: its `/proc/stat` and the group's
    /// `cpu.stat`.
    Cgroup(CgroupSource),
}

/// The idle check of an external server: over one sampling interval, the
/// busy fraction of the cores the server may run on (its container's
/// cpuset) that is not the server's own work ([`foreign_busy_fraction`]),
/// not the harness's core. The `core` the protocol passes is the harness's
/// and is ignored; a `CoreBusy` error names it.
#[derive(Debug, Clone)]
pub struct ServerCoresProbe {
    pub cores: Vec<usize>,
    pub sample_interval: std::time::Duration,
    host: ServerHost,
}

impl ServerCoresProbe {
    /// `/proc/stat` and the server's CPU in ns, read together.
    fn read(&self) -> Result<(String, u64), ProtocolError> {
        match &self.host {
            ServerHost::Local { proc_root, pids } => {
                let path = proc_root.join("stat");
                let stat =
                    std::fs::read_to_string(&path).map_err(|error| ProtocolError::Parse {
                        what: path.display().to_string(),
                        message: error.to_string(),
                    })?;
                Ok((stat, processes_cpu_ns(proc_root, pids)?))
            }
            ServerHost::Cgroup(source) => Ok((source.read(PROC_STAT)?, source.cpu()?.usage_ns)),
        }
    }
}

impl LoadProbe for ServerCoresProbe {
    fn core_busy_fraction(&mut self, _harness_core: usize) -> Result<f64, ProtocolError> {
        let (before, server_before_ns) = self.read()?;
        std::thread::sleep(self.sample_interval);
        let (after, server_after_ns) = self.read()?;
        foreign_busy_fraction(
            &self.cores,
            &before,
            &after,
            server_after_ns.saturating_sub(server_before_ns),
            proc_stat_tick_ns(),
        )
    }

    /// The server host's load average on this host; `NaN` (recorded `None`)
    /// for a cgroup source, which does not read it.
    fn loadavg(&mut self) -> f64 {
        match &self.host {
            ServerHost::Local { proc_root, .. } => {
                std::fs::read_to_string(proc_root.join("loadavg"))
                    .ok()
                    .and_then(|text| text.split_whitespace().next()?.parse().ok())
                    .unwrap_or(f64::NAN)
            }
            ServerHost::Cgroup(_) => f64::NAN,
        }
    }
}

/// The idle probe for an engine with `accounting`: the harness core
/// ([`OsLoadProbe`]) for `CallingThread`; the server's cores for
/// `Processes` (the first pid's `Cpus_allowed_list` under
/// `files.proc_root`) and `Cgroup` (the group's `cpuset.cpus.effective`,
/// `/proc/stat` through the source).
pub fn load_probe_for(
    accounting: &CpuAccounting,
    files: &ProcessCpuFiles,
) -> Result<Box<dyn LoadProbe>, ProtocolError> {
    let sample_interval = OsLoadProbe::default().sample_interval;
    Ok(match accounting {
        CpuAccounting::CallingThread => Box::new(OsLoadProbe::default()),
        CpuAccounting::Processes(pids) => {
            let pid = pids.first().ok_or_else(|| ProtocolError::Parse {
                what: "Processes accounting".into(),
                message: "no pids".into(),
            })?;
            let path = files.proc_root.join(pid.to_string()).join("status");
            let status =
                std::fs::read_to_string(&path).map_err(|_| ProtocolError::MissingProcess {
                    pid: *pid,
                    path: path.display().to_string(),
                })?;
            let list = status
                .lines()
                .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
                .ok_or_else(|| ProtocolError::Parse {
                    what: path.display().to_string(),
                    message: "no Cpus_allowed_list line".into(),
                })?;
            Box::new(ServerCoresProbe {
                cores: parse_cpu_list(list)?,
                sample_interval,
                host: ServerHost::Local {
                    proc_root: files.proc_root.clone(),
                    pids: pids.clone(),
                },
            })
        }
        CpuAccounting::Cgroup(source) => Box::new(ServerCoresProbe {
            cores: parse_cpu_list(&source.read("cpuset.cpus.effective")?)?,
            sample_interval,
            host: ServerHost::Cgroup(source.clone()),
        }),
    })
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
    /// The index of `median` in `samples`: the measured run it came from.
    pub median_run: usize,
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
        let median_run = median_run(&samples, clock);
        let median = samples[median_run].clone();
        Ok(JobRecord {
            core,
            busy_before: before.busy_fraction,
            busy_after: after.busy_fraction,
            loadavg: [before.loadavg, after.loadavg],
            marks,
            samples,
            median,
            median_run,
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
fn median_run(samples: &[StepCost], clock: &EngineClock) -> usize {
    let mut order: Vec<usize> = (0..samples.len()).collect();
    order.sort_by_key(|&run| clock.engine_cpu_ns(&samples[run]));
    order[order.len() / 2]
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
        points.push(FitPoint {
            setting: size.to_string(),
            terms: vec![measured_units[job.median_run] as f64, 1.0],
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
    /// A server's cgroup file that cannot be read.
    #[error(transparent)]
    Cgroup(#[from] CgroupError),
    /// The server's cgroup was throttled by a CPU quota during the run
    /// (`nr_throttled` grew by this many periods); start it without `--cpus`.
    #[error("the server's cgroup was throttled {nr_throttled} times by a CPU quota: start it without --cpus")]
    CgroupThrottled { nr_throttled: u64 },
    /// The job's engine call failed.
    #[error("the job failed: {0}")]
    Job(#[from] EngineError),
}

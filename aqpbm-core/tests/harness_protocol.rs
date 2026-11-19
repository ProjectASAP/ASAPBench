//! The measurement protocol: pinning, idle check, warm-up, median of an odd
//! number of runs, the noise band, and the CPU clocks.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use aqpbm_core::harness::engine::{CpuAccounting, EngineError, StepReport};
use aqpbm_core::harness::fake::FakeClock;
use aqpbm_core::harness::protocol::{
    compare, core_busy_fraction, median, noise_band, pin_current_thread, process_state_from_stat,
    processes_cpu_ns, Comparison, CpuClock, EngineClock, JobMark, LoadProbe, MeasurementClocks,
    OsCpuClock, ProcessCpuFiles, ProcessesCpuClock, Protocol, ProtocolError,
};
use aqpbm_core::harness::run_directory::RunDirectory;
use chrono::{TimeZone, Utc};

/// A load probe that returns `busy` fractions in order (the last one
/// repeats) and a fixed load average.
struct FakeProbe {
    busy: VecDeque<f64>,
    last: f64,
    loadavg: f64,
}

impl FakeProbe {
    fn new(busy: &[f64], loadavg: f64) -> Self {
        FakeProbe {
            busy: busy.iter().copied().collect(),
            last: *busy.last().unwrap(),
            loadavg,
        }
    }
}

impl LoadProbe for FakeProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        Ok(self.busy.pop_front().unwrap_or(self.last))
    }
    fn loadavg(&mut self) -> f64 {
        self.loadavg
    }
}

/// A fresh directory under the system temp dir, unique per process and test,
/// removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aqpbm-core-harness-protocol-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A job that charges `ns` to `clock` per call and counts its calls.
fn counting_job(
    clock: &FakeClock,
    ns: u64,
    calls: &Rc<Cell<u32>>,
) -> impl FnMut() -> Result<StepReport, EngineError> {
    let clock = clock.clone();
    let calls = Rc::clone(calls);
    move || {
        calls.set(calls.get() + 1);
        clock.charge_thread(ns);
        Ok(StepReport::default())
    }
}

/// After `pin_current_thread(0)` the thread runs on core 0, and a job run
/// through the protocol on core 0 runs there and records `core == 0`.
#[cfg(target_os = "linux")]
#[test]
fn pinning_runs_the_job_on_the_requested_core() {
    pin_current_thread(0).unwrap();
    // SAFETY: `sched_getcpu` has no preconditions.
    assert_eq!(unsafe { libc::sched_getcpu() }, 0);

    let clock = FakeClock::default();
    let mut probe = FakeProbe::new(&[0.0], 0.0);
    let ran_on = Rc::new(Cell::new(-1));
    let job_clock = clock.clone();
    let job_ran_on = Rc::clone(&ran_on);
    let mut job = move || {
        // SAFETY: as above.
        job_ran_on.set(unsafe { libc::sched_getcpu() });
        job_clock.charge_thread(10);
        Ok(StepReport::default())
    };
    let record = Protocol::default()
        .run_job(0, &mut probe, &EngineClock::CallingThread(&clock), &mut job)
        .unwrap();
    assert_eq!(record.core, 0);
    assert_eq!(ran_on.get(), 0);
}

/// Core 100000 is outside any allowed set: refused as `CoreNotAllowed`, and
/// the message names core 100000 and the allowed set (for example `0-55`).
#[cfg(target_os = "linux")]
#[test]
fn pinning_outside_the_allowed_set_is_refused_by_name() {
    let error = pin_current_thread(100_000).unwrap_err();
    let message = error.to_string();
    match error {
        ProtocolError::CoreNotAllowed { core, allowed } => {
            assert_eq!(core, 100_000);
            assert!(!allowed.is_empty());
            assert!(message.contains(&allowed), "{message}");
        }
        other => panic!("expected CoreNotAllowed, got {other:?}"),
    }
    assert!(message.contains("100000"), "{message}");
}

/// `cpu3` from `100 0 50 800 50 ...` to `130 0 60 860 50 ...`: Δuser 30 +
/// Δsystem 10 busy, Δidle 60, Δiowait 0, so busy = 40 / 100 = 0.40. The
/// `cpu` and `cpu30` lines, which would give other values, are not read.
#[test]
fn core_busy_fraction_from_two_proc_stat_samples() {
    let before = "cpu  1000 0 500 8000 500 0 0 0 0 0\n\
                  cpu30 0 0 0 1000 0 0 0 0 0 0\n\
                  cpu3 100 0 50 800 50 0 0 0 0 0\n\
                  intr 12345\n";
    let after = "cpu  1100 0 600 8100 500 0 0 0 0 0\n\
                 cpu30 100 0 0 1000 0 0 0 0 0 0\n\
                 cpu3 130 0 60 860 50 0 0 0 0 0\n\
                 intr 12399\n";
    let busy = core_busy_fraction(3, before, after).unwrap();
    assert!((busy - 0.40).abs() < 1e-12, "{busy}");
}

/// A core 40% busy (load average 1.5) against a 10% limit refuses the job
/// before it runs: `CoreBusy { core: 3, busy_fraction: 0.40, loadavg: 1.5 }`,
/// a message naming the core, `40%`, `10%` and `taskset`, and zero calls.
#[test]
fn idle_check_refuses_a_busy_core_and_never_runs_the_job() {
    let clock = FakeClock::default();
    let mut probe = FakeProbe::new(&[0.40], 1.5);
    let calls = Rc::new(Cell::new(0));
    let mut job = counting_job(&clock, 10, &calls);
    let protocol = Protocol {
        max_core_busy_fraction: 0.10,
        ..Protocol::default()
    };
    let error = protocol
        .run_job(3, &mut probe, &EngineClock::CallingThread(&clock), &mut job)
        .unwrap_err();
    let message = error.to_string();
    match error {
        ProtocolError::CoreBusy {
            core,
            busy_fraction,
            loadavg,
            ..
        } => {
            assert_eq!(core, 3);
            assert_eq!(busy_fraction, 0.40);
            assert_eq!(loadavg, 1.5);
        }
        other => panic!("expected CoreBusy, got {other:?}"),
    }
    assert!(message.contains("core 3"), "{message}");
    assert!(message.contains("40%"), "{message}");
    assert!(message.contains("10%"), "{message}");
    assert!(message.contains("taskset"), "{message}");
    assert_eq!(calls.get(), 0);
}

/// A core idle before (0.02) and 0.35 busy after the job keeps the result
/// but marks it `BusyAfter { 0.35 }`, and its `status.log` line says
/// `busy-after 0.35`.
#[test]
fn a_core_that_turns_busy_during_the_job_marks_it() {
    let clock = FakeClock::default();
    let mut probe = FakeProbe::new(&[0.02, 0.35], 0.5);
    let calls = Rc::new(Cell::new(0));
    let mut job = counting_job(&clock, 10, &calls);
    let record = Protocol::default()
        .run_job(0, &mut probe, &EngineClock::CallingThread(&clock), &mut job)
        .unwrap();
    assert_eq!(
        record.marks,
        vec![JobMark::BusyAfter {
            busy_fraction: 0.35
        }]
    );
    assert_eq!(record.busy_before, 0.02);
    assert_eq!(record.busy_after, 0.35);

    let tmp = TempDir::new("busy-after");
    let utc = Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap();
    let run_directory =
        RunDirectory::create(&tmp.0, "test-machine", utc, "test", "busy-after").unwrap();
    run_directory.append_status("ingest", &record).unwrap();
    let status = std::fs::read_to_string(run_directory.path().join("status.log")).unwrap();
    assert!(status.contains("busy-after 0.35"), "{status}");
}

/// Two identical `/proc/stat` texts have no ticks between them: idleness is
/// unmeasured (`IdleUnmeasurable`), not busy 0.
#[test]
fn an_unchanged_proc_stat_is_unmeasured_not_idle() {
    let stat = "cpu  1000 0 500 8000 500 0 0 0 0 0\n\
                cpu3 100 0 50 800 50 0 0 0 0 0\n";
    assert_eq!(
        core_busy_fraction(3, stat, stat),
        Err(ProtocolError::IdleUnmeasurable { core: 3 })
    );
}

/// With warm-up 1 and 5 runs, calls costing 100, 50, 70, 60, 90 and 80 ns
/// run 6 times; the samples are 50, 70, 60, 90, 80 (the 100 ns warm-up
/// dropped) and the median is 70.
#[test]
fn warm_up_runs_are_discarded() {
    let clock = FakeClock::default();
    let mut probe = FakeProbe::new(&[0.0], 0.0);
    let costs = [100_u64, 50, 70, 60, 90, 80];
    let calls = Rc::new(Cell::new(0_usize));
    let job_clock = clock.clone();
    let job_calls = Rc::clone(&calls);
    let mut job = move || {
        let call = job_calls.get();
        job_calls.set(call + 1);
        job_clock.charge_thread(costs[call]);
        Ok(StepReport::default())
    };
    let protocol = Protocol {
        warmup_runs: 1,
        measured_runs: 5,
        ..Protocol::default()
    };
    let record = protocol
        .run_job(0, &mut probe, &EngineClock::CallingThread(&clock), &mut job)
        .unwrap();
    assert_eq!(calls.get(), 6);
    let samples: Vec<Option<u64>> = record.samples.iter().map(|s| s.thread_cpu_ns).collect();
    assert_eq!(
        samples,
        vec![Some(50), Some(70), Some(60), Some(90), Some(80)]
    );
    assert_eq!(record.median.thread_cpu_ns, Some(70));
}

/// `measured_runs` 4 and 0 are refused as `RunCountNotOdd(4)` and
/// `RunCountNotOdd(0)`, and the job never runs.
#[test]
fn an_even_or_zero_run_count_is_refused() {
    let clock = FakeClock::default();
    let calls = Rc::new(Cell::new(0));
    for runs in [4, 0] {
        let mut probe = FakeProbe::new(&[0.0], 0.0);
        let mut job = counting_job(&clock, 10, &calls);
        let protocol = Protocol {
            measured_runs: runs,
            ..Protocol::default()
        };
        assert_eq!(
            protocol.run_job(0, &mut probe, &EngineClock::CallingThread(&clock), &mut job),
            Err(ProtocolError::RunCountNotOdd(runs))
        );
    }
    assert_eq!(calls.get(), 0);
}

/// The median of 10, 10, 10, 10, 1000 is 10, the middle value, not the
/// mean 208.
#[test]
fn median_of_n_is_the_middle_value_not_the_mean() {
    assert_eq!(median(&[10.0, 10.0, 10.0, 10.0, 1000.0]), Ok(10.0));
}

/// Two arms of one plan with medians 100 and 111 (5 runs each) give a
/// noise band of 11 / 100 = 0.11, relative to the smaller median, whichever
/// arm comes first.
#[test]
fn noise_band_from_two_arms_of_one_plan() {
    let arm_a = [98.0, 102.0, 100.0, 99.0, 101.0];
    let arm_b = [110.0, 113.0, 111.0, 109.0, 112.0];
    let band = noise_band(&arm_a, &arm_b).unwrap();
    assert!((band - 0.11).abs() < 1e-12, "{band}");
    let reversed = noise_band(&arm_b, &arm_a).unwrap();
    assert!((reversed - 0.11).abs() < 1e-12, "{reversed}");
}

/// With band 0.125: 112.5 vs 100 and 100 vs 112.5 are ties (exactly
/// 1.125×), 112.6 vs 100 is costlier, and 88 vs 100 is cheaper
/// (100 / 88 = 1.136 > 1.125). The values are exact in binary.
#[test]
fn differences_inside_the_noise_band_are_ties() {
    let band = 0.125;
    assert_eq!(compare(112.5, 100.0, band), Comparison::Tie);
    assert_eq!(compare(100.0, 112.5, band), Comparison::Tie);
    assert_eq!(compare(112.6, 100.0, band), Comparison::Costlier);
    assert_eq!(compare(88.0, 100.0, band), Comparison::Cheaper);
}

/// An arm with median 0 against one with median 5 cannot scale a band: the
/// error is `NonPositiveMedian { median: 0 }`.
#[test]
fn a_non_positive_median_cannot_set_a_noise_band() {
    let error = noise_band(&[0.0; 5], &[5.0; 5]).unwrap_err();
    assert_eq!(error, ProtocolError::NonPositiveMedian { median: 0.0 });
    assert!(error.to_string().contains('0'), "{error}");
}

/// A `/proc/<pid>/stat` line whose command name `(click (house) srv)` holds
/// spaces and parentheses has state `S`, the first field after the last `)`.
#[test]
fn process_state_from_a_proc_pid_stat_line() {
    let stat = "1234 (click (house) srv) S 1 1234 1234 0 -1 4194560 100 0 0 0 \
                250 50 0 0 20 0 1 0 100 1000 10\n";
    assert_eq!(process_state_from_stat(stat), Ok('S'));
}

/// Pids with 1 s and 2 s of thread CPU in `<proc_root>/<pid>/task/*/schedstat`
/// sum to 3 s; a pid without a stat file is an error naming the pid.
#[test]
fn process_accounting_sums_every_pid_and_names_a_missing_one() {
    let root = TempDir::new("processes");
    write_fake_process(&root.0, 101, 'S', &[1_000_000_000]);
    write_fake_process(&root.0, 102, 'S', &[1_500_000_000, 500_000_000]);
    assert_eq!(processes_cpu_ns(&root.0, &[101, 102]), Ok(3_000_000_000));

    let error = processes_cpu_ns(&root.0, &[101, 999]).unwrap_err();
    assert!(
        matches!(error, ProtocolError::MissingProcess { pid: 999, .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("999"), "{error}");
}

/// The OS thread clock does not see other threads: while a background thread
/// spins 100 ms and this thread sleeps 100 ms, thread CPU grows < 5 ms and
/// process CPU > 30 ms.
#[test]
#[ignore = "timing: run on a dedicated host"]
fn calling_thread_accounting_ignores_other_threads() {
    let clock = OsCpuClock;
    let thread_before = clock.thread_cpu_ns();
    let process_before = clock.process_cpu_ns();
    let spinner = std::thread::spawn(|| {
        let start = std::time::Instant::now();
        let mut x = 0_u64;
        while start.elapsed() < std::time::Duration::from_millis(100) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        x
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    spinner.join().unwrap();
    let thread_ns = clock.thread_cpu_ns() - thread_before;
    let process_ns = clock.process_cpu_ns() - process_before;
    assert!(thread_ns < 5_000_000, "thread CPU {thread_ns} ns");
    assert!(process_ns > 30_000_000, "process CPU {process_ns} ns");
}

/// Writes a fake process under `root`: `<pid>/stat` in state `state` (its
/// `utime` and `stime` 0, as on a real `/proc` for less than one 10 ms
/// tick), and one `<pid>/task/<tid>/schedstat` per entry of `thread_cpu_ns`
/// (tid = pid, pid + 1, ...), whose first field is that thread's CPU in ns.
fn write_fake_process(root: &std::path::Path, pid: u32, state: char, thread_cpu_ns: &[u64]) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("stat"),
        format!("{pid} (srv) {state} 1 {pid} {pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0\n"),
    )
    .unwrap();
    for (index, ns) in thread_cpu_ns.iter().enumerate() {
        let task = dir.join("task").join((pid + index as u32).to_string());
        std::fs::create_dir_all(&task).unwrap();
        std::fs::write(task.join("schedstat"), format!("{ns} 0 0\n")).unwrap();
    }
}

/// A `Processes` engine (pid 4343 under a fake `/proc`) whose measured runs
/// cost 90, 30, 70, 50 and 20 ns of server CPU (no warm-up): the samples
/// carry no thread CPU, and the median is the middle process CPU, 50, not
/// the third run (70), which a median over the (absent) thread CPU picks.
#[test]
fn a_processes_engine_median_is_the_middle_process_cpu() {
    let root = TempDir::new("process-median");
    write_fake_process(&root.0, 4343, 'S', &[0]);
    let engine_clock = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: fake_cpu_files(&root.0),
    }
    .clock_for(&CpuAccounting::Processes(vec![4343]))
    .unwrap();
    let costs = [90_u64, 30, 70, 50, 20];
    let mut call = 0;
    let mut server_cpu_ns = 0;
    let protocol = Protocol {
        warmup_runs: 0,
        measured_runs: 5,
        ..Protocol::default()
    };
    let record = protocol
        .run_job(
            0,
            &mut FakeProbe::new(&[0.0], 0.0),
            &engine_clock,
            &mut || {
                server_cpu_ns += costs[call];
                call += 1;
                write_fake_process(&root.0, 4343, 'S', &[server_cpu_ns]);
                Ok(StepReport::default())
            },
        )
        .unwrap();
    let samples: Vec<(Option<u64>, u64)> = record
        .samples
        .iter()
        .map(|sample| (sample.thread_cpu_ns, sample.process_cpu_ns))
        .collect();
    assert_eq!(
        samples,
        vec![(None, 90), (None, 30), (None, 70), (None, 50), (None, 20)]
    );
    assert_eq!(record.median.process_cpu_ns, 50);
}

/// A server pid in state `Z` (a zombie: exited, not yet reaped) has a stat
/// file but runs no more: reading it is an error naming the pid, as for a
/// missing one, both when the clock is built and on a later reading.
#[test]
fn a_zombie_server_pid_is_an_exited_process() {
    let root = TempDir::new("zombie");
    write_fake_process(&root.0, 4444, 'Z', &[1_000]);
    let error = processes_cpu_ns(&root.0, &[4444]).unwrap_err();
    assert_eq!(
        error,
        ProtocolError::ProcessExited {
            pid: 4444,
            state: 'Z'
        }
    );
    assert!(error.to_string().contains("4444"), "{error}");
    assert!(error.to_string().contains("exited"), "{error}");
    assert_eq!(
        ProcessesCpuClock::new(vec![4444], fake_cpu_files(&root.0)).unwrap_err(),
        error
    );

    write_fake_process(&root.0, 4445, 'S', &[1_000]);
    let clock = ProcessesCpuClock::new(vec![4445], fake_cpu_files(&root.0)).unwrap();
    write_fake_process(&root.0, 4445, 'Z', &[2_000]);
    assert_eq!(clock.process_cpu_ns(), 1_000, "the last good reading");
    assert_eq!(
        clock.take_read_error(),
        Some(ProtocolError::ProcessExited {
            pid: 4445,
            state: 'Z'
        })
    );
}

/// A server's CPU at ns resolution: two threads whose `schedstat` read
/// 2,000 and 3,000 ns sum to 5,000 ns. Field 14 of `stat` (`utime`, in
/// 10 ms ticks on a real `/proc`) would read 0 for both.
#[test]
fn server_cpu_is_the_sum_of_its_threads_schedstat() {
    let root = TempDir::new("schedstat");
    write_fake_process(&root.0, 4646, 'S', &[2_000, 3_000]);
    let clock = ProcessesCpuClock::new(vec![4646], fake_cpu_files(&root.0)).unwrap();
    assert_eq!(clock.process_cpu_ns(), 5_000);
    assert_eq!(clock.take_read_error(), None);
}

/// Puts `pids` in the cgroup v2 group `group` (their `<pid>/cgroup` reads
/// `0::<group>`, the group's `cgroup.procs` lists them) and writes the
/// group's `cpu.stat` with `usage_usec`.
fn write_fake_cgroup(root: &std::path::Path, pids: &[u32], group: &str, usage_usec: u64) {
    for pid in pids {
        std::fs::write(root.join(format!("{pid}/cgroup")), format!("0::{group}\n")).unwrap();
    }
    let dir = root.join("cgroup").join(group.trim_start_matches('/'));
    std::fs::create_dir_all(&dir).unwrap();
    let procs: String = pids.iter().map(|pid| format!("{pid}\n")).collect();
    std::fs::write(dir.join("cgroup.procs"), procs).unwrap();
    std::fs::write(
        dir.join("cpu.stat"),
        format!("usage_usec {usage_usec}\nuser_usec {usage_usec}\nsystem_usec 0\n"),
    )
    .unwrap();
}

/// Servers 4747 and 4748 in their own cgroup `/system.slice/engine.scope`
/// with `usage_usec 2500` read 2,500,000 ns, the group's CPU, not their
/// threads' 5,000 ns of `schedstat` (the group also counts threads that
/// exited). The same pids sharing the harness's group (`<proc>/self`), or
/// split over two groups, read their threads' `schedstat` instead.
#[test]
fn servers_in_their_own_cgroup_read_its_cpu_stat() {
    let root = TempDir::new("cgroup");
    write_fake_process(&root.0, 4747, 'S', &[2_000]);
    write_fake_process(&root.0, 4748, 'S', &[3_000]);
    let group = "/system.slice/engine.scope";
    write_fake_cgroup(&root.0, &[4747, 4748], group, 2_500);
    let read = |pids: &[u32]| {
        ProcessesCpuClock::new(pids.to_vec(), fake_cpu_files(&root.0))
            .unwrap()
            .process_cpu_ns()
    };
    assert_eq!(read(&[4747, 4748]), 2_500_000);

    // The harness in the same group: the group is not the servers' own.
    std::fs::create_dir_all(root.0.join("self")).unwrap();
    std::fs::write(root.0.join("self/cgroup"), format!("0::{group}\n")).unwrap();
    assert_eq!(read(&[4747, 4748]), 5_000);
    std::fs::remove_dir_all(root.0.join("self")).unwrap();

    // Two groups: no one group holds the servers.
    write_fake_cgroup(&root.0, &[4748], "/other.scope", 9);
    assert_eq!(read(&[4747, 4748]), 5_000);
}

/// The fake `/proc` at `root`, with its cgroup mount at `root/cgroup`.
fn fake_cpu_files(root: &std::path::Path) -> ProcessCpuFiles {
    ProcessCpuFiles {
        proc_root: root.to_path_buf(),
        cgroup_root: root.join("cgroup"),
    }
}

/// A load probe that counts its samples.
struct CountingProbe(u32);

impl LoadProbe for CountingProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        self.0 += 1;
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        0.0
    }
}

/// A core that does not exist (100000) is refused as `CoreNotAllowed`
/// before the idle check samples it: the probe is never read, and the job
/// never runs.
#[cfg(target_os = "linux")]
#[test]
fn a_nonexistent_core_is_refused_before_the_idle_sample() {
    let clock = FakeClock::default();
    let mut probe = CountingProbe(0);
    let calls = Rc::new(Cell::new(0));
    let mut job = counting_job(&clock, 10, &calls);
    let error = Protocol::default()
        .run_job(
            100_000,
            &mut probe,
            &EngineClock::CallingThread(&clock),
            &mut job,
        )
        .unwrap_err();
    assert!(
        matches!(error, ProtocolError::CoreNotAllowed { core: 100_000, .. }),
        "{error:?}"
    );
    assert_eq!(probe.0, 0, "the idle check sampled a nonexistent core");
    assert_eq!(calls.get(), 0);
}

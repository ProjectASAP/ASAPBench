//! A server's cgroup v2 files: CPU from `cpu.stat`, memory, reading them
//! over SSH from the server host, the clock over them and a whole arm run
//! on a fake server in its own group.

use std::path::{Path, PathBuf};

use aqpbm_core::harness::arm_run::{run_arm, ArmSchedule};
use aqpbm_core::harness::cgroup::{CgroupCpu, CgroupError, CgroupMemory, CgroupSource};
use aqpbm_core::harness::data::DataRecord;
use aqpbm_core::harness::engine::{
    ArmSpec, CpuAccounting, EngineUnderTest, QueryLanguage, QueryText, StepReport, WorkloadQuery,
    WorkloadSpec,
};
use aqpbm_core::harness::fake::{FakeEngine, FakeServer};
use aqpbm_core::harness::protocol::{
    LoadProbe, MeasurementClocks, OsCpuClock, Protocol, ProtocolError,
};
use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};

struct TempDir(PathBuf);

impl TempDir {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "aqpbm-core-harness-cgroup-{}-{test}",
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

struct IdleProbe;

impl LoadProbe for IdleProbe {
    fn core_busy_fraction(&mut self, _core: usize) -> Result<f64, ProtocolError> {
        Ok(0.0)
    }
    fn loadavg(&mut self) -> f64 {
        0.0
    }
}

fn cpu_stat(usage_usec: u64, nr_throttled: u64) -> String {
    format!("usage_usec {usage_usec}\nuser_usec 0\nsystem_usec 0\nnr_periods 0\nnr_throttled {nr_throttled}\nthrottled_usec 0\n")
}

/// A group directory with `cpu.stat` and the memory files of C2.
fn group(dir: &Path, usage_usec: u64) {
    std::fs::write(dir.join("cpu.stat"), cpu_stat(usage_usec, 0)).unwrap();
    std::fs::write(dir.join("memory.current"), "1048576\n").unwrap();
    std::fs::write(dir.join("memory.peak"), "2097152\n").unwrap();
    std::fs::write(
        dir.join("memory.stat"),
        "anon 524288\nfile 262144\nkernel 4096\n",
    )
    .unwrap();
}

/// C1: `usage_usec` in ns; no `usage_usec` line names the field.
#[test]
fn cpu_stat_reads_usage_in_ns() {
    let cpu = CgroupCpu::parse(&cpu_stat(2500, 0)).unwrap();
    assert_eq!(cpu.usage_ns, 2_500_000);
    assert_eq!(cpu.nr_throttled, 0);
    let error = CgroupCpu::parse("user_usec 1\n").unwrap_err();
    assert!(error.to_string().contains("usage_usec"), "{error}");
}

/// C2: memory files; no `memory.peak` (kernel < 5.19) reads `None`.
#[test]
fn memory_files_read_current_anon_file_and_lifetime_peak() {
    let temp = TempDir::new("memory");
    group(&temp.0, 0);
    let source = CgroupSource::Local(temp.0.clone());
    assert_eq!(
        source.memory().unwrap(),
        CgroupMemory {
            current_bytes: 1_048_576,
            anon_bytes: 524_288,
            file_bytes: 262_144,
            lifetime_peak_bytes: Some(2_097_152),
        }
    );
    std::fs::remove_file(temp.0.join("memory.peak")).unwrap();
    assert_eq!(source.memory().unwrap().lifetime_peak_bytes, None);
}

/// An executable shell script named `name` in `dir`.
#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

/// A stand-in for `ssh`: records its arguments in `ssh-arguments`, drops
/// the options and the host, and runs the remote command on this host.
#[cfg(unix)]
fn fake_ssh(dir: &Path) -> String {
    script(
        dir,
        "fake-ssh",
        "printf '%s\\n' \"$@\" > \"$(dirname \"$0\")/ssh-arguments\"\nfor last; do :; done\nexec sh -c \"$last\"\n",
    )
}

/// A remote source reads the same values as the local directory, afresh
/// per read, over one shared SSH connection; a path with a quote reaches
/// `cat` intact; files outside the cgroup set are refused.
#[cfg(unix)]
#[test]
fn ssh_reads_the_same_values_as_the_directory_and_nothing_else() {
    let temp = TempDir::new("ssh-'quoted'");
    group(&temp.0, 2500);
    let local = CgroupSource::Local(temp.0.clone());
    let remote = CgroupSource::Remote {
        ssh_program: fake_ssh(&temp.0),
        host: "bench@192.0.2.1".into(),
        cgroup_directory: temp.0.clone(),
    };
    assert_eq!(remote.cpu().unwrap(), local.cpu().unwrap());
    assert_eq!(remote.memory().unwrap(), local.memory().unwrap());
    let arguments = std::fs::read_to_string(temp.0.join("ssh-arguments")).unwrap();
    assert!(arguments.contains("ControlMaster=auto"), "{arguments}");
    assert!(arguments.contains("BatchMode=yes"), "{arguments}");
    // `--` ends the options, so a host can never be read as one.
    assert!(arguments.contains("--\nbench@192.0.2.1\n"), "{arguments}");
    std::fs::write(temp.0.join("cpu.stat"), cpu_stat(9000, 0)).unwrap();
    assert_eq!(remote.cpu().unwrap().usage_ns, 9_000_000);
    // Without `memory.peak` (an older kernel) the remote command exits 3,
    // whatever language the server host's error messages are in.
    std::fs::remove_file(temp.0.join("memory.peak")).unwrap();
    assert_eq!(remote.memory().unwrap().lifetime_peak_bytes, None);
    if Path::new("/proc/stat").exists() {
        assert!(remote.read("proc-stat").unwrap().starts_with("cpu"));
    }
    for file in ["cgroup.procs", "../cpu.stat"] {
        let error = remote.read(file).unwrap_err();
        assert!(error.to_string().contains(file), "{error}");
    }
}

/// SSH's own failure (exit status 255) names the host and SSH's message.
#[cfg(unix)]
#[test]
fn an_unreachable_host_is_an_ssh_error() {
    let temp = TempDir::new("ssh-unreachable");
    let remote = CgroupSource::Remote {
        ssh_program: script(
            &temp.0,
            "refused-ssh",
            "echo 'ssh: connect to host 192.0.2.1 port 22: Connection refused' >&2\nexit 255\n",
        ),
        host: "bench@192.0.2.1".into(),
        cgroup_directory: PathBuf::from("/sys/fs/cgroup/system.slice/docker-1a2b.scope"),
    };
    match remote.cpu().unwrap_err() {
        CgroupError::Remote { host, message } => {
            assert_eq!(host, "bench@192.0.2.1");
            assert!(message.contains("Connection refused"), "{message}");
        }
        other => panic!("expected an SSH error, got {other:?}"),
    }
}

/// C4: the clock reads `usage_usec` around a step; thread CPU 0. An
/// unreadable group fails when the clock is made.
#[test]
fn cgroup_clock_reads_usage_and_fails_early_on_a_missing_group() {
    let temp = TempDir::new("clock");
    group(&temp.0, 1000);
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    let clock = clocks
        .clock_for(&CpuAccounting::Cgroup(CgroupSource::Local(temp.0.clone())))
        .unwrap();
    let cpu = clock.as_clock();
    let before = cpu.process_cpu_ns();
    std::fs::write(temp.0.join("cpu.stat"), cpu_stat(3500, 0)).unwrap();
    assert_eq!(cpu.process_cpu_ns() - before, 2_500_000);
    assert_eq!(cpu.thread_cpu_ns(), 0);
    assert!(clocks
        .clock_for(&CpuAccounting::Cgroup(CgroupSource::Local(
            temp.0.join("gone")
        )))
        .is_err());
}

/// C5: `nr_throttled` above its value at clock creation fails the job.
#[test]
fn a_throttled_group_fails_the_job() {
    let temp = TempDir::new("throttled");
    group(&temp.0, 1000);
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    let clock = clocks
        .clock_for(&CpuAccounting::Cgroup(CgroupSource::Local(temp.0.clone())))
        .unwrap();
    let protocol = Protocol {
        warmup_runs: 0,
        measured_runs: 1,
        max_core_busy_fraction: 1.0,
    };
    let dir = temp.0.clone();
    let result = protocol.run_job(0, &mut IdleProbe, &clock, &mut || {
        std::fs::write(dir.join("cpu.stat"), cpu_stat(2000, 1)).unwrap();
        Ok(StepReport::default())
    });
    assert_eq!(
        result.unwrap_err(),
        ProtocolError::CgroupThrottled { nr_throttled: 1 }
    );
}

/// C6: `cpu.stat` has µs resolution: 1,500 ns twice reads 1 µs then 3 µs,
/// so the steps measure 1,000 and 2,000 ns.
#[test]
fn cgroup_cpu_is_quantized_to_whole_microseconds() {
    let temp = TempDir::new("quantized");
    let server =
        FakeServer::start_in_cgroup(&temp.0.join("proc"), 4242, &temp.0.join("group")).unwrap();
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    let clock = clocks
        .clock_for(&CpuAccounting::Cgroup(CgroupSource::Local(
            temp.0.join("group"),
        )))
        .unwrap();
    let cpu = clock.as_clock();
    let mut steps = Vec::new();
    for _ in 0..2 {
        let before = cpu.process_cpu_ns();
        server.charge(1_500);
        steps.push(cpu.process_cpu_ns() - before);
    }
    assert_eq!(steps, vec![1_000, 2_000]);
}

/// 2 series, one sample per second for 60 s from 1,000 ms; one
/// `sum_over_time` query over 10 s every 10 s.
fn two_series_workload() -> WorkloadSpec {
    WorkloadSpec {
        name: "two".into(),
        queries: vec![WorkloadQuery {
            id: "sum".into(),
            texts: vec![QueryText {
                language: QueryLanguage::PromQl,
                text: "sum_over_time(m[10s])".into(),
            }],
            window_ms: 10_000,
            evaluation_interval_ms: 10_000,
            truth: None,
        }],
        data: vec![DataRecord {
            metric: "m".into(),
            description: TimeSeriesDescription {
                labels: vec![SeriesLabelSpec {
                    name: "s".into(),
                    cardinality: 2,
                    skewness: 0.0,
                }],
                series_num: 2,
                start_ms: 1_000,
                sample_interval_ms: 1_000,
                duration_ms: 60_000,
                value_distribution: DataDistribution::Uniform(UniformParameter {
                    lower_bound: 0.0,
                    upper_bound: 10.0,
                    seed: 0,
                }),
                counter: false,
                seed: 1,
            },
        }],
    }
}

/// A1: a whole arm on a fake server in its own group. Ingesting a chunk of
/// `n` samples charges 10 µs + 50 ns·n (whole µs for n = 20: 11 µs);
/// medians come from process CPU and carry no thread CPU; the record has
/// the sample count, the memory before and after, and the whole-arm CPU,
/// which also counts 500 µs the fake charges at each install.
#[test]
fn an_arm_on_a_cgroup_server_records_cpu_memory_and_samples() {
    let temp = TempDir::new("arm");
    let server =
        FakeServer::start_in_cgroup(&temp.0.join("proc"), 4243, &temp.0.join("group")).unwrap();
    let engine = FakeEngine {
        server: Some(server),
        ns_per_install: 500_000,
        ..FakeEngine::default()
    };
    assert_eq!(
        engine.cpu_accounting(),
        CpuAccounting::Cgroup(CgroupSource::Local(temp.0.join("group")))
    );
    let workload = two_series_workload();
    let schedule = ArmSchedule {
        refresh_ms: 10_000,
        evaluate_at_ms: vec![61_000],
    };
    let protocol = Protocol {
        warmup_runs: 0,
        measured_runs: 3,
        max_core_busy_fraction: 1.0,
    };
    let clocks = MeasurementClocks::with_calling_thread(&OsCpuClock);
    let arm = ArmSpec {
        name: "exact".into(),
        raw_data_retained: true,
        engine_options: serde_json::Value::Null,
    };
    let result = run_arm(
        &engine,
        &workload,
        &arm,
        &schedule,
        &protocol,
        0,
        &mut IdleProbe,
        &clocks,
    )
    .unwrap();
    assert_eq!(result.ingested_samples, 120);
    assert_eq!(
        result.cpu_source,
        format!("cgroup {}", temp.0.join("group").join("cpu.stat").display())
    );
    assert_eq!(result.installs_held, 3);
    assert_eq!(result.ingest.len(), 6);
    for job in &result.ingest {
        assert_eq!(job.median.thread_cpu_ns, None);
        // 10,000 + 50 * 20 = 11,000 ns, a whole number of µs.
        assert_eq!(job.median.process_cpu_ns, 11_000);
    }
    let memory = CgroupMemory {
        current_bytes: 3 * 1_048_576,
        anon_bytes: 2 * 1_048_576,
        file_bytes: 1_048_576,
        lifetime_peak_bytes: Some(5 * 1_048_576),
    };
    assert_eq!(result.server_memory_before, Some(memory));
    assert_eq!(result.server_memory_after, Some(memory));
    let steps_ns: u64 = result
        .ingest
        .iter()
        .chain(&result.evaluate)
        .flat_map(|job| &job.samples)
        .map(|cost| cost.process_cpu_ns)
        .sum();
    assert_eq!(result.server_cpu_ns_whole_arm, Some(steps_ns + 3 * 500_000));
}

/// The server's cores come from its cpu list; the idle check takes the
/// busiest of them.
#[test]
fn the_idle_check_watches_the_server_cores() {
    use aqpbm_core::harness::protocol::{
        foreign_busy_fraction, load_probe_for, parse_cpu_list, ProcessCpuFiles,
    };
    assert_eq!(parse_cpu_list("2,22\n").unwrap(), vec![2, 22]);
    assert_eq!(parse_cpu_list("0-3,8").unwrap(), vec![0, 1, 2, 3, 8]);
    assert!(parse_cpu_list("3-1").is_err());
    // core 2: 1 busy tick of 10; core 22: 5 of 10.
    let before = "cpu2 0 0 0 0 0 0 0 0\ncpu22 0 0 0 0 0 0 0 0\n";
    let after = "cpu2 1 0 0 9 0 0 0 0\ncpu22 5 0 0 5 0 0 0 0\n";
    // No server CPU: 6 busy ticks of 20, less 4 ticks of rounding.
    assert_eq!(
        foreign_busy_fraction(&[2, 22], before, after, 0, 10_000_000).unwrap(),
        0.1
    );
    // The server's own work is not foreign load: 6 busy ticks (60 ms at
    // 10 ms a tick) of 20, the server used 10 ms, 2 ticks per core (40 ms)
    // are rounding → 10 ms of 200 ms.
    assert_eq!(
        foreign_busy_fraction(&[2, 22], before, after, 10_000_000, 10_000_000).unwrap(),
        0.05
    );
    // A server busier than the ticks show (tick rounding) is never negative.
    assert_eq!(
        foreign_busy_fraction(&[2, 22], before, after, 90_000_000, 10_000_000).unwrap(),
        0.0
    );

    let temp = TempDir::new("probe");
    let proc_root = temp.0.join("proc");
    std::fs::create_dir_all(proc_root.join("40")).unwrap();
    std::fs::write(
        proc_root.join("40/status"),
        "Name:\tserver\nCpus_allowed_list:\t2,22\n",
    )
    .unwrap();
    let files = ProcessCpuFiles {
        proc_root: proc_root.clone(),
        cgroup_root: temp.0.join("cgroup"),
    };
    assert!(load_probe_for(&CpuAccounting::Processes(vec![40]), &files).is_ok());
    let group_dir = temp.0.join("group");
    std::fs::create_dir_all(&group_dir).unwrap();
    std::fs::write(group_dir.join("cpuset.cpus.effective"), "4-5\n").unwrap();
    assert!(load_probe_for(
        &CpuAccounting::Cgroup(CgroupSource::Local(group_dir)),
        &files
    )
    .is_ok());
    assert!(load_probe_for(&CpuAccounting::Processes(vec![41]), &files).is_err());
}

/// A `Processes` engine's group is read only when every process in it is
/// the server or its descendant; a foreign process makes the clock read
/// `schedstat`.
#[test]
fn a_group_with_a_foreign_process_is_not_the_server_cpu() {
    let temp = TempDir::new("foreign");
    let server = FakeServer::start(&temp.0, 40).unwrap();
    let group = server.cpu_files.cgroup_root.join("srv.scope");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cpu.stat"), cpu_stat(400_000, 0)).unwrap();
    std::fs::write(temp.0.join("40/cgroup"), "0::/srv.scope\n").unwrap();
    // The server's child, pid 41 (parent 40), in the same group.
    std::fs::create_dir_all(temp.0.join("41")).unwrap();
    std::fs::write(temp.0.join("41/stat"), "41 (child) S 40 1 1 0\n").unwrap();
    std::fs::write(group.join("cgroup.procs"), "40\n41\n").unwrap();
    let clocks = MeasurementClocks {
        calling_thread: &OsCpuClock,
        process_cpu_files: server.cpu_files.clone(),
    };
    let own = clocks
        .clock_for(&CpuAccounting::Processes(vec![40]))
        .unwrap();
    assert_eq!(
        own.cpu_source(),
        format!("cgroup {}", group.join("cpu.stat").display())
    );
    // An unrelated pid 99 (parent 1) joins the group.
    std::fs::create_dir_all(temp.0.join("99")).unwrap();
    std::fs::write(temp.0.join("99/stat"), "99 (other) S 1 1 1 0\n").unwrap();
    std::fs::write(group.join("cgroup.procs"), "40\n41\n99\n").unwrap();
    let shared = clocks
        .clock_for(&CpuAccounting::Processes(vec![40]))
        .unwrap();
    assert_eq!(shared.cpu_source(), "schedstat");
}

#[cfg(unix)]
#[test]
fn a_file_on_the_server_host_is_replaced_locally_and_over_ssh() {
    let temp = TempDir::new("replace-'quoted'");
    let config = temp.0.join("stream-aggregation.yaml");
    std::fs::write(&config, "[]\n").unwrap();
    let local = CgroupSource::Local(temp.0.clone());
    local
        .replace_file_on_server_host(&config, "- interval: 10s\n")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "- interval: 10s\n"
    );
    let remote = CgroupSource::Remote {
        ssh_program: fake_ssh(&temp.0),
        host: "bench@192.0.2.1".into(),
        cgroup_directory: temp.0.clone(),
    };
    remote
        .replace_file_on_server_host(&config, "- interval: 20s\n")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "- interval: 20s\n"
    );
    assert!(!temp.0.join("stream-aggregation.yaml.tmp").exists());
    #[cfg(unix)]
    {
        let target = temp.0.join("not-the-config");
        std::fs::write(&target, "kept").unwrap();
        std::os::unix::fs::symlink(&target, temp.0.join("stream-aggregation.yaml.tmp")).unwrap();
        local
            .replace_file_on_server_host(&config, "- interval: 30s\n")
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
        std::os::unix::fs::symlink(&target, temp.0.join("stream-aggregation.yaml.tmp")).unwrap();
        remote
            .replace_file_on_server_host(&config, "- interval: 40s\n")
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "- interval: 40s\n"
        );
    }
    let missing = temp.0.join("no-such-directory").join("config.yaml");
    assert!(matches!(
        remote.replace_file_on_server_host(&missing, "x"),
        Err(CgroupError::Write { .. })
    ));
}

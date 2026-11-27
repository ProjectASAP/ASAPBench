use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use aqpbm_core::harness::cpu_frequency::{governors, CpuFrequencyError, HeldGovernor};

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cpu_root(test: &str, cpus: &[(&str, &str, Option<&str>)]) -> TempRoot {
    let root = std::env::temp_dir().join(format!(
        "aqpbm-core-harness-cpu-frequency-{}-{test}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("cpufreq")).unwrap();
    fs::write(root.join("online"), "0-1\n").unwrap();
    for (cpu, governor, available) in cpus {
        let directory = root.join(cpu).join("cpufreq");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("scaling_governor"), format!("{governor}\n")).unwrap();
        if let Some(available) = available {
            fs::write(
                directory.join("scaling_available_governors"),
                format!("{available}\n"),
            )
            .unwrap();
        }
    }
    TempRoot(root)
}

fn governor_of(root: &Path, cpu: &str) -> String {
    fs::read_to_string(root.join(cpu).join("cpufreq").join("scaling_governor"))
        .unwrap()
        .trim()
        .to_string()
}

const BOTH: Option<&str> = Some("performance schedutil");

#[test]
fn holds_the_governor_and_restores_each_cpu_its_own() {
    let root = cpu_root(
        "holds",
        &[
            ("cpu0", "schedutil", BOTH),
            ("cpu1", "powersave", Some("performance powersave")),
        ],
    );
    let held = HeldGovernor::hold(&root.0, "performance").unwrap();
    assert_eq!(governor_of(&root.0, "cpu0"), "performance");
    assert_eq!(governor_of(&root.0, "cpu1"), "performance");
    assert_eq!(
        governors(&root.0).unwrap(),
        BTreeSet::from(["performance".to_string()])
    );
    held.restore().unwrap();
    assert_eq!(governor_of(&root.0, "cpu0"), "schedutil");
    assert_eq!(governor_of(&root.0, "cpu1"), "powersave");
}

#[test]
fn dropping_the_hold_restores_too() {
    let root = cpu_root("drop", &[("cpu0", "schedutil", BOTH)]);
    {
        let _held = HeldGovernor::hold(&root.0, "performance").unwrap();
        assert_eq!(governor_of(&root.0, "cpu0"), "performance");
    }
    assert_eq!(governor_of(&root.0, "cpu0"), "schedutil");
}

#[test]
fn reads_the_governors_in_effect() {
    let root = cpu_root(
        "read",
        &[
            ("cpu0", "schedutil", BOTH),
            ("cpu1", "performance", BOTH),
            ("cpu10", "schedutil", None),
        ],
    );
    assert_eq!(
        governors(&root.0).unwrap(),
        BTreeSet::from(["performance".to_string(), "schedutil".to_string()])
    );
}

#[test]
fn refuses_a_governor_some_cpu_lacks_and_changes_nothing() {
    let root = cpu_root(
        "unavailable",
        &[
            ("cpu0", "schedutil", BOTH),
            ("cpu1", "schedutil", Some("schedutil powersave")),
        ],
    );
    let error = HeldGovernor::hold(&root.0, "performance").unwrap_err();
    assert!(
        matches!(&error, CpuFrequencyError::Unavailable { policy, .. } if policy.ends_with("cpu1/cpufreq")),
        "{error}"
    );
    assert_eq!(governor_of(&root.0, "cpu0"), "schedutil");
    assert_eq!(governor_of(&root.0, "cpu1"), "schedutil");
}

#[test]
fn a_machine_without_cpufreq_has_no_governor() {
    let missing = std::env::temp_dir().join(format!(
        "aqpbm-core-harness-cpu-frequency-{}-missing",
        std::process::id()
    ));
    let error = HeldGovernor::hold(&missing, "performance").unwrap_err();
    assert!(matches!(error, CpuFrequencyError::NoGovernor(_)), "{error}");
    let root = cpu_root("empty", &[]);
    assert!(matches!(
        governors(&root.0).unwrap_err(),
        CpuFrequencyError::NoGovernor(_)
    ));
}

#[cfg(unix)]
#[test]
fn a_failed_write_restores_the_cpus_already_written() {
    use std::os::unix::fs::PermissionsExt;
    let root = cpu_root(
        "write-fails",
        &[("cpu0", "schedutil", BOTH), ("cpu1", "schedutil", BOTH)],
    );
    let locked = root.0.join("cpu1").join("cpufreq").join("scaling_governor");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::write(&locked, "schedutil").is_ok() {
        return;
    }
    let error = HeldGovernor::hold(&root.0, "performance").unwrap_err();
    assert!(matches!(error, CpuFrequencyError::Write { .. }), "{error}");
    assert!(error.to_string().contains("needs root"), "{error}");
    assert_eq!(governor_of(&root.0, "cpu0"), "schedutil");
}

#[cfg(unix)]
#[test]
fn cpus_sharing_a_policy_get_its_own_governor_back() {
    let root = cpu_root("shared", &[]);
    let policy = root.0.join("cpufreq").join("policy0");
    fs::create_dir_all(&policy).unwrap();
    fs::write(policy.join("scaling_governor"), "schedutil\n").unwrap();
    fs::write(
        policy.join("scaling_available_governors"),
        "performance schedutil\n",
    )
    .unwrap();
    for cpu in ["cpu0", "cpu1"] {
        fs::create_dir_all(root.0.join(cpu)).unwrap();
        std::os::unix::fs::symlink("../cpufreq/policy0", root.0.join(cpu).join("cpufreq")).unwrap();
    }
    let held = HeldGovernor::hold(&root.0, "performance").unwrap();
    assert_eq!(governor_of(&root.0, "cpu1"), "performance");
    held.restore().unwrap();
    assert_eq!(governor_of(&root.0, "cpu0"), "schedutil");
    assert_eq!(governor_of(&root.0, "cpu1"), "schedutil");
}

#[test]
fn offline_cpus_are_left_alone() {
    let root = cpu_root(
        "offline",
        &[("cpu0", "schedutil", BOTH), ("cpu1", "schedutil", BOTH)],
    );
    fs::write(root.0.join("cpu1").join("online"), "0\n").unwrap();
    let held = HeldGovernor::hold(&root.0, "performance").unwrap();
    assert_eq!(governor_of(&root.0, "cpu0"), "performance");
    assert_eq!(governor_of(&root.0, "cpu1"), "schedutil");
    assert_eq!(
        governors(&root.0).unwrap(),
        BTreeSet::from(["performance".to_string()])
    );
    held.restore().unwrap();
}

#[cfg(unix)]
#[test]
fn an_interrupt_restores_the_governor() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    const CHILD: &str = "AQPBM_CPU_FREQUENCY_TEST_ROOT";
    if let Some(child_root) = std::env::var_os(CHILD) {
        let held = HeldGovernor::hold(Path::new(&child_root), "performance").unwrap();
        held.restore_on_interrupt_or_panic();
        println!("held");
        std::thread::sleep(std::time::Duration::from_secs(30));
        return;
    }
    let root = cpu_root("interrupt", &[("cpu0", "schedutil", BOTH)]);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "an_interrupt_restores_the_governor",
            "--nocapture",
        ])
        .env(CHILD, &root.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(lines.any(|line| line.unwrap().trim() == "held"));
    assert_eq!(governor_of(&root.0, "cpu0"), "performance");
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert_eq!(governor_of(&root.0, "cpu0"), "schedutil");
}

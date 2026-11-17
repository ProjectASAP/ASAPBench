//! The machine key: architecture, CPU model, logical cores and memory type,
//! never the hostname and never the memory size.

use aqpbm_core::harness::machine::MachineDescription;

const E5_MODEL: &str = "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz";
const E5_MEMORY_KIB: u64 = 263_724_000;
const E5_KEY: &str = "x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4";

/// `/proc/cpuinfo` of `cores` logical cores of `model`.
fn cpuinfo(model: &str, cores: u32) -> String {
    (0..cores)
        .map(|core| format!("processor\t: {core}\nmodel name\t: {model}\n\n"))
        .collect()
}

fn meminfo(kib: u64) -> String {
    format!("MemTotal:       {kib} kB\nMemFree:        1000 kB\n")
}

/// 56 logical cores of the E5-2683 v3 on x86_64 with `memory_kib` of memory,
/// DIMM types `dimm_types` (EDAC) and an optional override.
fn e5(
    memory_kib: u64,
    dimm_types: &[&str],
    memory_type_override: Option<&str>,
) -> MachineDescription {
    MachineDescription::from_proc(
        "x86_64",
        &cpuinfo(E5_MODEL, 56),
        &meminfo(memory_kib),
        dimm_types,
        memory_type_override,
        "Linux 5.15",
    )
    .unwrap()
}

/// Architecture, model, cores and memory type give the slug: `(R)`, `CPU`
/// and `@ 2.00GHz` are dropped from the model, the rest is lowercased and
/// dash-joined. 56 cores of the E5-2683 v3 with registered DDR4 DIMMs give
/// `x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4`.
/// Moved from the ASAPPlanner test `machine_key_from_cpu_cores_memory`.
#[test]
fn machine_key_from_cpu_cores_memory() {
    let machine = e5(E5_MEMORY_KIB, &["Registered-DDR4", "Registered-DDR4"], None);
    assert_eq!(machine.cpu_architecture, "x86_64");
    assert_eq!(machine.cpu_model, E5_MODEL);
    assert_eq!(machine.logical_cores, 56);
    assert_eq!(machine.memory_type, "Registered-DDR4");
    assert_eq!(machine.memory_kib, E5_MEMORY_KIB);
    assert_eq!(machine.os, "Linux 5.15");
    assert_eq!(machine.machine_key(), E5_KEY);
}

/// The key is a function of architecture, CPU model, cores and memory type
/// only: this machine's detected key contains no hostname token of 3 or more
/// characters that the CPU model does not also contain.
/// Moved from the ASAPPlanner test `machine_key_ignores_hostname`.
#[test]
fn machine_key_ignores_hostname() {
    let fixture = e5(E5_MEMORY_KIB, &["Registered-DDR4"], None).machine_key();
    for hostname in ["a", "xeon-e5-2683", "node-17"] {
        // The fixture's inputs do not include a hostname, so every
        // hostname gives the same key, and it is never the hostname.
        assert_ne!(fixture, hostname);
    }
    let detected = MachineDescription::detect(None).unwrap();
    let key = detected.machine_key();
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let model = detected.cpu_model.to_lowercase();
    for token in hostname
        .trim()
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
    {
        if token.len() >= 3 && !model.contains(token) {
            assert!(
                !key.contains(token),
                "key {key} contains hostname token {token}"
            );
        }
    }
}

/// Total memory is not part of the key: 64 GiB, 256 GiB and `MemTotal`
/// +0.5% (boot jitter) of the same DIMM type all give `E5_KEY`, while
/// `memory_kib` differs.
/// Moved from the ASAPPlanner test `machine_key_ignores_memory_size`.
#[test]
fn machine_key_ignores_memory_size() {
    let dimms = ["Registered-DDR4"];
    let key = e5(E5_MEMORY_KIB, &dimms, None).machine_key();
    assert_eq!(key, E5_KEY);
    let small = e5(64 * 1024 * 1024, &dimms, None);
    assert_eq!(small.machine_key(), key);
    assert_eq!(small.memory_kib, 64 * 1024 * 1024);
    assert_eq!(e5(256 * 1024 * 1024, &dimms, None).machine_key(), key);
    assert_eq!(
        e5(E5_MEMORY_KIB * 1005 / 1000, &dimms, None).machine_key(),
        key
    );
}

/// A VM without EDAC and without an override: `unknown-memory` ends the
/// key (`...-56c-unknown-memory`), and the warning names it and the
/// `AQPBM_MEMORY_TYPE` override. Known DIMMs give no warning.
/// Moved from the ASAPPlanner test `machine_key_without_edac_is_unknown_memory`.
#[test]
fn machine_key_without_edac_is_unknown_memory() {
    let machine = e5(E5_MEMORY_KIB, &[], None);
    assert_eq!(
        machine.machine_key(),
        "x86_64-intel-xeon-e5-2683-v3-56c-unknown-memory"
    );
    let warning = machine.warning().expect("a warning for unknown memory");
    assert!(warning.contains("unknown-memory"), "{warning}");
    assert!(warning.contains("AQPBM_MEMORY_TYPE"), "{warning}");
    assert!(e5(E5_MEMORY_KIB, &["Registered-DDR4"], None)
        .warning()
        .is_none());
}

/// `--memory-type` / `AQPBM_MEMORY_TYPE` = `Registered-DDR4` supplies the
/// type when EDAC has none: the key is `E5_KEY` and no warning is given.
/// Moved from the ASAPPlanner test `machine_key_memory_type_override`.
#[test]
fn machine_key_memory_type_override() {
    let machine = e5(E5_MEMORY_KIB, &[], Some("Registered-DDR4"));
    assert_eq!(machine.machine_key(), E5_KEY);
    assert!(machine.warning().is_none());
}

/// DIMMs of different types give the sorted unique values joined, whatever
/// the DIMM order: `[Registered, LRDIMM, Registered]` and a permutation both
/// end `-56c-lrdimm-ddr4-registered-ddr4`.
/// Moved from the ASAPPlanner test `machine_key_mixed_dimm_types`.
#[test]
fn machine_key_mixed_dimm_types() {
    for dimms in [
        ["Registered-DDR4", "LRDIMM-DDR4", "Registered-DDR4"],
        ["LRDIMM-DDR4", "Registered-DDR4", "LRDIMM-DDR4"],
    ] {
        assert_eq!(
            e5(E5_MEMORY_KIB, &dimms, None).machine_key(),
            "x86_64-intel-xeon-e5-2683-v3-56c-lrdimm-ddr4-registered-ddr4"
        );
    }
}

/// A model with `(TM)`, a double space, `@ 2.35GHz` and a trailing `(R)`
/// still gives only `[a-z0-9-]`, with no leading, trailing or double dash.
/// Moved from the ASAPPlanner test `machine_key_slug_charset`.
#[test]
fn machine_key_slug_charset() {
    let machine = MachineDescription::from_proc(
        "aarch64",
        &cpuinfo("AMD EPYC(TM) 7452 32-Core Processor  @ 2.35GHz (R)", 8),
        &meminfo(16_000_000),
        &["Registered-DDR4"],
        None,
        "Linux",
    )
    .unwrap();
    let key = machine.machine_key();
    assert!(
        key.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "{key}"
    );
    assert!(
        !key.contains("--") && !key.starts_with('-') && !key.ends_with('-'),
        "{key}"
    );
}

/// `logical_cores` counts the `processor` lines of `/proc/cpuinfo`, not this
/// thread's affinity mask: a thread pinned to core 0 (as under `taskset -c 0`)
/// detects the same key and core count as an unpinned one.
#[cfg(target_os = "linux")]
#[test]
fn machine_key_counts_cpuinfo_processors_not_the_affinity_mask() {
    let unpinned = MachineDescription::detect(None).unwrap();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap();
    let processors = cpuinfo
        .lines()
        .filter(|line| line.split(':').next().map(str::trim) == Some("processor"))
        .count() as u32;
    assert_eq!(unpinned.logical_cores, processors);

    let pinned = std::thread::spawn(|| {
        // SAFETY: `set` is a plain bit mask owned by this frame, and
        // `sched_setaffinity(0, ..)` only changes the calling thread.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(0, &mut set);
            let status = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
            assert_eq!(status, 0, "pinning to core 0 failed");
        }
        MachineDescription::detect(None).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(pinned.logical_cores, processors);
    assert_eq!(pinned.machine_key(), unpinned.machine_key());
}

/// An aarch64-style `/proc/cpuinfo` with `processor`, `BogoMIPS` and
/// `CPU part` but no `model name` is an error naming the `model name` field.
#[test]
fn cpuinfo_without_a_model_name_is_an_error_naming_the_field() {
    let cpuinfo: String = (0..4)
        .map(|core| format!("processor\t: {core}\nBogoMIPS\t: 50.00\nCPU part\t: 0xd0c\n\n"))
        .collect();
    let error = MachineDescription::from_proc(
        "aarch64",
        &cpuinfo,
        &meminfo(16_000_000),
        &["Registered-DDR4"],
        None,
        "Linux",
    )
    .unwrap_err();
    assert!(error.to_string().contains("model name"), "{error}");
}

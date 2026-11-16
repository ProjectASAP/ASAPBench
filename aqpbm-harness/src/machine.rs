//! The machine a measurement ran on, and the key calibrations and runs are
//! stored under: CPU architecture, CPU model, logical cores and memory type.
//! Never the hostname and never the memory size.
//!
//! Example: an "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz" on x86_64 with 56
//! logical cores and registered DDR4 DIMMs is
//! `x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The memory type of a machine whose DIMM types cannot be read (no EDAC, as
/// in a VM) and that has no override.
pub const UNKNOWN_MEMORY_TYPE: &str = "unknown-memory";

/// The environment variable that overrides the memory type, like `--memory-type`.
pub const MEMORY_TYPE_ENV: &str = "AQPBM_MEMORY_TYPE";

/// What `machine.json` and `run.json` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineDescription {
    /// `std::env::consts::ARCH`, for example `x86_64`.
    pub cpu_architecture: String,
    /// The `model name` of `/proc/cpuinfo`.
    pub cpu_model: String,
    /// `processor` lines of `/proc/cpuinfo`: the machine's cores, not this
    /// process's affinity mask.
    pub logical_cores: u32,
    /// The DIMM type, for example `Registered-DDR4`; differing DIMM types are
    /// the sorted unique values joined by `-`; [`UNKNOWN_MEMORY_TYPE`] when
    /// unreadable and not overridden.
    pub memory_type: String,
    /// `MemTotal`, in KiB. Recorded for information; not part of the key.
    pub memory_kib: u64,
    pub os: String,
}

impl MachineDescription {
    /// The machine `/proc/cpuinfo`, `/proc/meminfo` and the EDAC DIMM types
    /// (`/sys/devices/system/edac/mc/mc*/dimm*/dimm_mem_type`, one string per
    /// DIMM, empty without EDAC) describe. `memory_type_override`
    /// (`--memory-type` / [`MEMORY_TYPE_ENV`]) replaces the EDAC types.
    pub fn from_proc(
        cpu_architecture: &str,
        cpuinfo: &str,
        meminfo: &str,
        dimm_memory_types: &[&str],
        memory_type_override: Option<&str>,
        os: &str,
    ) -> Result<Self, MachineError> {
        let field = |text: &str, file: &str, name: &str| -> Result<String, MachineError> {
            text.lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == name).then(|| value.trim().to_string())
                })
                .ok_or_else(|| MachineError::MissingField {
                    file: file.into(),
                    field: name.into(),
                })
        };
        let cpu_model = field(cpuinfo, "/proc/cpuinfo", "model name")?;
        let logical_cores = cpuinfo
            .lines()
            .filter(|line| line.split(':').next().map(str::trim) == Some("processor"))
            .count() as u32;
        let mem_total = field(meminfo, "/proc/meminfo", "MemTotal")?;
        let memory_kib = mem_total
            .trim_end_matches("kB")
            .trim()
            .parse()
            .map_err(|_| MachineError::MissingField {
                file: "/proc/meminfo".into(),
                field: format!("MemTotal (a number of kB, got {mem_total:?})"),
            })?;
        let memory_type = match memory_type_override {
            Some(memory_type) => memory_type.to_string(),
            None => {
                let types: BTreeSet<&str> = dimm_memory_types
                    .iter()
                    .map(|t| t.trim())
                    .filter(|t| !t.is_empty())
                    .collect();
                if types.is_empty() {
                    UNKNOWN_MEMORY_TYPE.to_string()
                } else {
                    types.into_iter().collect::<Vec<_>>().join("-")
                }
            }
        };
        Ok(MachineDescription {
            cpu_architecture: cpu_architecture.to_string(),
            cpu_model,
            logical_cores,
            memory_type,
            memory_kib,
            os: os.to_string(),
        })
    }

    /// This machine, read from `/proc` and `/sys`.
    pub fn detect(memory_type_override: Option<&str>) -> Result<Self, MachineError> {
        let read = |path: &str| {
            std::fs::read_to_string(path).map_err(|error| MachineError::Read {
                path: path.into(),
                message: error.to_string(),
            })
        };
        let dimm_types = edac_dimm_memory_types();
        let dimm_types: Vec<&str> = dimm_types.iter().map(String::as_str).collect();
        let os = ["/proc/sys/kernel/ostype", "/proc/sys/kernel/osrelease"]
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .map(|text| text.trim().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        Self::from_proc(
            std::env::consts::ARCH,
            &read("/proc/cpuinfo")?,
            &read("/proc/meminfo")?,
            &dimm_types,
            memory_type_override,
            if os.is_empty() {
                std::env::consts::OS
            } else {
                &os
            },
        )
    }

    /// The directory name calibrations and runs are stored under: `[a-z0-9_-]+`
    /// from architecture, model, cores and memory type, e.g.
    /// `x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4`.
    ///
    /// The model drops `(R)`, `(TM)`, the word `CPU` and everything from `@`
    /// (the clock); the architecture is kept as the OS names it, so its
    /// underscore stays (`x86_64`).
    pub fn machine_key(&self) -> String {
        let model = self
            .cpu_model
            .split('@')
            .next()
            .unwrap_or("")
            .replace("(R)", " ")
            .replace("(TM)", " ")
            .replace("(tm)", " ")
            .split_whitespace()
            .filter(|word| *word != "CPU")
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{}-{}-{}c-{}",
            self.cpu_architecture.to_lowercase(),
            slug(&model),
            self.logical_cores,
            slug(&self.memory_type)
        )
    }

    /// A warning when the memory type is [`UNKNOWN_MEMORY_TYPE`], naming it and
    /// the `--memory-type` / [`MEMORY_TYPE_ENV`] override.
    pub fn warning(&self) -> Option<String> {
        (self.memory_type == UNKNOWN_MEMORY_TYPE).then(|| {
            format!(
                "memory type is {UNKNOWN_MEMORY_TYPE}: no EDAC DIMM types are readable (as in a VM); \
                 set --memory-type or {MEMORY_TYPE_ENV} (for example Registered-DDR4) so \
                 calibrations of different memory are not stored under one key"
            )
        })
    }
}

/// Lowercase ASCII letters and digits, every other run of characters one
/// dash, none at either end: `Intel Xeon E5-2683 v3` is `intel-xeon-e5-2683-v3`.
fn slug(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// `/sys/devices/system/edac/mc/mc*/dimm*/dimm_mem_type`, one per DIMM; empty
/// without EDAC.
fn edac_dimm_memory_types() -> Vec<String> {
    let entries = |path: &std::path::Path, prefix: &str| -> Vec<std::path::PathBuf> {
        let mut paths: Vec<_> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        paths
    };
    entries(std::path::Path::new("/sys/devices/system/edac/mc"), "mc")
        .iter()
        .flat_map(|controller| entries(controller, "dimm"))
        .filter_map(|dimm| std::fs::read_to_string(dimm.join("dimm_mem_type")).ok())
        .map(|memory_type| memory_type.trim().to_string())
        .collect()
}

/// A machine description that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MachineError {
    /// `/proc/cpuinfo` or `/proc/meminfo` lacks a field, such as `model name`.
    #[error("{file} has no '{field}' field")]
    MissingField { file: String, field: String },
    /// A file could not be read.
    #[error("cannot read {path}: {message}")]
    Read { path: String, message: String },
}

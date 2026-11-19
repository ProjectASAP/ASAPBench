//! CPU and memory of an external server that runs in its own cgroup v2
//! group, such as a docker container's
//! `/sys/fs/cgroup/system.slice/docker-<id>.scope`.
//!
//! The group's files are read either from a local directory or, when the
//! server runs on another host than the harness, over SSH with `cat` on the
//! server host. Only [`CGROUP_FILES`] and the host's `/proc/stat`
//! ([`PROC_STAT`]) are read.
//!
//! Example: `cpu.stat` with `usage_usec 2500` reads 2,500,000 ns; a group
//! whose `nr_throttled` grows during a step was throttled by a CPU quota,
//! and that step is not a sample.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// The group files the harness reads: CPU, current and peak memory, memory
/// breakdown, and the cores the group may run on.
pub const CGROUP_FILES: [&str; 5] = [
    "cpu.stat",
    "memory.current",
    "memory.peak",
    "memory.stat",
    "cpuset.cpus.effective",
];

/// The name under which [`CgroupSource::read`] reads the server host's
/// `/proc/stat`, so the harness can check the server's cores are idle
/// around a step.
pub const PROC_STAT: &str = "proc-stat";

/// Where a server's cgroup v2 files are read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CgroupSource {
    /// The group's directory on this host.
    Local(PathBuf),
    /// The group's directory on the server host, read over SSH. Reads to one
    /// host share one connection, so a read costs one round trip.
    Remote {
        /// The SSH client, normally `ssh`.
        ssh_program: String,
        /// The SSH destination, such as `bench@10.0.0.2`.
        host: String,
        /// The group's directory on that host, such as
        /// `/sys/fs/cgroup/system.slice/docker-<id>.scope`.
        cgroup_directory: PathBuf,
    },
}

impl CgroupSource {
    /// The current text of one of [`CGROUP_FILES`], or of the server
    /// host's `/proc/stat` for [`PROC_STAT`].
    pub fn read(&self, file: &str) -> Result<String, CgroupError> {
        if !CGROUP_FILES.contains(&file) && file != PROC_STAT {
            return Err(CgroupError::NotServed(file.into()));
        }
        match self {
            CgroupSource::Local(dir) => {
                let path = if file == PROC_STAT {
                    PathBuf::from("/proc/stat")
                } else {
                    dir.join(file)
                };
                std::fs::read_to_string(&path).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        CgroupError::NotFound(path.display().to_string())
                    } else {
                        CgroupError::Read {
                            file: path.display().to_string(),
                            message: error.to_string(),
                        }
                    }
                })
            }
            CgroupSource::Remote {
                ssh_program,
                host,
                cgroup_directory,
            } => {
                let path = if file == PROC_STAT {
                    PathBuf::from("/proc/stat")
                } else {
                    cgroup_directory.join(file)
                };
                ssh_read(ssh_program, host, &path)
            }
        }
    }

    /// `cpu.stat` read once.
    pub fn cpu(&self) -> Result<CgroupCpu, CgroupError> {
        CgroupCpu::parse(&self.read("cpu.stat")?)
    }

    /// The memory files read once; `memory.peak` is absent before Linux 5.19.
    pub fn memory(&self) -> Result<CgroupMemory, CgroupError> {
        let current_bytes = parse_number("memory.current", self.read("memory.current")?.trim())?;
        let lifetime_peak_bytes = match self.read("memory.peak") {
            Ok(text) => Some(parse_number("memory.peak", text.trim())?),
            Err(CgroupError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let stat = self.read("memory.stat")?;
        Ok(CgroupMemory {
            current_bytes,
            anon_bytes: stat_field("memory.stat", &stat, "anon")?,
            file_bytes: stat_field("memory.stat", &stat, "file")?,
            lifetime_peak_bytes,
        })
    }
}

/// The CPU counters of `cpu.stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgroupCpu {
    /// `usage_usec` in ns: CPU of every thread and child of the group.
    pub usage_ns: u64,
    /// `nr_throttled`: periods a CPU quota stopped the group (0 without a
    /// `--cpus` quota; absent when the cpu controller is off, read as 0).
    pub nr_throttled: u64,
}

impl CgroupCpu {
    pub fn parse(cpu_stat: &str) -> Result<CgroupCpu, CgroupError> {
        let usage_usec = stat_field("cpu.stat", cpu_stat, "usage_usec")?;
        let nr_throttled = match stat_field("cpu.stat", cpu_stat, "nr_throttled") {
            Ok(value) => value,
            Err(CgroupError::MissingField { .. }) => 0,
            Err(error) => return Err(error),
        };
        Ok(CgroupCpu {
            usage_ns: usage_usec * 1_000,
            nr_throttled,
        })
    }
}

/// A group's memory, read at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupMemory {
    /// `memory.current`: every page the group holds, page cache included.
    pub current_bytes: u64,
    /// `memory.stat` `anon`: heap and stacks, without page cache.
    pub anon_bytes: u64,
    /// `memory.stat` `file`: page cache.
    pub file_bytes: u64,
    /// `memory.peak`: the most `memory.current` has been since the group
    /// started. Before Linux 6.12 it cannot be reset, so it spans the
    /// container's whole life, every earlier arm included. `None` before 5.19.
    pub lifetime_peak_bytes: Option<u64>,
}

fn parse_number(file: &str, text: &str) -> Result<u64, CgroupError> {
    text.parse().map_err(|_| CgroupError::Parse {
        file: file.into(),
        message: format!("'{text}' is not a number"),
    })
}

/// The value of the `<field> <value>` line of a flat-keyed cgroup file.
fn stat_field(file: &str, text: &str, field: &str) -> Result<u64, CgroupError> {
    let value = text
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == field).then_some(value)
        })
        .ok_or_else(|| CgroupError::MissingField {
            file: file.into(),
            field: field.into(),
        })?;
    parse_number(file, value.trim())
}

/// Options of every SSH read: never prompt for a password; share one
/// connection per host, kept open 60 s after the last read; give up on a
/// host that does not answer within 5 s, and on a connection that stops
/// answering within about 15 s. The control socket lives in `/tmp` because
/// macOS limits a socket path to 104 bytes.
const SSH_OPTIONS: [&str; 12] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ControlMaster=auto",
    "-o",
    "ControlPath=/tmp/aqpbm-ssh-%C",
    "-o",
    "ControlPersist=60",
    "-o",
    "ConnectTimeout=5",
    "-o",
    "ServerAliveInterval=5",
];

/// The text of `path` on `host`: `<ssh_program> <SSH_OPTIONS> <host> cat -- '<path>'`.
/// Exit status 255 is SSH's own failure (no connection, refused key); any
/// other failure is `cat`'s, and a missing file is [`CgroupError::NotFound`].
fn ssh_read(ssh_program: &str, host: &str, path: &Path) -> Result<String, CgroupError> {
    let shown = format!("{host}:{}", path.display());
    let output = Command::new(ssh_program)
        .args(SSH_OPTIONS)
        .arg(host)
        .arg(format!("cat -- {}", shell_quoted(&path.to_string_lossy())))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| CgroupError::Remote {
            host: host.into(),
            message: format!("cannot start {ssh_program}: {error}"),
        })?;
    if output.status.success() {
        return String::from_utf8(output.stdout).map_err(|_| CgroupError::Parse {
            file: shown,
            message: "not UTF-8".into(),
        });
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match output.status.code() {
        Some(255) | None => Err(CgroupError::Remote {
            host: host.into(),
            message: stderr,
        }),
        // Such as `memory.peak` before Linux 5.19.
        _ if stderr.contains("No such file or directory") => Err(CgroupError::NotFound(shown)),
        _ => Err(CgroupError::Read {
            file: shown,
            message: stderr,
        }),
    }
}

/// `text` in single quotes for the server host's shell, each `'` as `'\''`.
fn shell_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A cgroup file that cannot be read or parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CgroupError {
    #[error("cannot read {file}: {message}")]
    Read { file: String, message: String },
    #[error("{file} has no '{field}' line")]
    MissingField { file: String, field: String },
    #[error("cannot parse {file}: {message}")]
    Parse { file: String, message: String },
    #[error("SSH to {host}: {message}")]
    Remote { host: String, message: String },
    /// The file does not exist, such as `memory.peak` before Linux 5.19.
    #[error("{0} does not exist")]
    NotFound(String),
    #[error("{0} is not one of the cgroup files the harness reads")]
    NotServed(String),
}

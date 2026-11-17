//! CPU and memory of an external server that runs in its own cgroup v2
//! group, such as a docker container's
//! `/sys/fs/cgroup/system.slice/docker-<id>.scope`.
//!
//! The group's files are read either from a local directory or, when the
//! server runs on another host than the harness, through a small agent on
//! the server host ([`serve_agent`]) that answers `GET /<file>` with the
//! file's current text. Only the four files below are served.
//!
//! Example: `cpu.stat` with `usage_usec 2500` reads 2,500,000 ns; a group
//! whose `nr_throttled` grows during a step was throttled by a CPU quota,
//! and that step is not a sample.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The files the agent serves: CPU, current and peak memory, memory breakdown.
pub const CGROUP_FILES: [&str; 4] = ["cpu.stat", "memory.current", "memory.peak", "memory.stat"];

/// Where a server's cgroup v2 files are read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CgroupSource {
    /// The group's directory on this host.
    Local(PathBuf),
    /// An agent on the server host, such as `http://10.0.0.2:9101`.
    Agent { url: String },
}

impl CgroupSource {
    /// The current text of one of [`CGROUP_FILES`].
    pub fn read(&self, file: &str) -> Result<String, CgroupError> {
        if !CGROUP_FILES.contains(&file) {
            return Err(CgroupError::NotServed(file.into()));
        }
        match self {
            CgroupSource::Local(dir) => {
                let path = dir.join(file);
                std::fs::read_to_string(&path).map_err(|error| CgroupError::Read {
                    file: path.display().to_string(),
                    message: error.to_string(),
                })
            }
            CgroupSource::Agent { url } => http_get(url, file),
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
            Err(CgroupError::Read { .. }) => None,
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

/// `GET <url>/<file>` over HTTP/1.0; the body on status 200.
fn http_get(url: &str, file: &str) -> Result<String, CgroupError> {
    let failed = |message: String| CgroupError::Agent {
        url: format!("{url}/{file}"),
        message,
    };
    let address = url
        .strip_prefix("http://")
        .ok_or_else(|| failed("the agent url must start with http://".into()))?
        .trim_end_matches('/');
    let mut stream = TcpStream::connect(address).map_err(|error| failed(error.to_string()))?;
    write!(stream, "GET /{file} HTTP/1.0\r\nHost: {address}\r\n\r\n")
        .map_err(|error| failed(error.to_string()))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| failed(error.to_string()))?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| failed("no header end in the response".into()))?;
    let status = head.lines().next().unwrap_or("");
    match status.split_whitespace().nth(1) {
        Some("200") => {}
        // The client asks only for served names, so a 404 is a missing file,
        // such as `memory.peak` before Linux 5.19.
        Some("404") => {
            return Err(CgroupError::Read {
                file: format!("{url}/{file}"),
                message: "not found on the agent".into(),
            })
        }
        _ => return Err(failed(format!("status '{status}'"))),
    }
    Ok(body.to_string())
}

/// Whether a process whose `/proc/self/cgroup` reads `proc_self_cgroup`
/// runs inside the group at `served_dir` under the cgroup v2 mount
/// `cgroup_root`. The agent refuses to serve its own group: its reads would
/// be charged to the server it measures.
/// Example: `0::/system.slice/docker-1a2b.scope` with root `/sys/fs/cgroup`
/// is inside `/sys/fs/cgroup/system.slice/docker-1a2b.scope`.
pub fn agent_in_served_group(proc_self_cgroup: &str, cgroup_root: &Path, served_dir: &Path) -> bool {
    let Some(own) = proc_self_cgroup.lines().find_map(|line| line.strip_prefix("0::")) else {
        return false;
    };
    let own_dir = cgroup_root.join(own.trim().trim_start_matches('/'));
    own_dir.starts_with(served_dir)
}

/// Serves `GET /<file>` for each of [`CGROUP_FILES`] in `dir`, read afresh
/// per request; every other path is 404. Runs until the listener fails.
/// One request per connection (HTTP/1.0), answered on the accepting thread.
pub fn serve_agent(listener: TcpListener, dir: &Path) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        // A broken client never stops the agent.
        let _ = answer(&mut stream, dir);
    }
    Ok(())
}

fn answer(stream: &mut TcpStream, dir: &Path) -> std::io::Result<()> {
    let mut reader = BufReader::new(&*stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Read the headers up to the blank line: closing a socket with unread
    // input resets the connection, and the client loses the answer.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 0 && header != "\r\n" && header != "\n" {
        header.clear();
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    let file = path.strip_prefix('/').unwrap_or("");
    let body = if CGROUP_FILES.contains(&file) {
        std::fs::read_to_string(dir.join(file)).ok()
    } else {
        None
    };
    match body {
        Some(body) => write!(
            stream,
            "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
        None => write!(stream, "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
    }
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
    #[error("cgroup agent {url}: {message}")]
    Agent { url: String, message: String },
    #[error("{0} is not one of the served cgroup files")]
    NotServed(String),
}

//! The run directory:
//! `<root>/runs/<machine_key>/<YYYYMMDDTHHMMSSZ>-<subcommand>-<label>/` with
//! `run.json`, `workloads/`, `data/`, `plans/`, `results/` and `status.log`.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::harness::data::DataRecord;
use crate::harness::protocol::{JobMark, JobRecord};
use crate::harness::records::{ArmResult, RunRecord};
use crate::harness::write_throughput::WriteThroughputResult;

/// A created run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDirectory {
    /// Private: only [`RunDirectory::create`] makes one, so every `RunDirectory` names a
    /// directory this run created.
    path: PathBuf,
}

impl RunDirectory {
    /// Creates the directory and its subdirectories. Refuses an existing
    /// directory and a label or subcommand outside `[a-z0-9-]`.
    pub fn create(
        root: &Path,
        machine_key: &str,
        utc: DateTime<Utc>,
        subcommand: &str,
        label: &str,
    ) -> Result<Self, RunDirectoryError> {
        check_name("machine key", machine_key, MACHINE_KEY_CHARSET, |c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
        })?;
        for (what, name) in [("subcommand", subcommand), ("label", label)] {
            check_name(what, name, SLUG_CHARSET, |c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
            })?;
        }
        let path = root.join("runs").join(machine_key).join(format!(
            "{}-{subcommand}-{label}",
            utc.format("%Y%m%dT%H%M%SZ")
        ));
        RunDirectory::create_at(&path)
    }

    /// Creates the run directory at `path` and its subdirectories, for a
    /// run whose engine runs are separate processes over one directory
    /// (`benchmark-external --run-directory`). Refuses an existing `path`.
    pub fn create_at(path: &Path) -> Result<Self, RunDirectoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
        }
        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(RunDirectoryError::RunDirectoryExists(path.to_path_buf()));
            }
            Err(source) => return Err(io(path, source)),
        }
        for sub in RUN_SUBDIRECTORIES {
            let sub = path.join(sub);
            std::fs::create_dir(&sub).map_err(|source| io(&sub, source))?;
        }
        Ok(RunDirectory {
            path: path.to_path_buf(),
        })
    }

    /// Opens a run directory another process created: every subdirectory
    /// must exist.
    pub fn open(path: &Path) -> Result<Self, RunDirectoryError> {
        for sub in RUN_SUBDIRECTORIES {
            if !path.join(sub).is_dir() {
                return Err(RunDirectoryError::NotARunDirectory {
                    path: path.to_path_buf(),
                    missing: sub.into(),
                });
            }
        }
        Ok(RunDirectory {
            path: path.to_path_buf(),
        })
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `run.json`.
    pub fn write_run(&self, run: &RunRecord) -> Result<(), RunDirectoryError> {
        write_json(&self.path.join("run.json"), run)
    }

    /// Writes `data/<metric>.datagen.json` and returns its path.
    pub fn write_data(&self, data: &DataRecord) -> Result<PathBuf, RunDirectoryError> {
        check_name("metric", &data.metric, METRIC_CHARSET, |c| {
            c.is_ascii_alphanumeric() || c == '_' || c == ':'
        })?;
        let path = self
            .path
            .join("data")
            .join(format!("{}.datagen.json", data.metric));
        write_json(&path, data)?;
        Ok(path)
    }

    /// Writes `results/<engine>/<workload>__<arm>.json` and returns its path.
    /// A workload or arm name containing `__` is refused: the file name would
    /// be ambiguous.
    pub fn write_arm(&self, result: &ArmResult) -> Result<PathBuf, RunDirectoryError> {
        check_result_name("engine", &result.engine, false)?;
        check_result_name("workload", &result.workload, true)?;
        check_result_name("arm", &result.arm, true)?;
        let dir = self.path.join("results").join(&result.engine);
        std::fs::create_dir_all(&dir).map_err(|source| io(&dir, source))?;
        let path = dir.join(format!("{}__{}.json", result.workload, result.arm));
        write_json(&path, result)?;
        Ok(path)
    }

    pub fn append_write_throughput(
        &self,
        result: &WriteThroughputResult,
    ) -> Result<PathBuf, RunDirectoryError> {
        check_result_name("engine", &result.engine.id, false)?;
        let dir = self.path.join("results").join(&result.engine.id);
        std::fs::create_dir_all(&dir).map_err(|source| io(&dir, source))?;
        let path = dir.join("write-throughput.jsonl");
        let mut line = serde_json::to_string(result).map_err(|source| RunDirectoryError::Json {
            path: path.clone(),
            source,
        })?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(|source| io(&path, source))?;
        Ok(path)
    }

    /// Appends one line to `status.log`: job name, core, busy before and
    /// after, load average and marks (such as `busy-after 0.35`).
    /// Example: `ingest-0 core 3 busy-before 0.02 busy-after 0.03 loadavg 0.5 0.75`;
    /// an unreadable load average is written `unknown`.
    pub fn append_status(&self, job: &str, record: &JobRecord) -> Result<(), RunDirectoryError> {
        let mut line = format!(
            "{job} core {} busy-before {:.2} busy-after {:.2} loadavg {} {}",
            record.core,
            record.busy_before,
            record.busy_after,
            loadavg_text(record.loadavg[0]),
            loadavg_text(record.loadavg[1])
        );
        for mark in &record.marks {
            match mark {
                JobMark::BusyAfter { busy_fraction } => {
                    line.push_str(&format!(" marked busy-after {busy_fraction:.2}"))
                }
            }
        }
        line.push('\n');
        let path = self.path.join("status.log");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(|source| io(&path, source))
    }
}

/// Refuses a name that cannot be part of a result file name: outside
/// `[a-zA-Z0-9._-]`, starting with `.`, or, for a workload or arm
/// (`separated`), containing `__`, which separates them in
/// `<workload>__<arm>.json`.
pub(crate) fn check_result_name(
    what: &str,
    name: &str,
    separated: bool,
) -> Result<(), RunDirectoryError> {
    check_name(what, name, FILE_NAME_CHARSET, |c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
    })?;
    if name.starts_with('.') {
        return Err(RunDirectoryError::BadName {
            what: what.into(),
            name: name.into(),
            allowed: FILE_NAME_CHARSET,
        });
    }
    if separated && name.contains("__") {
        return Err(RunDirectoryError::AmbiguousName {
            what: what.into(),
            name: name.into(),
        });
    }
    Ok(())
}

/// The subdirectories of every run directory.
const RUN_SUBDIRECTORIES: [&str; 4] = ["workloads", "data", "plans", "results"];

fn loadavg_text(loadavg: Option<f64>) -> String {
    loadavg.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

/// The characters of a subcommand or label.
const SLUG_CHARSET: &str = "[a-z0-9-]";
/// The characters of a machine key: a slug, plus the architecture's `_`.
const MACHINE_KEY_CHARSET: &str = "[a-z0-9_-]";
/// The characters of a metric name, as Prometheus allows them.
const METRIC_CHARSET: &str = "[a-zA-Z0-9_:]";
/// The characters of an engine, workload or arm name in a result file name.
const FILE_NAME_CHARSET: &str = "[a-zA-Z0-9._-]";

/// Refuses an empty `name` or one with a character `allowed_char` rejects.
fn check_name(
    what: &str,
    name: &str,
    allowed: &'static str,
    allowed_char: impl Fn(char) -> bool,
) -> Result<(), RunDirectoryError> {
    if name.is_empty() || !name.chars().all(allowed_char) {
        return Err(RunDirectoryError::BadName {
            what: what.into(),
            name: name.into(),
            allowed,
        });
    }
    Ok(())
}

fn io(path: &Path, source: std::io::Error) -> RunDirectoryError {
    RunDirectoryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Writes `value` as pretty JSON with a final newline to a new file; an
/// existing file is refused (`create_new`), so a result is never overwritten.
pub(crate) fn write_json<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), RunDirectoryError> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|source| RunDirectoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    text.push('\n');
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|source| io(path, source))
}

/// A run directory that cannot be created or written.
#[derive(Debug, thiserror::Error)]
pub enum RunDirectoryError {
    /// The directory exists; runs are never overwritten.
    #[error("run directory {0} exists; runs are never overwritten")]
    RunDirectoryExists(PathBuf),
    /// A name outside its character set, such as a label outside `[a-z0-9-]`.
    #[error("{what} '{name}' must match {allowed}+")]
    BadName {
        what: String,
        name: String,
        allowed: &'static str,
    },
    /// A workload or arm name that would make a result file name ambiguous.
    #[error(
        "{what} '{name}' contains '__', which separates workload and arm in result file names"
    )]
    AmbiguousName { what: String, name: String },
    /// A directory without one of the run subdirectories.
    #[error("{path} is not a run directory: it has no {missing}/")]
    NotARunDirectory { path: PathBuf, missing: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

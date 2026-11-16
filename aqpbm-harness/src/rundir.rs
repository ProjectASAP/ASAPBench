//! The run directory:
//! `<root>/runs/<machine_key>/<YYYYMMDDTHHMMSSZ>-<subcommand>-<label>/` with
//! `run.json`, `workloads/`, `data/`, `plans/`, `results/` and `status.log`.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::data::DataRecord;
use crate::protocol::{JobMark, JobRecord};
use crate::records::{ArmResult, RunRecord};

/// A created run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir {
    /// Private: only [`RunDir::create`] makes one, so every `RunDir` names a
    /// directory this run created.
    path: PathBuf,
}

impl RunDir {
    /// Creates the directory and its subdirectories. Refuses an existing
    /// directory and a label or subcommand outside `[a-z0-9-]`.
    pub fn create(
        root: &Path,
        machine_key: &str,
        utc: DateTime<Utc>,
        subcommand: &str,
        label: &str,
    ) -> Result<Self, RunDirError> {
        check_name("machine key", machine_key, MACHINE_KEY_CHARSET, |c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
        })?;
        for (what, name) in [("subcommand", subcommand), ("label", label)] {
            check_name(what, name, SLUG_CHARSET, |c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
            })?;
        }
        let parent = root.join("runs").join(machine_key);
        std::fs::create_dir_all(&parent).map_err(|source| io(&parent, source))?;
        let path = parent.join(format!(
            "{}-{subcommand}-{label}",
            utc.format("%Y%m%dT%H%M%SZ")
        ));
        // `create_dir` fails on an existing directory, so two runs never share one.
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(RunDirError::RunDirExists(path));
            }
            Err(source) => return Err(io(&path, source)),
        }
        for sub in ["workloads", "data", "plans", "results"] {
            let sub = path.join(sub);
            std::fs::create_dir(&sub).map_err(|source| io(&sub, source))?;
        }
        Ok(RunDir { path })
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `run.json`.
    pub fn write_run(&self, run: &RunRecord) -> Result<(), RunDirError> {
        write_json(&self.path.join("run.json"), run)
    }

    /// Writes `data/<metric>.datagen.json` and returns its path.
    pub fn write_data(&self, data: &DataRecord) -> Result<PathBuf, RunDirError> {
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
    pub fn write_arm(&self, result: &ArmResult) -> Result<PathBuf, RunDirError> {
        for (what, name) in [
            ("engine", &result.engine),
            ("workload", &result.workload),
            ("arm", &result.arm),
        ] {
            check_name(what, name, FILE_NAME_CHARSET, |c| {
                c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
            })?;
            if name.starts_with('.') {
                return Err(RunDirError::BadName {
                    what: what.into(),
                    name: name.clone(),
                    allowed: FILE_NAME_CHARSET,
                });
            }
        }
        for (what, name) in [("workload", &result.workload), ("arm", &result.arm)] {
            if name.contains("__") {
                return Err(RunDirError::AmbiguousName {
                    what: what.into(),
                    name: name.clone(),
                });
            }
        }
        let dir = self.path.join("results").join(&result.engine);
        std::fs::create_dir_all(&dir).map_err(|source| io(&dir, source))?;
        let path = dir.join(format!("{}__{}.json", result.workload, result.arm));
        write_json(&path, result)?;
        Ok(path)
    }

    /// Appends one line to `status.log`: job name, core, busy before and
    /// after, load average and marks (such as `busy-after 0.35`).
    /// Example: `ingest-0 core 3 busy-before 0.02 busy-after 0.03 loadavg 0.5 0.75`;
    /// an unreadable load average is written `unknown`.
    pub fn append_status(&self, job: &str, record: &JobRecord) -> Result<(), RunDirError> {
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
) -> Result<(), RunDirError> {
    if name.is_empty() || !name.chars().all(allowed_char) {
        return Err(RunDirError::BadName {
            what: what.into(),
            name: name.into(),
            allowed,
        });
    }
    Ok(())
}

fn io(path: &Path, source: std::io::Error) -> RunDirError {
    RunDirError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Writes `value` as pretty JSON with a final newline to a new file; an
/// existing file is refused (`create_new`), so a result is never overwritten.
fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), RunDirError> {
    let mut text = serde_json::to_string_pretty(value).map_err(|source| RunDirError::Json {
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
pub enum RunDirError {
    /// The directory exists; runs are never overwritten.
    #[error("run directory {0} exists; runs are never overwritten")]
    RunDirExists(PathBuf),
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

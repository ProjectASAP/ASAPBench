//! The run directory:
//! `<root>/runs/<machine_key>/<YYYYMMDDTHHMMSSZ>-<subcommand>-<label>/` with
//! `run.json`, `workloads/`, `data/`, `plans/`, `results/` and `status.log`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::data::DataRecord;
use crate::protocol::JobRecord;
use crate::records::{ArmResult, RunRecord};

/// A created run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir {
    pub path: PathBuf,
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
        let _ = (root, machine_key, utc, subcommand, label);
        todo!("implemented after test approval")
    }

    /// Writes `run.json`.
    pub fn write_run(&self, run: &RunRecord) -> Result<(), RunDirError> {
        let _ = run;
        todo!("implemented after test approval")
    }

    /// Writes `data/<metric>.datagen.json` and returns its path.
    pub fn write_data(&self, data: &DataRecord) -> Result<PathBuf, RunDirError> {
        let _ = data;
        todo!("implemented after test approval")
    }

    /// Writes `results/<engine>/<workload>__<arm>.json` and returns its path.
    /// A workload or arm name containing `__` is refused: the file name would
    /// be ambiguous.
    pub fn write_arm(&self, result: &ArmResult) -> Result<PathBuf, RunDirError> {
        let _ = result;
        todo!("implemented after test approval")
    }

    /// Appends one line to `status.log`: job name, core, busy before and
    /// after, load average and marks (such as `busy-after 0.35`).
    pub fn append_status(&self, job: &str, record: &JobRecord) -> Result<(), RunDirError> {
        let _ = (job, record);
        todo!("implemented after test approval")
    }
}

/// A run directory that cannot be created or written.
#[derive(Debug, thiserror::Error)]
pub enum RunDirError {
    /// The directory exists; runs are never overwritten.
    #[error("run directory {0} exists; runs are never overwritten")]
    RunDirExists(PathBuf),
    /// A name outside `[a-z0-9-]`.
    #[error("{what} '{name}' must match [a-z0-9-]+")]
    BadName { what: String, name: String },
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

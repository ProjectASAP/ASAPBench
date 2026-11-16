//! Data: samples, their JSONL form, the `data/<metric>.datagen.json` record
//! a run keeps instead of the samples, and the refresh chunks an engine
//! ingests.
//!
//! Every engine reads the same samples: `aqpbm-datagen`'s
//! `TimeSeriesDescription` on the left-open grid `(start, start + duration]`.

use std::collections::BTreeMap;

use aqpbm_datagen::TimeSeriesDescription;
use serde::{Deserialize, Serialize};

/// A series' labels, `__name__` included, such as
/// `{__name__: "m", job: "job_0"}`.
pub type SeriesLabels = BTreeMap<String, String>;

/// One sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub labels: SeriesLabels,
    pub ts_ms: i64,
    pub value: f64,
}

/// Reads one sample per line, `{"labels": {...}, "ts_ms": 1, "value": 2.0}`;
/// blank lines are skipped. A bad line is an error naming its 0-based index.
pub fn read_samples_jsonl(text: &str) -> Result<Vec<Sample>, SampleReadError> {
    let _ = text;
    todo!("implemented after test approval")
}

/// A JSONL line that is not a sample.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("samples line {line}: {message}")]
pub struct SampleReadError {
    /// 0-based line index.
    pub line: usize,
    pub message: String,
}

/// `data/<metric>.datagen.json`: the generator description and seed of one
/// metric, never the samples. Regenerating from it gives bit-identical samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataRecord {
    /// Written as the `__name__` label of every series.
    pub metric: String,
    pub description: TimeSeriesDescription,
}

/// The samples of one refresh: every sample in `(end_ms - refresh, end_ms]`,
/// time-major.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleChunk {
    pub end_ms: i64,
    pub samples: Vec<Sample>,
}

/// The record's samples cut into refresh chunks `(end - refresh_ms, end]`,
/// with ends `start + refresh_ms`, `start + 2 refresh_ms`, ... A sample on a
/// chunk end belongs to the chunk it ends. A generator error ends the chunks
/// with that error.
pub fn refresh_chunks(
    record: &DataRecord,
    refresh_ms: u64,
) -> Result<impl Iterator<Item = Result<SampleChunk, DataError>>, DataError> {
    let _ = (record, refresh_ms);
    todo!("implemented after test approval");
    #[allow(unreachable_code)]
    Ok(std::iter::empty())
}

/// Data that cannot be generated or cut.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataError {
    /// The description is invalid, or a counter passed 2^53 while generating.
    #[error("data of metric {metric}: {message}")]
    Generator { metric: String, message: String },
    /// A refresh interval of 0.
    #[error("refresh_ms must be > 0")]
    ZeroRefresh,
}

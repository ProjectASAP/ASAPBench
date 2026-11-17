//! Data: samples, their JSONL form, the `data/<metric>.datagen.json` record
//! a run keeps instead of the samples, and the refresh chunks an engine
//! ingests.
//!
//! Every engine reads the same samples: `aqpbm-datagen`'s
//! `TimeSeriesDescription` on the left-open grid `(start, start + duration]`.

use std::collections::BTreeMap;
use std::sync::Arc;

use aqpbm_datagen::{OwnedTimeSeriesSamples, TimeSeriesDescription};
use serde::{Deserialize, Serialize};

/// A series' labels, `__name__` included, such as
/// `{__name__: "m", job: "job_0"}`.
pub type SeriesLabels = BTreeMap<String, String>;

/// One sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    /// Shared by every sample of the series: generating a sample clones a
    /// pointer, not the label map.
    pub labels: Arc<SeriesLabels>,
    pub ts_ms: i64,
    pub value: f64,
}

/// Reads one sample per line, `{"labels": {...}, "ts_ms": 1, "value": 2.0}`;
/// blank lines are skipped. A bad line is an error naming its 0-based index.
pub fn read_samples_jsonl(text: &str) -> Result<Vec<Sample>, SampleReadError> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line, text)| {
            serde_json::from_str(text).map_err(|error| SampleReadError {
                line,
                message: error.to_string(),
            })
        })
        .collect()
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
/// Example: 2 series every 10 s over 60 s from `start`, refresh 30 s: a chunk
/// ending at `start + 30 s` with the samples at 10, 20 and 30 s, and one
/// ending at `start + 60 s` with those at 40, 50 and 60 s. When the
/// duration is not a multiple of the refresh, the last chunk ends on the
/// next refresh end after it.
pub fn refresh_chunks(
    record: &DataRecord,
    refresh_ms: u64,
) -> Result<impl Iterator<Item = Result<SampleChunk, DataError>>, DataError> {
    if refresh_ms == 0 {
        return Err(DataError::ZeroRefresh);
    }
    let generated = record
        .description
        .generate()
        .map_err(|error| DataError::Generator {
            metric: record.metric.clone(),
            message: error.to_string(),
        })?;
    let labels = generated
        .labels
        .iter()
        .map(|pairs| {
            let mut labels: SeriesLabels = pairs.iter().cloned().collect();
            labels.insert("__name__".into(), record.metric.clone());
            Arc::new(labels)
        })
        .collect();
    let description = &record.description;
    let chunk_count = description.duration_ms.div_ceil(refresh_ms);
    Ok(RefreshChunks {
        metric: record.metric.clone(),
        samples: generated.into_samples(),
        labels,
        start_ms: description.start_ms,
        refresh_ms,
        chunk_count,
        next_chunk: 0,
        pending: None,
        finished: false,
    })
}

/// The iterator of [`refresh_chunks`]; it owns the generated series and
/// draws samples as the chunks are taken.
struct RefreshChunks {
    metric: String,
    samples: OwnedTimeSeriesSamples,
    /// Series `i`'s labels, `__name__` included.
    labels: Vec<Arc<SeriesLabels>>,
    start_ms: i64,
    refresh_ms: u64,
    chunk_count: u64,
    /// 0-based index of the next chunk; it ends at `start + (index + 1) refresh`.
    next_chunk: u64,
    /// A sample drawn past the previous chunk's end.
    pending: Option<Sample>,
    /// After the last chunk or an error.
    finished: bool,
}

impl Iterator for RefreshChunks {
    type Item = Result<SampleChunk, DataError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.next_chunk >= self.chunk_count {
            return None;
        }
        self.next_chunk += 1;
        let end_ms = self.start_ms + (self.next_chunk * self.refresh_ms) as i64;
        let mut samples = Vec::new();
        if let Some(sample) = self.pending.take() {
            if sample.ts_ms > end_ms {
                self.pending = Some(sample);
                return Some(Ok(SampleChunk { end_ms, samples }));
            }
            samples.push(sample);
        }
        for item in self.samples.by_ref() {
            let (series, ts_ms, value) = match item {
                Ok(sample) => sample,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(DataError::Generator {
                        metric: self.metric.clone(),
                        message: error.to_string(),
                    }));
                }
            };
            let sample = Sample {
                labels: Arc::clone(&self.labels[series]),
                ts_ms,
                value,
            };
            if ts_ms > end_ms {
                self.pending = Some(sample);
                break;
            }
            samples.push(sample);
        }
        Some(Ok(SampleChunk { end_ms, samples }))
    }
}

/// The refresh chunks of several metrics merged by end: one chunk per
/// distinct end, ascending, holding every record's samples of that refresh
/// (record by record, each time-major). The first generator error ends the
/// chunks with that error. Example: metrics `a` and `b` on one grid give one
/// chunk per refresh with the samples of both.
pub fn workload_chunks(
    records: &[DataRecord],
    refresh_ms: u64,
) -> Result<impl Iterator<Item = Result<SampleChunk, DataError>>, DataError> {
    let mut per_record = Vec::with_capacity(records.len());
    for record in records {
        let chunks: Box<dyn Iterator<Item = Result<SampleChunk, DataError>>> =
            Box::new(refresh_chunks(record, refresh_ms)?);
        per_record.push(chunks.peekable());
    }
    let mut finished = false;
    Ok(std::iter::from_fn(move || {
        if finished {
            return None;
        }
        let mut end_ms = None;
        for chunks in &mut per_record {
            match chunks.peek() {
                None => {}
                Some(Err(_)) => {
                    finished = true;
                    return chunks.next();
                }
                Some(Ok(chunk)) => {
                    end_ms = Some(end_ms.map_or(chunk.end_ms, |e: i64| e.min(chunk.end_ms)))
                }
            }
        }
        let end_ms = end_ms?;
        let mut samples = Vec::new();
        for chunks in &mut per_record {
            if let Some(Ok(chunk)) =
                chunks.next_if(|item| matches!(item, Ok(c) if c.end_ms == end_ms))
            {
                samples.extend(chunk.samples);
            }
        }
        Some(Ok(SampleChunk { end_ms, samples }))
    }))
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

//! Per-source statistics over samples, like a database's `ANALYZE`: series
//! count, sample interval, distinct label values, value magnitude and sign.
//! Statistics that depend on a query's window or grouping stay with the
//! engine that plans the query.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::data::Sample;

/// The smallest and largest non-zero absolute value seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedMagnitude {
    pub min_abs: f64,
    pub max_abs: f64,
}

/// The sign of every value seen, read from the sign bit: `-0.0` is negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservedSign {
    NonNegative,
    Negative,
    Mixed,
}

/// Statistics of one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataStatistics {
    /// Distinct label sets.
    pub series_count: u64,
    /// The median over series of each series' median gap between samples;
    /// `None` when no series has two samples.
    pub sample_interval_ms: Option<u64>,
    /// Per label name (not `__name__`), the distinct values; a series without
    /// the label counts as one more value.
    pub distinct_values_per_label: BTreeMap<String, u64>,
    /// Per label set asked for (names sorted), the distinct observed
    /// combinations; only sets whose labels all occur in the metric.
    pub distinct_values_per_label_set: Vec<(Vec<String>, u64)>,
    /// `None` when every value is zero.
    pub value_magnitude: Option<ObservedMagnitude>,
    pub value_sign: ObservedSign,
}

/// Statistics per metric (`__name__`) over `samples`. `label_sets` are the
/// groupings whose distinct combinations to count, such as `[["job", "instance"]]`.
pub fn data_statistics(
    samples: &[Sample],
    label_sets: &[Vec<String>],
) -> BTreeMap<String, DataStatistics> {
    let _ = (samples, label_sets);
    todo!("implemented after test approval")
}

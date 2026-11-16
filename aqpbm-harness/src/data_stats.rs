//! Per-source statistics over samples, like a database's `ANALYZE`: series
//! count, sample interval, distinct label values, value magnitude and sign.
//! Statistics that depend on a query's window or grouping stay with the
//! engine that plans the query.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::data::{Sample, SeriesLabels};

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
    /// The median over series of each series' median gap between samples
    /// (the lower median for an even count, so it is a measured gap);
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
    let mut metrics: BTreeMap<&str, MetricSamples> = BTreeMap::new();
    for sample in samples {
        let metric = sample
            .labels
            .get("__name__")
            .map(String::as_str)
            .unwrap_or("");
        let gathered = metrics.entry(metric).or_default();
        let mut labels = sample.labels.clone();
        labels.remove("__name__");
        gathered
            .timestamps_per_series
            .entry(labels)
            .or_default()
            .push(sample.ts_ms);
        gathered.values.push(sample.value);
    }
    metrics
        .into_iter()
        .map(|(metric, gathered)| {
            (
                metric.to_string(),
                metric_statistics(
                    &gathered.timestamps_per_series,
                    &gathered.values,
                    label_sets,
                ),
            )
        })
        .collect()
}

/// One metric's samples, gathered for its statistics.
#[derive(Default)]
struct MetricSamples {
    /// Per series (labels without `__name__`), its timestamps in ms.
    timestamps_per_series: BTreeMap<SeriesLabels, Vec<i64>>,
    /// Every value, in sample order.
    values: Vec<f64>,
}

/// The statistics of one metric's series and values.
fn metric_statistics(
    series: &BTreeMap<SeriesLabels, Vec<i64>>,
    values: &[f64],
    label_sets: &[Vec<String>],
) -> DataStatistics {
    let label_names: BTreeSet<&str> = series
        .keys()
        .flat_map(|labels| labels.keys().map(String::as_str))
        .collect();
    let distinct_values_per_label = label_names
        .iter()
        .map(|&name| {
            let values: BTreeSet<Option<&String>> =
                series.keys().map(|labels| labels.get(name)).collect();
            (name.to_string(), values.len() as u64)
        })
        .collect();
    let distinct_values_per_label_set = label_sets
        .iter()
        .filter(|set| set.iter().all(|name| label_names.contains(name.as_str())))
        .map(|set| {
            let mut names = set.clone();
            names.sort();
            names.dedup();
            let combinations: BTreeSet<Vec<Option<&String>>> = series
                .keys()
                .map(|labels| names.iter().map(|name| labels.get(name)).collect())
                .collect();
            (names, combinations.len() as u64)
        })
        .collect();

    let series_intervals: Vec<i64> = series
        .values()
        .filter_map(|timestamps| {
            let mut timestamps = timestamps.clone();
            timestamps.sort_unstable();
            let gaps: Vec<i64> = timestamps
                .windows(2)
                .map(|pair| pair[1] - pair[0])
                .collect();
            lower_median(gaps)
        })
        .collect();
    let sample_interval_ms = lower_median(series_intervals).map(|gap| gap as u64);

    let non_zero = values.iter().filter(|v| **v != 0.0 && !v.is_nan());
    let value_magnitude =
        non_zero
            .map(|v| v.abs())
            .fold(None, |range: Option<ObservedMagnitude>, abs| {
                Some(match range {
                    None => ObservedMagnitude {
                        min_abs: abs,
                        max_abs: abs,
                    },
                    Some(range) => ObservedMagnitude {
                        min_abs: range.min_abs.min(abs),
                        max_abs: range.max_abs.max(abs),
                    },
                })
            });
    let negative = values.iter().any(|v| v.is_sign_negative());
    let non_negative = values.iter().any(|v| v.is_sign_positive());
    let value_sign = match (negative, non_negative) {
        (true, true) => ObservedSign::Mixed,
        (true, false) => ObservedSign::Negative,
        (false, _) => ObservedSign::NonNegative,
    };

    DataStatistics {
        series_count: series.len() as u64,
        sample_interval_ms,
        distinct_values_per_label,
        distinct_values_per_label_set,
        value_magnitude,
        value_sign,
    }
}

/// The lower median, a measured value: of 1000, 1000, 1000, 2000 it is the
/// second, 1000. `None` for no values.
fn lower_median(mut values: Vec<i64>) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[(values.len() - 1) / 2])
}

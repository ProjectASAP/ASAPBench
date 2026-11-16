//! Per-source statistics over hand-made and generated samples.
//!
//! Source `m`:
//!
//! | Series | Samples (t ms : value) |
//! |---|---|
//! | A `{job=a,instance=1}` | 0:1, 1000:2, 2000:−3, 4000:0 |
//! | B `{job=a,instance=2}` | 0:5, 1000:10 |
//! | C `{job=b,instance=1}` | 500:0.5, 1500:0.25, 2500:100 |
//! | D `{job=b}` (no instance) | 0:7, 2000:7 |
//!
//! Source `lat`: two series, every value in [0.001, 3], one of them −0.0.

use std::collections::BTreeMap;

use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use aqpbm_harness::data::{refresh_chunks, DataRecord, Sample};
use aqpbm_harness::data_stats::{data_statistics, DataStatistics, ObservedMagnitude, ObservedSign};

/// One sample of metric `metric` with `labels`.
fn sample(metric: &str, labels: &[(&str, &str)], ts_ms: i64, value: f64) -> Sample {
    let mut all: BTreeMap<String, String> = labels
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    all.insert("__name__".into(), metric.into());
    Sample {
        labels: all.into(),
        ts_ms,
        value,
    }
}

fn fixture() -> Vec<Sample> {
    let a = [("job", "a"), ("instance", "1")];
    let b = [("job", "a"), ("instance", "2")];
    let c = [("job", "b"), ("instance", "1")];
    let d = [("job", "b")];
    let mut samples = vec![
        sample("m", &a, 0, 1.0),
        sample("m", &a, 1_000, 2.0),
        sample("m", &a, 2_000, -3.0),
        sample("m", &a, 4_000, 0.0),
        sample("m", &b, 0, 5.0),
        sample("m", &b, 1_000, 10.0),
        sample("m", &c, 500, 0.5),
        sample("m", &c, 1_500, 0.25),
        sample("m", &c, 2_500, 100.0),
        sample("m", &d, 0, 7.0),
        sample("m", &d, 2_000, 7.0),
    ];
    let x = [("host", "x")];
    let y = [("host", "y")];
    samples.extend([
        sample("lat", &x, 0, 0.001),
        sample("lat", &x, 1_000, 3.0),
        sample("lat", &y, 0, -0.0),
        sample("lat", &y, 1_000, 1.5),
    ]);
    samples
}

/// The statistics of `metric` over `samples`, no label sets asked for.
fn statistics_of(samples: &[Sample], metric: &str) -> DataStatistics {
    data_statistics(samples, &[])
        .remove(metric)
        .unwrap_or_else(|| panic!("no statistics for {metric}"))
}

fn statistics_m() -> DataStatistics {
    statistics_of(&fixture(), "m")
}

fn job_instance() -> Vec<Vec<String>> {
    vec![vec!["job".to_string(), "instance".to_string()]]
}

/// `m` has four distinct label sets (A, B, C, D).
/// Moved from ASAPPlanner #1332 `facts_series_count`.
#[test]
fn statistics_series_count() {
    assert_eq!(statistics_m().series_count, 4);
}

/// Gaps A 1000, 1000, 2000; B 1000; C 1000, 1000; D 2000. Per-series
/// medians 1000, 1000, 1000, 2000, whose median is 1000 (the mean gap,
/// 1286, is wrong). A second source separates the two medians: P has 10
/// samples 1 s apart, Q and R 2 samples 5 s apart; the pooled median gap
/// is 1000, the median of per-series medians 5000.
/// Moved from ASAPPlanner #1332 `facts_sample_interval_median_gap`.
#[test]
fn statistics_sample_interval_is_the_median_of_series_medians() {
    assert_eq!(statistics_m().sample_interval_ms, Some(1_000));
    let mut samples: Vec<Sample> = (0..10)
        .map(|i| sample("s", &[("id", "p")], i * 1_000, 1.0))
        .collect();
    for id in ["q", "r"] {
        samples.push(sample("s", &[("id", id)], 0, 1.0));
        samples.push(sample("s", &[("id", id)], 5_000, 1.0));
    }
    assert_eq!(statistics_of(&samples, "s").sample_interval_ms, Some(5_000));
}

/// `job` takes 2 values; `instance` 3 (1, 2, and missing on D); `__name__`
/// is not a label here.
/// Moved from ASAPPlanner #1332 `facts_distinct_per_label`.
#[test]
fn statistics_distinct_per_label_counts_missing_as_one() {
    let statistics = statistics_m();
    assert_eq!(statistics.distinct_values_per_label["job"], 2);
    assert_eq!(statistics.distinct_values_per_label["instance"], 3);
    assert!(!statistics
        .distinct_values_per_label
        .contains_key("__name__"));
}

/// The label set `[job, instance]` sees 4 observed combinations (a/1, a/2,
/// b/1, b/missing), not the product 6, reported with names sorted; `lat`
/// has neither label, so it gets no entry.
/// Moved from ASAPPlanner #1332 `facts_label_set_from_workload_by`.
#[test]
fn statistics_label_sets_are_the_observed_combinations() {
    let statistics = data_statistics(&fixture(), &job_instance());
    assert_eq!(
        statistics["m"].distinct_values_per_label_set,
        vec![(vec!["instance".to_string(), "job".to_string()], 4)]
    );
    assert!(statistics["lat"].distinct_values_per_label_set.is_empty());
}

/// Zeros are excluded: `m`'s values span 0.25 to 100 (a `min_abs` of 0
/// would be invalid). A source of only 0.0 and −0.0 has no magnitude.
/// Moved from ASAPPlanner #1332 `facts_column_magnitudes_exclude_zero`.
#[test]
fn statistics_magnitudes_exclude_zero() {
    assert_eq!(
        statistics_m().value_magnitude,
        Some(ObservedMagnitude {
            min_abs: 0.25,
            max_abs: 100.0,
        })
    );
    let zeros = vec![sample("z", &[], 0, 0.0), sample("z", &[], 1_000, -0.0)];
    assert_eq!(statistics_of(&zeros, "z").value_magnitude, None);
}

/// Signs by sign bit: `m` has −3 and positives, so `Mixed`; `lat` holds
/// −0.0 and positives, so `Mixed` too, not `NonNegative` (a `v >= 0.0`
/// check would accept −0.0).
/// Moved from ASAPPlanner #1332 `facts_sign_suggestion_sign_bit` (the observed sign only).
#[test]
fn statistics_sign_is_read_from_the_sign_bit() {
    let statistics = data_statistics(&fixture(), &[]);
    assert_eq!(statistics["m"].value_sign, ObservedSign::Mixed);
    assert_eq!(statistics["lat"].value_sign, ObservedSign::Mixed);
}

/// A counter 0, 5, 10, 2, 7 resets once: still `NonNegative`; magnitudes
/// over the raw samples (zeros excluded): 2 to 10.
/// Moved from ASAPPlanner #1332 `facts_counter_reset`.
#[test]
fn statistics_of_a_counter_with_a_reset() {
    let samples: Vec<Sample> = [0.0, 5.0, 10.0, 2.0, 7.0]
        .iter()
        .enumerate()
        .map(|(i, &value)| sample("c_total", &[], i as i64 * 1_000, value))
        .collect();
    let statistics = statistics_of(&samples, "c_total");
    assert_eq!(statistics.value_sign, ObservedSign::NonNegative);
    assert_eq!(
        statistics.value_magnitude,
        Some(ObservedMagnitude {
            min_abs: 2.0,
            max_abs: 10.0,
        })
    );
}

/// A series of one sample has no gap and does not panic: with a second
/// series of 2 samples 3 s apart the interval is 3000; a source of one
/// sample alone has no interval (`None`).
/// Moved from ASAPPlanner #1332 `facts_single_sample_series`.
#[test]
fn statistics_of_a_single_sample_series() {
    let samples = vec![
        sample("s", &[("id", "lonely")], 0, 1.0),
        sample("s", &[("id", "p")], 0, 1.0),
        sample("s", &[("id", "p")], 3_000, 1.0),
    ];
    assert_eq!(statistics_of(&samples, "s").sample_interval_ms, Some(3_000));
    let alone = vec![sample("t", &[], 0, 1.0)];
    assert_eq!(statistics_of(&alone, "t").sample_interval_ms, None);
}

/// Generated data: 50 series of `g` over `host` (100 values) × `endpoint`
/// (8 values, Zipf 1.1) every 10 s for 600 s give series count 50, interval
/// 10,000, at most 50 hosts and at most 8 endpoints.
/// Moved from ASAPPlanner #1332 `facts_datagen_round_trip`.
#[test]
fn statistics_of_generated_data() {
    let record = DataRecord {
        metric: "g".into(),
        description: TimeSeriesDescription {
            labels: vec![
                SeriesLabelSpec {
                    name: "host".into(),
                    cardinality: 100,
                    skewness: 0.0,
                },
                SeriesLabelSpec {
                    name: "endpoint".into(),
                    cardinality: 8,
                    skewness: 1.1,
                },
            ],
            series_num: 50,
            start_ms: 0,
            sample_interval_ms: 10_000,
            duration_ms: 600_000,
            value_distribution: DataDistribution::Uniform(UniformParameter {
                lower_bound: 0.0,
                upper_bound: 1.0,
                seed: 0,
            }),
            counter: false,
            seed: 42,
        },
    };
    let mut samples = Vec::new();
    for chunk in refresh_chunks(&record, 60_000).unwrap() {
        samples.extend(chunk.unwrap().samples);
    }
    let statistics = statistics_of(&samples, "g");
    assert_eq!(statistics.series_count, 50);
    assert_eq!(statistics.sample_interval_ms, Some(10_000));
    assert!(statistics.distinct_values_per_label["host"] <= 50);
    assert!(statistics.distinct_values_per_label["endpoint"] <= 8);
}

/// Four 1 Hz series of `m` over 600 s (one of them varying as
/// `ts % 13,000`), with the label set `[job, instance]`: two runs of
/// `data_statistics` serialize to the same JSON string.
/// Adapted from ASAPPlanner #1332 `facts_seeded_bit_identical`.
#[test]
fn statistics_json_is_byte_identical_across_runs() {
    let series = [
        ("a", "1", 1.0),
        ("a", "2", 2.0),
        ("b", "3", 3.0),
        ("b", "4", 4.0),
    ];
    let mut samples = Vec::new();
    for ts_ms in (0..=600_000).step_by(1_000) {
        for (index, (job, instance, value)) in series.iter().enumerate() {
            let value = match index {
                1 => (ts_ms % 13_000) as f64,
                _ => *value,
            };
            samples.push(sample(
                "m",
                &[("job", job), ("instance", instance)],
                ts_ms,
                value,
            ));
        }
    }
    let once = || serde_json::to_string(&data_statistics(&samples, &job_instance())).unwrap();
    assert_eq!(once(), once());
}

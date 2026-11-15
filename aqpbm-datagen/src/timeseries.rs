//! Labelled time series: `series_num` series, each with one value per sample
//! interval over a duration.
//!
//! A series is a set of `(label name, label value)` pairs. Each label has a
//! cardinality, the number of values it can take, and a Zipf skewness over
//! those values, so a skewed label puts most series on its first few values.
//! Every series draws its values independently from the same
//! [`DataDistribution`]; in counter mode the emitted value is the running sum
//! of the draws, the way a Prometheus counter grows.
//!
//! ```
//! use aqpbm_datagen::{
//!     DataDistribution, LabelSpec, ParetoParameter, TimeSeriesDescription,
//! };
//!
//! let description = TimeSeriesDescription {
//!     // 50 series over `host` (100 values, uniform) x `endpoint` (8 values,
//!     // Zipf 1.1, so `endpoint_0` is the most common).
//!     labels: vec![
//!         LabelSpec { name: "host".into(), cardinality: 100, skewness: 0.0 },
//!         LabelSpec { name: "endpoint".into(), cardinality: 8, skewness: 1.1 },
//!     ],
//!     series_num: 50,
//!     start_ms: 1_700_000_000_000,
//!     // One sample every 10 s for 60 s: 6 samples per series.
//!     sample_interval_ms: 10_000,
//!     duration_ms: 60_000,
//!     // Heavy-tailed latencies of at least 1 ms. The seed inside the
//!     // distribution is not used; `seed` below fixes every draw.
//!     value: DataDistribution::Pareto(ParetoParameter { alpha: 1.5, scale: 1.0, seed: 0 }),
//!     counter: false,
//!     seed: 42,
//! };
//! let generated = description.generate().unwrap();
//! assert_eq!(generated.labels.len(), 50);
//!
//! let mut samples = 0;
//! for (labels, timestamp_ms, value) in generated.samples() {
//!     // labels: [("host", "host_17"), ("endpoint", "endpoint_0")], in
//!     // `description.labels` order.
//!     assert_eq!(labels[0].0, "host");
//!     assert!((timestamp_ms - 1_700_000_000_000) % 10_000 == 0);
//!     assert!(value >= 1.0);
//!     samples += 1;
//! }
//! assert_eq!(samples, 50 * 6);
//! ```

use std::collections::HashSet;

use rand::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;
use serde::{Deserialize, Serialize};

use crate::dist::{DataDistribution, Sampler, ZipfParameter};
use crate::error::DataGenError;
use crate::rule::MonotonicAcc;
use crate::table::splitmix64;

fn bad(msg: String) -> DataGenError {
    DataGenError::BadParam(msg)
}

/// One label of a series: its name, how many values it takes, and how skewed
/// the series are over those values.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelSpec {
    /// The label name, such as `host`.
    pub name: String,
    /// How many values the label takes: `{name}_0` to `{name}_{cardinality-1}`.
    pub cardinality: u64,
    /// Zipf skewness over the values, by index: value `i` is picked with
    /// probability `∝ (i + 1)^-skewness`. `0.0`, the default, is uniform.
    #[serde(default)]
    pub skewness: f64,
}

/// What to generate: which series, at which timestamps, with which values.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSeriesDescription {
    /// The labels every series carries, in output order.
    pub labels: Vec<LabelSpec>,
    /// How many distinct series (distinct label sets) to generate. At most the
    /// product of the label cardinalities.
    pub series_num: u64,
    /// The timestamp of each series' first sample, in milliseconds.
    #[serde(default)]
    pub start_ms: i64,
    /// Milliseconds between two samples of one series.
    pub sample_interval_ms: u64,
    /// Milliseconds covered. Samples sit at `start_ms + k * sample_interval_ms`
    /// for every `k` with `k * sample_interval_ms < duration_ms`.
    pub duration_ms: u64,
    /// The distribution each series draws its values from, independently of
    /// the other series. Its own `seed` is not used: `seed` fixes every draw.
    pub value: DataDistribution,
    /// Counter mode: the value at sample `k` is the sum of the first `k + 1`
    /// draws, each floored at zero, so the series never decreases.
    #[serde(default)]
    pub counter: bool,
    /// Fixes the label sets and every value: the same description generates
    /// the same series.
    pub seed: u64,
}

/// The generated series. Values are drawn lazily by [`GeneratedTimeSeries::samples`].
pub struct GeneratedTimeSeries {
    /// Each series' label set, as `(name, value)` pairs in
    /// [`TimeSeriesDescription::labels`] order. Series `i` is `labels[i]`.
    pub labels: Vec<Vec<(String, String)>>,
    start_ms: i64,
    sample_interval_ms: u64,
    samples_per_series: u64,
    counter: bool,
    seed: u64,
    sampler: Sampler,
}

/// One sample: the series' label set, the timestamp in milliseconds, and the value.
pub type TimeSeriesSample<'a> = (&'a [(String, String)], i64, f64);

/// The seed of one independent stream of draws under the description's seed.
fn stream_seed(seed: u64, stream: u64) -> u64 {
    splitmix64(seed ^ splitmix64(stream))
}

/// Label draws use one stream; series `i`'s values use stream `i`.
const LABEL_STREAM: u64 = u64::MAX;

/// Give up on finding distinct label sets after this many draws per series:
/// only a skew so steep that the tail values are almost never drawn gets here.
const DRAWS_PER_SERIES: u64 = 1000;

impl TimeSeriesDescription {
    /// Check the description without drawing anything.
    pub fn validate(&self) -> Result<(), DataGenError> {
        if self.series_num == 0 {
            return Err(bad("time series: series_num must be > 0".into()));
        }
        if self.sample_interval_ms == 0 || self.duration_ms == 0 {
            return Err(bad(format!(
                "time series: sample_interval_ms ({}) and duration_ms ({}) must be > 0",
                self.sample_interval_ms, self.duration_ms
            )));
        }
        let last_offset = (self.samples_per_series() - 1).checked_mul(self.sample_interval_ms);
        if last_offset
            .and_then(|o| i64::try_from(o).ok())
            .and_then(|o| self.start_ms.checked_add(o))
            .is_none()
        {
            return Err(bad("time series: the last timestamp overflows i64".into()));
        }
        let mut names = HashSet::new();
        for label in &self.labels {
            if label.name.is_empty() || !names.insert(label.name.as_str()) {
                return Err(bad(format!(
                    "time series: label name '{}' is empty or used twice",
                    label.name
                )));
            }
            if label.cardinality == 0 {
                return Err(bad(format!(
                    "time series: label '{}' needs a cardinality > 0",
                    label.name
                )));
            }
            if !label.skewness.is_finite() || label.skewness < 0.0 {
                return Err(bad(format!(
                    "time series: label '{}' needs a finite skewness >= 0, got {}",
                    label.name, label.skewness
                )));
            }
        }
        let label_sets = self
            .labels
            .iter()
            .try_fold(1u64, |n, l| n.checked_mul(l.cardinality));
        if label_sets.is_some_and(|n| n < self.series_num) {
            return Err(bad(format!(
                "time series: {} series asked for, but the label cardinalities allow only {} distinct label sets",
                self.series_num,
                label_sets.unwrap_or(u64::MAX)
            )));
        }
        self.value.sampler().map(|_| ())
    }

    /// How many samples each series has: `ceil(duration_ms / sample_interval_ms)`.
    pub fn samples_per_series(&self) -> u64 {
        self.duration_ms.div_ceil(self.sample_interval_ms.max(1))
    }

    /// Draw the series' label sets. Values come later, from
    /// [`GeneratedTimeSeries::samples`].
    pub fn generate(&self) -> Result<GeneratedTimeSeries, DataGenError> {
        self.validate()?;
        let samplers = self
            .labels
            .iter()
            .map(|label| {
                DataDistribution::Zipf(ZipfParameter {
                    skewness: label.skewness,
                    population_size: label.cardinality,
                    seed: 0,
                })
                .sampler()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(stream_seed(self.seed, LABEL_STREAM));
        let mut seen = HashSet::new();
        let mut labels = Vec::with_capacity(self.series_num as usize);
        let mut draws = 0u64;
        while (labels.len() as u64) < self.series_num {
            if draws >= self.series_num.saturating_mul(DRAWS_PER_SERIES) {
                return Err(bad(format!(
                    "time series: found {} distinct label sets in {draws} draws, short of \
                     series_num {}; lower the skewness or series_num",
                    labels.len(),
                    self.series_num
                )));
            }
            draws += 1;
            // Zipf ranks run 1..=cardinality; value `i` is rank `i + 1`.
            let indexes: Vec<u64> = samplers
                .iter()
                .map(|s| s.sample(&mut rng) as u64 - 1)
                .collect();
            if seen.insert(indexes.clone()) {
                labels.push(
                    self.labels
                        .iter()
                        .zip(&indexes)
                        .map(|(label, i)| (label.name.clone(), format!("{}_{i}", label.name)))
                        .collect(),
                );
            }
        }
        Ok(GeneratedTimeSeries {
            labels,
            start_ms: self.start_ms,
            sample_interval_ms: self.sample_interval_ms,
            samples_per_series: self.samples_per_series(),
            counter: self.counter,
            seed: self.seed,
            sampler: self.value.sampler()?,
        })
    }
}

impl GeneratedTimeSeries {
    /// Every sample of every series, time-major: all series at the first
    /// timestamp in series order, then all at the second, and so on.
    /// Calling it again replays the same samples.
    pub fn samples(&self) -> TimeSeriesSamples<'_> {
        let streams = (0..self.labels.len() as u64)
            .map(|i| SeriesStream {
                rng: Xoshiro256PlusPlus::seed_from_u64(stream_seed(self.seed, i)),
                counter: self.counter.then(|| {
                    // Primed at zero: `MonotonicAcc` returns its start for the first push
                    // and adds every later draw, so the first sample is the first draw.
                    let mut acc = MonotonicAcc::new(0.0);
                    acc.push(0.0).expect("zero is in range");
                    acc
                }),
            })
            .collect();
        TimeSeriesSamples {
            series: self,
            streams,
            next: 0,
        }
    }
}

struct SeriesStream {
    rng: Xoshiro256PlusPlus,
    counter: Option<MonotonicAcc>,
}

/// The iterator [`GeneratedTimeSeries::samples`] returns. In counter mode it
/// stops early, after the last in-range value, if a running sum passes the
/// range an `f64` counts by ones (2^53).
pub struct TimeSeriesSamples<'a> {
    series: &'a GeneratedTimeSeries,
    streams: Vec<SeriesStream>,
    /// Index of the next sample, time-major.
    next: u64,
}

impl<'a> Iterator for TimeSeriesSamples<'a> {
    type Item = TimeSeriesSample<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let series_num = self.streams.len() as u64;
        if self.next >= series_num * self.series.samples_per_series {
            return None;
        }
        let (step, index) = (self.next / series_num, (self.next % series_num) as usize);
        self.next += 1;
        let stream = &mut self.streams[index];
        let draw = self.series.sampler.sample(&mut stream.rng);
        let value = match &mut stream.counter {
            Some(acc) => match acc.push(draw) {
                Ok(v) => v,
                Err(_) => {
                    self.next = u64::MAX;
                    return None;
                }
            },
            None => draw,
        };
        let timestamp_ms = self.series.start_ms + (step * self.series.sample_interval_ms) as i64;
        Some((&self.series.labels[index], timestamp_ms, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let total = self.streams.len() as u64 * self.series.samples_per_series;
        let left = total.saturating_sub(self.next) as usize;
        // A counter can stop early, so only plain mode knows the exact count.
        let lower = if self.series.counter { 0 } else { left };
        (lower, Some(left))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dist::UniformParameter;
    use std::collections::HashMap;

    fn description(counter: bool) -> TimeSeriesDescription {
        TimeSeriesDescription {
            labels: vec![
                LabelSpec {
                    name: "host".into(),
                    cardinality: 20,
                    skewness: 0.0,
                },
                LabelSpec {
                    name: "code".into(),
                    cardinality: 5,
                    skewness: 1.5,
                },
            ],
            series_num: 30,
            start_ms: 1000,
            sample_interval_ms: 10,
            duration_ms: 95,
            value: DataDistribution::Uniform(UniformParameter {
                lower_bound: 0.0,
                upper_bound: 4.0,
                seed: 0,
            }),
            counter,
            seed: 7,
        }
    }

    type OwnedSample = (Vec<(String, String)>, i64, f64);

    fn collect(d: &TimeSeriesDescription) -> Vec<OwnedSample> {
        d.generate()
            .unwrap()
            .samples()
            .map(|(l, t, v)| (l.to_vec(), t, v))
            .collect()
    }

    #[test]
    fn the_same_description_generates_the_same_samples_and_the_seed_changes_them() {
        let d = description(false);
        assert_eq!(collect(&d), collect(&d));
        let reseeded = TimeSeriesDescription {
            seed: 8,
            ..d.clone()
        };
        assert_ne!(collect(&d), collect(&reseeded));
    }

    /// 30 distinct label sets, each sampled at 1000, 1010, ..., 1090 (95 ms
    /// covers 10 samples), time-major.
    #[test]
    fn every_series_is_distinct_and_sampled_on_the_interval() {
        let d = description(false);
        let samples = collect(&d);
        assert_eq!(samples.len(), 30 * 10);
        let mut by_series: HashMap<_, Vec<i64>> = HashMap::new();
        for (labels, t, v) in &samples {
            assert!((0.0..4.0).contains(v));
            assert_eq!(labels[0].0, "host");
            assert_eq!(labels[1].0, "code");
            by_series.entry(labels.clone()).or_default().push(*t);
        }
        assert_eq!(by_series.len(), 30);
        let expected: Vec<i64> = (0..10).map(|k| 1000 + 10 * k).collect();
        assert!(by_series.values().all(|ts| *ts == expected));
        assert!(samples.windows(2).all(|w| w[0].1 <= w[1].1));
    }

    /// Zipf 1.5 over 5 values puts most series on `code_0`.
    #[test]
    fn a_skewed_label_concentrates_on_its_first_values() {
        let d = TimeSeriesDescription {
            series_num: 100,
            labels: vec![
                LabelSpec {
                    name: "host".into(),
                    cardinality: 1000,
                    skewness: 0.0,
                },
                LabelSpec {
                    name: "code".into(),
                    cardinality: 5,
                    skewness: 1.5,
                },
            ],
            ..description(false)
        };
        let g = d.generate().unwrap();
        let head = g.labels.iter().filter(|l| l[1].1 == "code_0").count();
        let tail = g.labels.iter().filter(|l| l[1].1 == "code_4").count();
        assert!(head > 40 && tail < head / 3, "head {head} tail {tail}");
    }

    #[test]
    fn a_counter_is_the_running_sum_of_the_plain_draws() {
        let plain = collect(&description(false));
        let counter = collect(&description(true));
        let mut sums: HashMap<Vec<(String, String)>, f64> = HashMap::new();
        for (p, c) in plain.iter().zip(&counter) {
            assert_eq!((&p.0, p.1), (&c.0, c.1));
            let sum = sums.entry(p.0.clone()).or_default();
            *sum += p.2;
            assert_eq!(*sum, c.2);
        }
    }

    #[test]
    fn bad_descriptions_are_refused_by_name() {
        type Edit = fn(&mut TimeSeriesDescription);
        let cases: Vec<(&str, Edit)> = vec![
            ("series_num must be > 0", |d| d.series_num = 0),
            ("must be > 0", |d| d.sample_interval_ms = 0),
            ("must be > 0", |d| d.duration_ms = 0),
            ("only 100 distinct", |d| d.series_num = 101),
            ("used twice", |d| d.labels[1].name = "host".into()),
            ("cardinality > 0", |d| d.labels[0].cardinality = 0),
            ("skewness >= 0", |d| d.labels[0].skewness = -1.0),
            ("overflows", |d| d.start_ms = i64::MAX),
            ("uniform", |d| {
                d.value = DataDistribution::Uniform(UniformParameter {
                    lower_bound: 1.0,
                    upper_bound: 0.0,
                    seed: 0,
                })
            }),
        ];
        for (want, edit) in cases {
            let mut d = description(false);
            edit(&mut d);
            let err = d.generate().err().expect(want).to_string();
            assert!(err.contains(want), "wanted '{want}' in: {err}");
        }
    }

    #[test]
    fn no_labels_means_one_series() {
        let d = TimeSeriesDescription {
            labels: Vec::new(),
            series_num: 1,
            ..description(false)
        };
        let samples = collect(&d);
        assert_eq!(samples.len(), 10);
        assert!(samples.iter().all(|(l, _, _)| l.is_empty()));
    }

    #[test]
    fn a_description_round_trips_through_yaml() {
        let d = description(true);
        let text = serde_norway::to_string(&d).unwrap();
        let back: TimeSeriesDescription = serde_norway::from_str(&text).unwrap();
        assert_eq!(d, back);
    }
}

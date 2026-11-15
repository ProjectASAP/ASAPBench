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
//!     DataDistribution, ParetoParameter, SeriesLabelSpec, TimeSeriesDescription,
//! };
//!
//! let description = TimeSeriesDescription {
//!     // 50 series over `host` (100 values, uniform) x `endpoint` (8 values,
//!     // Zipf 1.1, so `endpoint_0` is the most common).
//!     labels: vec![
//!         SeriesLabelSpec { name: "host".into(), cardinality: 100, skewness: 0.0 },
//!         SeriesLabelSpec { name: "endpoint".into(), cardinality: 8, skewness: 1.1 },
//!     ],
//!     series_num: 50,
//!     start_ms: 1_700_000_000_000,
//!     // One sample every 10 s for 60 s: 6 samples per series.
//!     sample_interval_ms: 10_000,
//!     duration_ms: 60_000,
//!     // Heavy-tailed latencies of at least 1 ms. The distribution's own seed
//!     // is mixed with `seed` below; 0 leaves `seed` in charge.
//!     value_distribution: DataDistribution::Pareto(ParetoParameter {
//!         alpha: 1.5,
//!         scale: 1.0,
//!         seed: 0,
//!     }),
//!     counter: false,
//!     seed: 42,
//! };
//! let generated = description.generate().unwrap();
//! assert_eq!(generated.labels.len(), 50);
//!
//! let mut samples = 0;
//! for sample in generated.samples() {
//!     // Only a counter can fail (its sum passed 2^53); this one is plain.
//!     let (labels, timestamp_ms, value) = sample.unwrap();
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

use rand::{Rng, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use serde::{Deserialize, Serialize};

use crate::dist::{DataDistribution, Sampler, ZipfParameter};
use crate::error::{bad, DataGenError};
use crate::rule::MonotonicAcc;
use crate::table::splitmix64;

/// One label of a series: its name, how many values it takes, and how skewed
/// the draws are over those values.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesLabelSpec {
    /// The label name, such as `host`.
    pub name: String,
    /// How many values the label takes: `{name}_0` to `{name}_{cardinality-1}`.
    pub cardinality: u64,
    /// Zipf skewness of each draw over the values, by index: a draw picks value
    /// `i` with probability `∝ (i + 1)^-skewness`. `0.0`, the default, is
    /// uniform. The skew is that of the draws, not of the series returned:
    /// series are distinct label sets, so once a heavy set is taken the next
    /// series must use another one. With 200 series over `code` (5 values,
    /// skewness 1.5) x `host` (1000 values), about 57% of the series carry
    /// `code_0`, as one draw would; as `series_num` approaches the product of
    /// the cardinalities the series approach uniform, and at the product every
    /// label set is used once.
    #[serde(default)]
    pub skewness: f64,
}

/// What to generate: which series, at which timestamps, with which values.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSeriesDescription {
    /// The labels every series carries, in output order.
    pub labels: Vec<SeriesLabelSpec>,
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
    /// the other series. Its own seed is mixed with `seed` for the values
    /// (`seed ^ value_distribution.seed`), so changing either changes every
    /// value; at 0 it leaves `seed` alone in charge.
    pub value_distribution: DataDistribution,
    /// Counter mode: the value at sample `k` is the sum of the first `k + 1`
    /// draws, each floored at zero, so the series never decreases. A sum past
    /// 2^53, where an `f64` stops counting by ones, is an error item from
    /// [`GeneratedTimeSeries::samples`].
    #[serde(default)]
    pub counter: bool,
    /// Fixes the label sets and, mixed with `value_distribution`'s seed, every
    /// value: the same description generates the same series.
    pub seed: u64,
}

/// The generated series. Values are drawn lazily by [`GeneratedTimeSeries::samples`].
pub struct GeneratedTimeSeries {
    /// Each series' label set, as `(name, value)` pairs in
    /// [`TimeSeriesDescription::labels`] order. Series `i` is `labels[i]`.
    pub labels: Vec<Vec<(String, String)>>,
    /// The description the series were generated from; `samples` reads the
    /// timestamps, counter mode and seeds from it.
    description: TimeSeriesDescription,
    /// `description.value_distribution`'s sampler, built once.
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

/// When the product of the cardinalities is at most this many times
/// `series_num`, label sets are chosen by weighted sampling without
/// replacement over every set instead of by repeated draws: for 1000 series
/// over 1000 sets, repeated draws would hunt for the last rare sets, while
/// sampling every set once is O(product) and always succeeds.
const WEIGHTED_SAMPLING_SETS_PER_SERIES: u64 = 4;

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
        if let Some(label_sets) = self.label_set_count() {
            if label_sets < self.series_num {
                return Err(bad(format!(
                    "time series: {} series asked for, but the label cardinalities allow only {label_sets} distinct label sets",
                    self.series_num,
                )));
            }
        }
        self.value_distribution.sampler().map(|_| ())
    }

    /// How many samples each series has: `ceil(duration_ms / sample_interval_ms)`.
    pub fn samples_per_series(&self) -> u64 {
        self.duration_ms.div_ceil(self.sample_interval_ms.max(1))
    }

    /// The product of the label cardinalities, or `None` past `u64::MAX`.
    fn label_set_count(&self) -> Option<u64> {
        self.labels
            .iter()
            .try_fold(1u64, |n, l| n.checked_mul(l.cardinality))
    }

    /// Draw the series' label sets. Values come later, from
    /// [`GeneratedTimeSeries::samples`].
    pub fn generate(&self) -> Result<GeneratedTimeSeries, DataGenError> {
        self.validate()?;
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(stream_seed(self.seed, LABEL_STREAM));
        let weighted_label_sets = self.label_set_count().filter(|&n| {
            n <= self
                .series_num
                .saturating_mul(WEIGHTED_SAMPLING_SETS_PER_SERIES)
        });
        let indexes = match weighted_label_sets {
            Some(label_sets) => self.weighted_label_sets(label_sets, &mut rng),
            None => self.drawn_label_sets(&mut rng)?,
        };
        let labels = indexes
            .iter()
            .map(|set| {
                self.labels
                    .iter()
                    .zip(set)
                    .map(|(label, i)| (label.name.clone(), format!("{}_{i}", label.name)))
                    .collect()
            })
            .collect();
        Ok(GeneratedTimeSeries {
            labels,
            sampler: self.value_distribution.sampler()?,
            description: self.clone(),
        })
    }

    /// Label sets by repeated draws, keeping each new one, for `series_num`
    /// well below the number of label sets. Each label draws its value index
    /// from Zipf(cardinality, skewness).
    fn drawn_label_sets(
        &self,
        rng: &mut Xoshiro256PlusPlus,
    ) -> Result<Vec<Vec<u64>>, DataGenError> {
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
        let mut seen = HashSet::new();
        let mut sets = Vec::with_capacity(self.series_num as usize);
        let mut draws = 0u64;
        while (sets.len() as u64) < self.series_num {
            if draws >= self.series_num.saturating_mul(DRAWS_PER_SERIES) {
                return Err(bad(format!(
                    "time series: found {} distinct label sets in {draws} draws, short of \
                     series_num {}; lower the skewness or series_num",
                    sets.len(),
                    self.series_num
                )));
            }
            draws += 1;
            // Zipf ranks run 1..=cardinality; value `i` is rank `i + 1`.
            let set: Vec<u64> = samplers.iter().map(|s| s.sample(rng) as u64 - 1).collect();
            if seen.insert(set.clone()) {
                sets.push(set);
            }
        }
        Ok(sets)
    }

    /// Label sets by weighted sampling without replacement over all
    /// `label_sets` sets, the same distribution as [`Self::drawn_label_sets`]
    /// but in one pass that always succeeds. Set `(i, j, ...)` weighs
    /// `w = (i + 1)^-s_1 (j + 1)^-s_2 ...`, and the `series_num` sets with the
    /// largest key `u^(1/w)`, `u` uniform in (0, 1), win (Efraimidis and
    /// Spirakis), returned largest key first, the order sequential draws would
    /// pick them in. The key is compared as `ln(-ln u) - ln w`, smallest
    /// first, so a weight below the smallest `f64` still ranks.
    fn weighted_label_sets(&self, label_sets: u64, rng: &mut Xoshiro256PlusPlus) -> Vec<Vec<u64>> {
        // Set `code` lists its indexes in mixed radix, the last label fastest.
        let indexes = |code: u64| {
            let mut rest = code;
            let mut set = vec![0; self.labels.len()];
            for (slot, label) in set.iter_mut().zip(&self.labels).rev() {
                *slot = rest % label.cardinality;
                rest /= label.cardinality;
            }
            set
        };
        let mut keyed: Vec<(f64, u64)> = (0..label_sets)
            .map(|code| {
                let log_weight: f64 = self
                    .labels
                    .iter()
                    .zip(indexes(code))
                    .map(|(label, i)| -label.skewness * ((i + 1) as f64).ln())
                    .sum();
                let u: f64 = rng.random();
                ((-u.ln()).ln() - log_weight, code)
            })
            .collect();
        let series_num = self.series_num as usize;
        if series_num < keyed.len() {
            keyed.select_nth_unstable_by(series_num - 1, |a, b| a.0.total_cmp(&b.0));
            keyed.truncate(series_num);
        }
        keyed.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        keyed.into_iter().map(|(_, code)| indexes(code)).collect()
    }
}

impl GeneratedTimeSeries {
    /// Every sample of every series, time-major: all series at the first
    /// timestamp in series order, then all at the second, and so on.
    /// Calling it again replays the same samples. Plain mode never fails; in
    /// counter mode, a running sum past 2^53 yields one `Err` naming the
    /// series and timestamp, and the iterator ends there.
    pub fn samples(&self) -> TimeSeriesSamples<'_> {
        let value_seed = self.description.seed ^ self.description.value_distribution.seed();
        let streams = (0..self.labels.len() as u64)
            .map(|i| SeriesStream {
                rng: Xoshiro256PlusPlus::seed_from_u64(stream_seed(value_seed, i)),
                counter: self.description.counter.then(|| {
                    // Primed at zero: `MonotonicAcc` returns its start for the
                    // first push and adds every later draw floored at zero, so
                    // the first sample is the first draw floored at zero.
                    let mut acc = MonotonicAcc::new(0.0);
                    acc.push(0.0).expect("zero is in range");
                    acc
                }),
            })
            .collect();
        TimeSeriesSamples {
            series: self,
            streams,
            sample_count: self.labels.len() as u64 * self.description.samples_per_series(),
            next: 0,
        }
    }
}

struct SeriesStream {
    rng: Xoshiro256PlusPlus,
    counter: Option<MonotonicAcc>,
}

/// The iterator [`GeneratedTimeSeries::samples`] returns.
pub struct TimeSeriesSamples<'a> {
    series: &'a GeneratedTimeSeries,
    streams: Vec<SeriesStream>,
    /// Samples in all: series times samples per series.
    sample_count: u64,
    /// Index of the next sample, time-major.
    next: u64,
}

impl<'a> Iterator for TimeSeriesSamples<'a> {
    type Item = Result<TimeSeriesSample<'a>, DataGenError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.sample_count {
            return None;
        }
        let series_num = self.streams.len() as u64;
        let (step, index) = (self.next / series_num, (self.next % series_num) as usize);
        self.next += 1;
        let description = &self.series.description;
        let timestamp_ms = description.start_ms + (step * description.sample_interval_ms) as i64;
        let stream = &mut self.streams[index];
        let draw = self.series.sampler.sample(&mut stream.rng);
        let value = match &mut stream.counter {
            Some(acc) => match acc.push(draw) {
                Ok(sum) => sum,
                Err(_) => {
                    self.next = self.sample_count;
                    return Some(Err(bad(format!(
                        "time series: the counter of series {index} passed 2^53 at timestamp \
                         {timestamp_ms} ms, past the range an f64 counts by ones; use a \
                         lighter-tailed value_distribution or a shorter duration"
                    ))));
                }
            },
            None => draw,
        };
        Some(Ok((&self.series.labels[index], timestamp_ms, value)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.sample_count.saturating_sub(self.next) as usize;
        // A counter can end early on an error, so only plain mode knows the
        // exact count.
        let lower = if self.series.description.counter {
            0
        } else {
            left
        };
        (lower, Some(left))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dist::{ParetoParameter, UniformParameter};
    use std::collections::HashMap;

    fn label(name: &str, cardinality: u64, skewness: f64) -> SeriesLabelSpec {
        SeriesLabelSpec {
            name: name.into(),
            cardinality,
            skewness,
        }
    }

    fn uniform(seed: u64) -> DataDistribution {
        DataDistribution::Uniform(UniformParameter {
            lower_bound: 0.0,
            upper_bound: 4.0,
            seed,
        })
    }

    fn description(counter: bool) -> TimeSeriesDescription {
        TimeSeriesDescription {
            labels: vec![label("host", 20, 0.0), label("code", 5, 1.5)],
            series_num: 30,
            start_ms: 1000,
            sample_interval_ms: 10,
            duration_ms: 95,
            value_distribution: uniform(0),
            counter,
            seed: 7,
        }
    }

    type OwnedSample = (Vec<(String, String)>, i64, f64);

    fn collect(d: &TimeSeriesDescription) -> Vec<OwnedSample> {
        d.generate()
            .unwrap()
            .samples()
            .map(|s| s.map(|(l, t, v)| (l.to_vec(), t, v)))
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// How many of the series carry `value` for label `position`.
    fn count(g: &GeneratedTimeSeries, position: usize, value: &str) -> usize {
        g.labels.iter().filter(|l| l[position].1 == value).count()
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

    /// Series `i`'s values come from stream `i` under `seed ^ value seed`: a
    /// value seed of 0 leaves `seed` alone in charge, and any other value seed
    /// changes every value but no label set.
    #[test]
    fn the_value_distributions_seed_is_mixed_into_the_values_only() {
        let d = description(false);
        let g = d.generate().unwrap();
        let sampler = d.value_distribution.sampler().unwrap();
        let first_values = |value_seed: u64| -> Vec<f64> {
            (0..30)
                .map(|i| {
                    let mut rng = Xoshiro256PlusPlus::seed_from_u64(stream_seed(value_seed, i));
                    sampler.sample(&mut rng)
                })
                .collect()
        };
        let first_step = |d: &TimeSeriesDescription| -> Vec<f64> {
            collect(d).iter().take(30).map(|s| s.2).collect()
        };
        assert_eq!(first_step(&d), first_values(7));

        let mixed = TimeSeriesDescription {
            value_distribution: uniform(3),
            ..d.clone()
        };
        assert_eq!(first_step(&mixed), first_values(7 ^ 3));
        assert_eq!(mixed.generate().unwrap().labels, g.labels);
        let (plain, reseeded) = (collect(&d), collect(&mixed));
        assert!(plain.iter().zip(&reseeded).all(|(a, b)| a.2 != b.2));
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

    /// 200 series over 5000 label sets: almost every draw is new, so the
    /// series keep the draw skew. Zipf 1.5 over 5 values gives `code_0`
    /// 1 / (1 + 2^-1.5 + 3^-1.5 + 4^-1.5 + 5^-1.5) = 57% of the draws, 114 of
    /// 200 series (standard deviation 7).
    #[test]
    fn far_below_the_label_set_count_the_series_keep_the_draw_skew() {
        let d = TimeSeriesDescription {
            labels: vec![label("host", 1000, 0.0), label("code", 5, 1.5)],
            series_num: 200,
            ..description(false)
        };
        let g = d.generate().unwrap();
        let head = count(&g, 1, "code_0");
        assert!((90..=138).contains(&head), "code_0 on {head} of 200");
        assert!(count(&g, 1, "code_4") < head / 4);
    }

    /// 100 of 200 label sets goes through weighted sampling without
    /// replacement: `code_0` has only 40 sets and most are taken, while the
    /// light `code_4` sets are mostly left.
    #[test]
    fn near_the_label_set_count_heavy_sets_are_taken_first() {
        let d = TimeSeriesDescription {
            labels: vec![label("host", 40, 0.0), label("code", 5, 1.5)],
            series_num: 100,
            ..description(false)
        };
        let g = d.generate().unwrap();
        let distinct: HashSet<_> = g.labels.iter().collect();
        assert_eq!(distinct.len(), 100);
        let (head, tail) = (count(&g, 1, "code_0"), count(&g, 1, "code_4"));
        assert!(head >= 30 && tail <= 15, "head {head} tail {tail}");
    }

    /// Asking for every label set returns each one once, however steep the
    /// skew: 50 x 20 values at skewness 3 puts the rarest set at about one
    /// draw in 10^9, which repeated draws would never find.
    #[test]
    fn series_num_equal_to_the_label_set_count_always_succeeds() {
        let d = TimeSeriesDescription {
            labels: vec![label("a", 50, 3.0), label("b", 20, 3.0)],
            series_num: 1000,
            ..description(false)
        };
        let g = d.generate().unwrap();
        let distinct: HashSet<_> = g.labels.iter().collect();
        assert_eq!(distinct.len(), 1000);
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

    /// Pareto(alpha 0.3) has no mean, so a counter of it passes 2^53 within a
    /// few thousand steps: the iterator yields one error naming the series and
    /// the timestamp, as its last item, instead of stopping without a word.
    #[test]
    fn a_counter_past_two_to_the_53_ends_with_an_error() {
        let d = TimeSeriesDescription {
            series_num: 10,
            duration_ms: 1_000_000,
            sample_interval_ms: 10,
            value_distribution: DataDistribution::Pareto(ParetoParameter {
                alpha: 0.3,
                scale: 1.0,
                seed: 0,
            }),
            ..description(true)
        };
        let g = d.generate().unwrap();
        let items: Vec<_> = g.samples().collect();
        assert!(items.len() < 10 * 100_000);
        let (last, before) = items.split_last().unwrap();
        assert!(before.iter().all(Result::is_ok));
        let err = last.as_ref().expect_err("an overflow error").to_string();
        assert!(
            err.contains("counter of series") && err.contains("2^53"),
            "{err}"
        );
        assert!(g.samples().nth(items.len()).is_none());
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
                d.value_distribution = DataDistribution::Uniform(UniformParameter {
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

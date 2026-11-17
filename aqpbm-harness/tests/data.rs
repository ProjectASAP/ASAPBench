//! Data: the `data/<metric>.datagen.json` record, refresh chunks over the
//! left-open grid `(start, start + duration]`, and JSONL samples.

use std::collections::BTreeMap;

use aqpbm_datagen::{
    DataDistribution, ParetoParameter, SeriesLabelSpec, TimeSeriesDescription, UniformParameter,
    ZipfParameter,
};
use aqpbm_harness::data::{
    read_samples_jsonl, refresh_chunks, DataError, DataRecord, Sample, SampleChunk,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn label(name: &str, cardinality: u64, skewness: f64) -> SeriesLabelSpec {
    SeriesLabelSpec {
        name: name.into(),
        cardinality,
        skewness,
    }
}

fn uniform() -> DataDistribution {
    DataDistribution::Uniform(UniformParameter {
        lower_bound: 0.0,
        upper_bound: 1.0,
        seed: 0,
    })
}

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

/// FNV-1a over every sample's labels, timestamp and value bits, after
/// sorting by (timestamp, labels) so only the samples, not their order
/// within a timestamp, decide the hash.
fn samples_hash(mut samples: Vec<Sample>) -> u64 {
    samples.sort_by(|a, b| (a.ts_ms, &a.labels).cmp(&(b.ts_ms, &b.labels)));
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for sample in &samples {
        for (name, value) in sample.labels.iter() {
            eat(name.as_bytes());
            eat(&[0xff]);
            eat(value.as_bytes());
            eat(&[0xfe]);
        }
        eat(&sample.ts_ms.to_le_bytes());
        eat(&sample.value.to_bits().to_le_bytes());
    }
    hash
}

/// The samples `aqpbm-datagen` generates for `record`, as harness samples
/// with the `__name__` label.
fn generated_directly(record: &DataRecord) -> Result<Vec<Sample>, Box<dyn std::error::Error>> {
    let generated = record.description.generate()?;
    let mut samples = Vec::new();
    for item in generated.samples() {
        let (labels, ts_ms, value) = item?;
        let mut labels: BTreeMap<String, String> = labels.iter().cloned().collect();
        labels.insert("__name__".into(), record.metric.clone());
        samples.push(Sample {
            labels: labels.into(),
            ts_ms,
            value,
        });
    }
    Ok(samples)
}

/// The samples of every refresh chunk of `record`.
fn regenerated(record: &DataRecord, refresh_ms: u64) -> Result<Vec<Sample>, DataError> {
    let mut samples = Vec::new();
    for chunk in refresh_chunks(record, refresh_ms)? {
        samples.extend(chunk?.samples);
    }
    Ok(samples)
}

fn chunks(record: &DataRecord, refresh_ms: u64) -> Vec<SampleChunk> {
    refresh_chunks(record, refresh_ms)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 2 series of `m` every 10 s over 60 s from start 1,000,000 ms: samples
/// at start + 10 s, ..., start + 60 s.
fn two_series_one_minute() -> DataRecord {
    DataRecord {
        metric: "m".into(),
        description: TimeSeriesDescription {
            labels: vec![label("host", 2, 0.0)],
            series_num: 2,
            start_ms: 1_000_000,
            sample_interval_ms: 10_000,
            duration_ms: 60_000,
            value_distribution: uniform(),
            counter: false,
            seed: 42,
        },
    }
}

/// A record of `m` (job ×4 uniform, instance ×16 Zipf 1.1, 8 series, every
/// 1 s for 600 s, values Zipf(1.1, 1000), seed 42), written to
/// `data/m.datagen.json` and read back, is the same record; its refresh
/// chunks hash the same as `aqpbm-datagen`'s own samples of the original
/// (4,800 samples), seed 43 hashes differently, and the file holds no
/// samples (< 2 KB).
/// Adapted from the ASAPPlanner test `datagen_seeded_bit_identical`.
#[test]
fn a_data_record_regenerates_bit_identical_samples() -> TestResult {
    let record = DataRecord {
        metric: "m".into(),
        description: TimeSeriesDescription {
            labels: vec![label("job", 4, 0.0), label("instance", 16, 1.1)],
            series_num: 8,
            start_ms: 0,
            sample_interval_ms: 1_000,
            duration_ms: 600_000,
            value_distribution: DataDistribution::Zipf(ZipfParameter {
                skewness: 1.1,
                population_size: 1_000,
                seed: 0,
            }),
            counter: false,
            seed: 42,
        },
    };
    let dir =
        std::env::temp_dir().join(format!("aqpbm_harness_data_record_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("data").join("m.datagen.json");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_string_pretty(&record)?)?;
    let bytes = std::fs::metadata(&path)?.len();
    let read_back: DataRecord = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    std::fs::remove_dir_all(&dir)?;

    assert!(bytes < 2_048, "{bytes} bytes");
    assert_eq!(read_back, record);
    let original = generated_directly(&record)?;
    assert_eq!(original.len(), 8 * 600);
    let original_hash = samples_hash(original);
    assert_eq!(
        samples_hash(regenerated(&read_back, 60_000)?),
        original_hash
    );
    let other = DataRecord {
        description: TimeSeriesDescription {
            seed: 43,
            ..record.description.clone()
        },
        ..record
    };
    assert_ne!(samples_hash(regenerated(&other, 60_000)?), original_hash);
    Ok(())
}

/// 2 series every 10 s over 60 s, refresh 30 s: 2 chunks, ending at start +
/// 30 s with 6 samples (10, 20 and 30 s) and at start + 60 s with 6 (40, 50
/// and 60 s).
#[test]
fn refresh_chunks_split_at_left_open_window_ends() {
    let record = two_series_one_minute();
    let start = record.description.start_ms;
    let chunks = chunks(&record, 30_000);
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    assert_eq!(chunks[0].end_ms, start + 30_000);
    assert_eq!(chunks[0].samples.len(), 6, "{chunks:?}");
    assert_eq!(chunks[1].end_ms, start + 60_000);
    assert_eq!(chunks[1].samples.len(), 6, "{chunks:?}");
    let times = |chunk: &SampleChunk| {
        let mut times: Vec<i64> = chunk.samples.iter().map(|s| s.ts_ms - start).collect();
        times.dedup();
        times
    };
    assert_eq!(times(&chunks[0]), vec![10_000, 20_000, 30_000]);
    assert_eq!(times(&chunks[1]), vec![40_000, 50_000, 60_000]);
}

/// As G2: both samples at start + 30 s, the end of the first chunk, are in
/// that chunk and not the next; every sample lies in `(end - 30 s, end]`.
#[test]
fn a_sample_on_a_window_end_belongs_to_the_window_it_ends() {
    let record = two_series_one_minute();
    let boundary = record.description.start_ms + 30_000;
    let chunks = chunks(&record, 30_000);
    let on_boundary =
        |chunk: &SampleChunk| chunk.samples.iter().filter(|s| s.ts_ms == boundary).count();
    assert_eq!(on_boundary(&chunks[0]), 2, "{chunks:?}");
    assert_eq!(on_boundary(&chunks[1]), 0, "{chunks:?}");
    for chunk in &chunks {
        for sample in &chunk.samples {
            assert!(
                chunk.end_ms - 30_000 < sample.ts_ms && sample.ts_ms <= chunk.end_ms,
                "{sample:?} in chunk ending {}",
                chunk.end_ms
            );
        }
    }
}

/// A counter of Pareto(α 0.3) draws (no mean), 10 series × 100,000 samples
/// every 10 ms, passes 2^53 within a few thousand steps: the chunks before
/// are `Ok`, and the last item is a `Generator` error for metric `c` naming
/// the series and 2^53.
#[test]
fn a_generator_error_ends_the_chunks_with_that_error() {
    let record = DataRecord {
        metric: "c".into(),
        description: TimeSeriesDescription {
            labels: vec![label("host", 10, 0.0)],
            series_num: 10,
            start_ms: 0,
            sample_interval_ms: 10,
            duration_ms: 1_000_000,
            value_distribution: DataDistribution::Pareto(ParetoParameter {
                alpha: 0.3,
                scale: 1.0,
                seed: 0,
            }),
            counter: true,
            seed: 7,
        },
    };
    let items: Vec<_> = refresh_chunks(&record, 10_000).unwrap().collect();
    let (last, before) = items.split_last().expect("at least the error item");
    assert!(before.iter().all(Result::is_ok));
    match last {
        Err(DataError::Generator { metric, message }) => {
            assert_eq!(metric, "c");
            assert!(
                message.contains("counter of series") && message.contains("2^53"),
                "{message}"
            );
        }
        other => panic!("expected a generator error last, got {other:?}"),
    }
}

/// Two JSONL lines `{labels, ts_ms, value}` read as two samples. A text whose
/// line at 0-based index 1 lacks `value` (`{"labels": {}, "ts_ms": 1}`
/// after one good line) is an error with `line: 1`.
/// Moved from the ASAPPlanner test `facts_reads_jsonl_samples`.
#[test]
fn samples_jsonl_reads_and_names_the_bad_line() {
    let good = "{\"labels\": {\"__name__\": \"m\", \"job\": \"a\"}, \"ts_ms\": 1000, \"value\": 2.0}\n\
                {\"labels\": {\"__name__\": \"m\", \"job\": \"b\"}, \"ts_ms\": 2000, \"value\": -0.5}\n";
    assert_eq!(
        read_samples_jsonl(good).unwrap(),
        vec![
            sample("m", &[("job", "a")], 1_000, 2.0),
            sample("m", &[("job", "b")], 2_000, -0.5),
        ]
    );
    let bad =
        "{\"labels\": {\"__name__\": \"m\", \"job\": \"a\"}, \"ts_ms\": 1000, \"value\": 2.0}\n\
               {\"labels\": {}, \"ts_ms\": 1}\n";
    let error = read_samples_jsonl(bad).unwrap_err();
    assert_eq!(error.line, 1, "{error}");
}

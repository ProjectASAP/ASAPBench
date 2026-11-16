//! A use-case spec and an `aqpbm-datagen` `TimeSeriesDescription` that
//! describe the same series draw the same samples: one sample grid and one
//! per-series stream seed, both from `aqpbm-datagen`.

use std::path::Path;

use aqpbm_datagen::{
    series_stream_seed, DataDistribution, NormalParameter, SeriesLabelSpec, TimeSeriesDescription,
};
use aqpbm_usecase::data::generate;
use aqpbm_usecase::spec::Spec;
use rand::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;

const START_MS: i64 = 1_767_225_600_000;

fn normal() -> DataDistribution {
    DataDistribution::Normal(NormalParameter {
        mean: 0.45,
        standard_deviation: 0.08,
        seed: 22,
    })
}

/// Three series sampled every 500 ms for 5 s from Normal(0.45, 0.08) with
/// seed 22, no phases.
fn spec() -> Spec {
    let spec: Spec = serde_json::from_value(serde_json::json!({
        "use_case": "conventions",
        "start_ms": START_MS,
        "duration_ms": 5000,
        "seed": 1,
        "metrics": [{
            "name": "cpu",
            "sample_interval_ms": 500,
            "series": [{
                "count": 3,
                "labels": {"host": "h{i}"},
                "baseline": {"kind": "normal", "mean": 0.45, "standard_deviation": 0.08, "seed": 22}
            }]
        }],
        "queries": [
            {"id": "q", "promql": "avg_over_time(cpu[1s])", "window_ms": 1000, "interval_ms": 1000, "unit": "ratio"}
        ]
    }))
    .unwrap();
    spec.check().unwrap();
    spec
}

#[test]
fn usecase_and_time_series_description_draw_the_same_samples() {
    let usecase = generate(&spec(), Path::new(".")).unwrap();
    let description = TimeSeriesDescription {
        labels: vec![SeriesLabelSpec {
            name: "host".into(),
            cardinality: 3,
            skewness: 0.0,
        }],
        series_num: 3,
        start_ms: START_MS,
        sample_interval_ms: 500,
        duration_ms: 5000,
        value_distribution: normal(),
        counter: false,
        seed: 0,
    };
    let generated = description.generate().unwrap();
    let mut by_series: Vec<Vec<(i64, u64)>> = vec![Vec::new(); 3];
    for (index, sample) in generated.samples().enumerate() {
        let (_, t, v) = sample.unwrap();
        by_series[index % 3].push((t, v.to_bits()));
    }
    assert_eq!(usecase.len(), 3);
    for (i, series) in usecase.iter().enumerate() {
        let drawn: Vec<(i64, u64)> = series
            .points
            .iter()
            .map(|&(t, v)| (t, v.to_bits()))
            .collect();
        assert_eq!(drawn, by_series[i], "series {i}");
    }
}

#[test]
fn a_usecase_series_draws_from_the_shared_stream_seed() {
    let usecase = generate(&spec(), Path::new(".")).unwrap();
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(series_stream_seed(22, 0));
    let expected = normal().sampler().unwrap().sample(&mut rng);
    assert_eq!(usecase[0].points[0].1.to_bits(), expected.to_bits());
}

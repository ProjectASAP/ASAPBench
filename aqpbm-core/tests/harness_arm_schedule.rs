//! The schedule every lane uses for a workload.

use aqpbm_datagen::{DataDistribution, SeriesLabelSpec, TimeSeriesDescription, UniformParameter};
use aqpbm_core::harness::arm_run::{ArmSchedule, ScheduleError};
use aqpbm_core::harness::data::DataRecord;
use aqpbm_core::harness::engine::{QueryLanguage, QueryText, WorkloadQuery, WorkloadSpec};

fn query(id: &str, window_ms: u64, evaluation_interval_ms: u64) -> WorkloadQuery {
    WorkloadQuery {
        id: id.into(),
        texts: vec![QueryText {
            language: QueryLanguage::PromQl,
            text: format!("sum_over_time(m[{window_ms}ms])"),
        }],
        window_ms,
        evaluation_interval_ms,
    }
}

fn workload(queries: Vec<WorkloadQuery>, start_ms: i64, duration_ms: u64) -> WorkloadSpec {
    WorkloadSpec {
        name: "w".into(),
        queries,
        data: vec![DataRecord {
            metric: "m".into(),
            description: TimeSeriesDescription {
                labels: vec![SeriesLabelSpec {
                    name: "s".into(),
                    cardinality: 1,
                    skewness: 0.0,
                }],
                series_num: 1,
                start_ms,
                sample_interval_ms: 1_000,
                duration_ms,
                value_distribution: DataDistribution::Uniform(UniformParameter {
                    lower_bound: 0.0,
                    upper_bound: 1.0,
                    seed: 0,
                }),
                counter: false,
                seed: 1,
            },
        }],
    }
}

/// Windows 30 s and 20 s every 10 s over data from 1,000 ms for 120 s: the
/// multiples of 10 s from 31,000 ms (start + longest window) to 121,000 ms.
#[test]
fn evaluations_are_the_interval_multiples_after_the_longest_window() {
    let spec = workload(
        vec![query("a", 30_000, 10_000), query("b", 20_000, 10_000)],
        1_000,
        120_000,
    );
    let schedule = ArmSchedule::for_workload(&spec).unwrap();
    assert_eq!(schedule.refresh_ms, 10_000);
    assert_eq!(
        schedule.evaluate_at_ms,
        (4..=12).map(|k| k * 10_000).collect::<Vec<i64>>()
    );
    // 125 s of data: the last multiple inside the data is still 120,000.
    let longer = workload(vec![query("a", 30_000, 10_000)], 1_000, 125_000);
    assert_eq!(
        ArmSchedule::for_workload(&longer)
            .unwrap()
            .evaluate_at_ms
            .last(),
        Some(&120_000)
    );
}

#[test]
fn workloads_no_schedule_fits_are_refused() {
    let mixed = workload(
        vec![query("a", 30_000, 10_000), query("b", 30_000, 15_000)],
        0,
        120_000,
    );
    assert!(matches!(
        ArmSchedule::for_workload(&mixed),
        Err(ScheduleError::MixedEvaluationIntervals {
            first_ms: 10_000,
            other_ms: 15_000,
            ..
        })
    ));
    let empty = workload(Vec::new(), 0, 120_000);
    assert!(matches!(
        ArmSchedule::for_workload(&empty),
        Err(ScheduleError::NoQueries { .. })
    ));
    let too_long = workload(vec![query("a", 200_000, 10_000)], 0, 120_000);
    assert!(matches!(
        ArmSchedule::for_workload(&too_long),
        Err(ScheduleError::NoEvaluationTime { .. })
    ));
}

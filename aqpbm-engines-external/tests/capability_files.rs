//! K1: the committed deployment-capabilities files (ASAPPlanner #1374
//! format) are exactly the hand-computed values of the design. An edit to a
//! file must also edit this test and the README row it comes from.

use serde_json::{json, Value};

fn file(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("capabilities")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).expect("file")).expect("JSON")
}

fn sketches_all() -> Value {
    json!([
        {"summary_type": "Hll", "readouts": ["Distinct"]},
        {"summary_type": "Kmv", "readouts": ["Distinct"]},
        {"summary_type": "Kll", "readouts": ["Quantile"]},
        {"summary_type": "DDSketch", "readouts": ["Quantile"]},
        {"summary_type": "CmsWithHeap", "readouts": ["TopK"]},
        {"summary_type": "CountSketchWithHeap", "readouts": ["TopK"]}
    ])
}

fn clickhouse_irate_exclusions() -> Value {
    json!([
        {"summary_family": {"exact": "IRate"}},
        {"operator": "Aggregate", "aggregate_measure": "i_rate"}
    ])
}

#[test]
fn capability_files_match_the_design() {
    assert_eq!(
        file("clickhouse.equivalent-only.json"),
        json!({
            // No sketch until the translator builds sketch states.
            "supported_summaries": [],
            "raw_data_retained": true,
            "query_time_results_retained": false,
            "counter_semantics": {"reset_handling": true, "extrapolation": true},
            "state_not_mergeable_across_windows": [{"exact": "Rate"}, {"exact": "Increase"}],
            "excluded_candidates": clickhouse_irate_exclusions()
        })
    );
    assert_eq!(
        file("clickhouse.with-substitutes.json"),
        json!({
            "supported_summaries": [],
            "raw_data_retained": true,
            "query_time_results_retained": false,
            "counter_semantics": {"reset_handling": true, "extrapolation": true},
            "state_not_mergeable_across_windows": [],
            "excluded_candidates": clickhouse_irate_exclusions()
        })
    );
    assert_eq!(
        file("victoriametrics.equivalent-only.json"),
        json!({
            "supported_summaries": [],
            "raw_data_retained": true,
            "query_time_results_retained": false,
            "counter_semantics": {"reset_handling": false, "extrapolation": false},
            "state_not_mergeable_across_windows": [],
            "excluded_candidates": []
        })
    );
    assert_eq!(
        file("victoriametrics.with-substitutes.json"),
        json!({
            "supported_summaries": sketches_all(),
            "raw_data_retained": true,
            "query_time_results_retained": false,
            "counter_semantics": {"reset_handling": true, "extrapolation": true},
            "state_not_mergeable_across_windows": [
                {"exact": "IRate"}, {"sketch": "Hll"}, {"sketch": "Kmv"},
                {"sketch": "CmsWithHeap"}, {"sketch": "CountSketchWithHeap"}
            ],
            "excluded_candidates": []
        })
    );
}

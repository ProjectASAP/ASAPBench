//! Reading an ASAPPlanner `InstallablePlan` file (wire version 15).

mod common;

use common::{fixture, fixture_value, payload_body, public_fixture};
use serde_json::json;
use system_bench::engines_external::plan_file::{
    Lifetime, Measure, PlanFile, PlanOperator, Predicate, RangeKind, Reduction, Source,
    SummaryFamily, Timing, SUPPORTED_PLAN_WIRE_VERSION,
};
use system_bench::engines_external::TranslationError;

#[test]
fn reads_a_wire_15_plan() {
    let plan = fixture("sum_over_time_1h_stored_sum");
    assert_eq!(SUPPORTED_PLAN_WIRE_VERSION, 15);
    assert_eq!(plan.nodes.len(), 6);
    assert_eq!(plan.roots, vec![5]);
    assert_eq!(
        plan.query_texts,
        vec!["sum by (job) (sum_over_time(latency[1h]))".to_string()]
    );

    let scan = &plan.nodes[0];
    assert_eq!(
        scan.operator,
        PlanOperator::Scan {
            source: Source::TimeSeries {
                metric: "latency".into()
            },
            predicates: vec![],
        }
    );
    assert_eq!(scan.timing, Timing::IngestionTime);
    assert_eq!(
        scan.output_columns,
        vec!["ts", "value", "job", "$promql_series_identity"]
    );
    // Labels are the `{"Plain": "utf8"}` columns, timestamps the
    // `{"Plain": "timestamp"}` ones; `value` is neither.
    assert_eq!(scan.label_columns, vec!["job", "$promql_series_identity"]);
    assert_eq!(scan.timestamp_columns, vec!["ts"]);
    assert!(scan.is_value_column(1));
    assert!(!scan.is_value_column(0));
    assert!(!scan.is_value_column(2));
    // An ingestion-time Scan carries no retention.
    assert_eq!(scan.retention_ms, None);

    let chain = &plan.nodes[1];
    assert_eq!(
        chain.operator,
        PlanOperator::SummaryTumblingWindowChain {
            child: 0,
            family: SummaryFamily::Exact("Sum".into()),
            width_ms: 60_000,
            reduction: Reduction::PerEntity,
            input: json!({"item": null, "weight": {"Column": "SampleValue"}}),
            filter: None,
        }
    );
    // `output_state.timing` on the wire.
    assert_eq!(chain.timing, Timing::IngestionTime);
    // `{"AcrossEvaluations": "Memory"}` on the wire.
    assert_eq!(chain.lifetime, Lifetime::AcrossEvaluations);
    // Wire `retention_ms`: the 1 h the query reads back.
    assert_eq!(chain.retention_ms, Some(3_600_000));

    let range = &plan.nodes[2];
    // 3600 s × 1000 + 0 ns / 10⁶.
    assert_eq!(
        range.operator,
        PlanOperator::TimeRange {
            child: 1,
            kind: RangeKind::Range,
            range_ms: 3_600_000
        }
    );
    assert_eq!(range.timing, Timing::QueryTime);
    // `"None"` on the wire.
    assert_eq!(range.lifetime, Lifetime::None);
    assert_eq!(range.retention_ms, None);

    // `group_by` is absent on the wire when it is `None`.
    assert_eq!(
        plan.nodes[3].operator,
        PlanOperator::SummaryMerge {
            children: vec![2],
            group_by: None
        }
    );
    assert_eq!(plan.nodes[3].lifetime, Lifetime::OneEvaluation);
    assert_eq!(
        plan.nodes[4].operator,
        PlanOperator::FinalizeExactAccumulator { child: 3 }
    );
    assert_eq!(
        plan.nodes[5].operator,
        PlanOperator::Aggregate {
            child: 4,
            measures: vec![Measure {
                kind: "sum".into(),
                column: None,
                arguments: serde_json::Map::new(),
            }],
            reduction: Reduction::Reduce {
                columns: vec![2],
                without: false
            },
            measure_filters: vec![],
            having: None,
        }
    );
    assert_eq!(plan.nodes[5].output_columns, vec!["job", "sum"]);
    assert_eq!(plan.nodes[5].label_columns, vec!["job"]);
}

#[test]
fn predicates_decode_to_label_equals() {
    let plan = fixture("max_5m_label_filter");
    let PlanOperator::Scan { predicates, .. } = &plan.nodes[0].operator else {
        panic!("node 0 is a Scan");
    };
    assert_eq!(
        predicates,
        &vec![Predicate::LabelEquals {
            column: 2,
            value: "j3".into()
        }]
    );

    // M6: another comparison is kept as is.
    let mut value = fixture_value("max_5m_label_filter");
    payload_body(&mut value, 0)["predicates"][0]["Compare"]["op"] = json!("Ne");
    let ne = payload_body(&mut value, 0)["predicates"][0].clone();
    let plan = PlanFile::from_value(value).unwrap();
    let PlanOperator::Scan { predicates, .. } = &plan.nodes[0].operator else {
        panic!("node 0 is a Scan");
    };
    assert_eq!(predicates, &vec![Predicate::Other(ne)]);
}

#[test]
fn only_promql_label_equality_is_label_equals() {
    // The same comparison with SQL semantics (where `job = ''` does not
    // match a series without the label) is kept as is.
    let mut value = fixture_value("max_5m_label_filter");
    payload_body(&mut value, 0)["predicates"][0]["Compare"]["semantics"] = json!("sql");
    let sql = payload_body(&mut value, 0)["predicates"][0].clone();
    let plan = PlanFile::from_value(value).unwrap();
    let PlanOperator::Scan { predicates, .. } = &plan.nodes[0].operator else {
        panic!("node 0 is a Scan");
    };
    assert_eq!(predicates, &vec![Predicate::Other(sql)]);
}

#[test]
fn aggregate_filters_are_one_optional_predicate_per_measure() {
    // Wire `filters` is `Vec<Option<Predicate>>`, one entry per measure.
    let filter = json!({"Literal": {"Boolean": true}});
    for (wire, decoded) in [
        (json!([null]), vec![None]),
        (json!([filter.clone()]), vec![Some(filter.clone())]),
        (json!([]), vec![]),
    ] {
        let mut value = fixture_value("sum_over_time_1h_stored_sum");
        payload_body(&mut value, 5)["filters"] = wire;
        let plan = PlanFile::from_value(value).unwrap();
        let PlanOperator::Aggregate {
            measure_filters, ..
        } = &plan.nodes[5].operator
        else {
            panic!("node 5 is an Aggregate");
        };
        assert_eq!(measure_filters, &decoded);
    }
    // The planner's serde default: an absent list is no filter.
    let mut value = fixture_value("sum_over_time_1h_stored_sum");
    payload_body(&mut value, 5)
        .as_object_mut()
        .unwrap()
        .remove("filters");
    let plan = PlanFile::from_value(value).unwrap();
    assert!(matches!(
        &plan.nodes[5].operator,
        PlanOperator::Aggregate { measure_filters, .. } if measure_filters.is_empty()
    ));
}

#[test]
fn query_time_scans_carry_their_retention() {
    // The planner keeps a query-time Scan's rows as long as its readers
    // read back (`retention_ms`).
    let plan = fixture("rate_5m_query_time");
    assert_eq!(plan.nodes[0].timing, Timing::QueryTime);
    assert_eq!(plan.nodes[0].retention_ms, Some(300_000));
}

#[test]
fn projections_and_binary_operators_decode() {
    // `avg by (region, service) (quantile_over_time(0.9, data[100s]))`, planned
    // as sum / count: node 6 casts the count, node 10 divides.
    let plan = public_fixture("mixed-03", "clickhouse.equivalent-only");
    assert_eq!(
        plan.nodes[6].operator,
        PlanOperator::Project {
            child: 5,
            expressions: vec![
                json!({"Column": 0}),
                json!({"Column": 1}),
                json!({"Cast": {"expr": {"Column": 2}, "to": "float64", "try_cast": false}}),
            ],
        }
    );
    assert_eq!(
        plan.nodes[10].operator,
        PlanOperator::BinaryOp {
            lhs: 9,
            rhs: 6,
            operator: json!({"checked_relative_division": false,
                             "checked_finite_division": true,
                             "kind": {"Arithmetic": "Div"}, "vector_match": null}),
            return_bool: false,
        }
    );
    assert_eq!(plan.nodes[10].label_columns, vec!["region", "service"]);
    assert!(plan.nodes[10].is_value_column(2));
    assert_eq!(plan.readers_of(9), vec![10]);
}

#[test]
fn refuses_another_wire_version() {
    // M1
    // The version is read first: the rest of the file is never parsed.
    let mut value = fixture_value("sum_over_time_1h_stored_sum");
    value["wire_version"] = json!(16);
    value["dag"] = json!(null);
    assert_eq!(
        PlanFile::from_value(value),
        Err(TranslationError::UnsupportedWireVersion {
            found: Some(16),
            supported: 15
        })
    );
    // M1'
    let mut value = fixture_value("sum_over_time_1h_stored_sum");
    value.as_object_mut().unwrap().remove("wire_version");
    assert_eq!(
        PlanFile::from_value(value),
        Err(TranslationError::UnsupportedWireVersion {
            found: None,
            supported: 15
        })
    );
}

#[test]
fn an_unknown_operator_still_decodes() {
    // M2
    let mut value = fixture_value("sum_over_time_1h_stored_sum");
    value["dag"]["nodes"][3]["payload"] = json!({"ASAP": {"FancyNewOp": {"children": [2]}}});
    let plan = PlanFile::from_value(value).unwrap();
    assert_eq!(
        plan.nodes[3].operator,
        PlanOperator::Other {
            kind: "ASAP.FancyNewOp".into()
        }
    );
}

#[test]
fn scalar_queries_are_refused() {
    // M5
    let mut value = fixture_value("sum_over_time_1h_stored_sum");
    value["dag"]["scalar_roots"] = json!([[0, {"Literal": {"Float64": 1.0}}]]);
    assert_eq!(
        PlanFile::from_value(value),
        Err(TranslationError::UnsupportedScalarQuery { query_index: 0 })
    );
}

#[test]
fn ids_outside_the_dag_are_refused() {
    let malformed = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = fixture_value("sum_over_time_1h_stored_sum");
        change(&mut value);
        matches!(
            PlanFile::from_value(value),
            Err(TranslationError::MalformedPlan { .. })
        )
    };
    assert!(malformed(&|v| payload_body(v, 5)["child"] = json!(9)));
    assert!(malformed(&|v| v["dag"]["roots"] = json!([99])));
    // A node may read only nodes that come before it (no cycle).
    assert!(malformed(&|v| payload_body(v, 1)["child"] = json!(3)));
    assert!(malformed(&|v| v["query_texts"] = json!([])));
}

#[test]
fn roots_and_readers() {
    let plan = fixture("joint_5m");
    assert_eq!(plan.roots, vec![4, 3, 8, 2, 13]);
    assert_eq!(plan.readers_of(1), vec![2, 3, 4]);
    assert_eq!(plan.readers_of(0), vec![1]);
    assert_eq!(plan.readers_of(13), Vec::<usize>::new());
}

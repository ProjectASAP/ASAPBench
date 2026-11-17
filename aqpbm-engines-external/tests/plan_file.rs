//! R1–R6: reading an ASAPPlanner `InstallablePlan` file (wire version 15).

mod common;

use aqpbm_engines_external::plan_file::{
    Lifetime, Measure, PlanFile, PlanOperator, Predicate, RangeKind, Reduction, Source,
    SummaryFamily, Timing, SUPPORTED_PLAN_WIRE_VERSION,
};
use aqpbm_engines_external::TranslationError;
use common::{fixture, fixture_value, payload_body};
use serde_json::json;

#[test]
fn reads_a_wire_15_plan() {
    let plan = fixture("sot_1h_stored_sum");
    assert_eq!(plan.wire_version, 15);
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

    let chain = &plan.nodes[1];
    assert_eq!(
        chain.operator,
        PlanOperator::WindowChain {
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
    assert_eq!(chain.retention_ms, Some(3_600_000));
    assert_eq!(chain.parameters, None);

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

    // `group_by` is absent on the wire when it is `None`.
    assert_eq!(
        plan.nodes[3].operator,
        PlanOperator::SummaryMerge {
            children: vec![2],
            group_by: None
        }
    );
    assert_eq!(plan.nodes[3].lifetime, Lifetime::OneEvaluation);
    assert_eq!(plan.nodes[4].operator, PlanOperator::Finalize { child: 3 });
    assert_eq!(
        plan.nodes[5].operator,
        PlanOperator::Aggregate {
            child: 4,
            measures: vec![Measure {
                kind: "sum".into(),
                col: None,
                arguments: serde_json::Map::new(),
            }],
            reduction: Reduction::Reduce {
                columns: vec![2],
                without: false
            },
            filters: vec![],
            having: None,
        }
    );
    assert_eq!(plan.nodes[5].output_columns, vec!["job", "sum"]);
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
fn refuses_another_wire_version() {
    // M1
    let mut value = fixture_value("sot_1h_stored_sum");
    value["wire_version"] = json!(16);
    assert_eq!(
        PlanFile::from_value(value),
        Err(TranslationError::UnsupportedWireVersion {
            found: Some(16),
            supported: 15
        })
    );
    // M1'
    let mut value = fixture_value("sot_1h_stored_sum");
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
    let mut value = fixture_value("sot_1h_stored_sum");
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
    let mut value = fixture_value("sot_1h_stored_sum");
    value["dag"]["scalar_roots"] = json!([[0, {"Literal": 1.0}]]);
    assert_eq!(
        PlanFile::from_value(value),
        Err(TranslationError::UnsupportedScalarQuery { query: 0 })
    );
}

#[test]
fn roots_and_readers() {
    let plan = fixture("joint_5m");
    assert_eq!(plan.roots, vec![4, 3, 8, 2, 13]);
    assert_eq!(plan.readers_of(1), vec![2, 3, 4]);
    assert_eq!(plan.readers_of(0), vec![1]);
    assert_eq!(plan.readers_of(13), Vec::<usize>::new());
}

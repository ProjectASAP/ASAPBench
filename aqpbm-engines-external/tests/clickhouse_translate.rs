//! X1–X10: translating planner-written plans into ClickHouse tables,
//! materialized views and one SQL query per query root. The goldens under
//! `tests/golden/clickhouse/` were written by hand before the translator.

mod common;

use aqpbm_engines_external::clickhouse::{translate, ClickHouseDeployment, TableRetention};
use aqpbm_engines_external::plan_file::PlanFile;
use aqpbm_engines_external::{SharingLost, Substitution, TranslationError};
use common::{
    fixture, fixture_value, fixture_value_public, golden, normalized, payload_body, public_fixture,
};
use serde_json::{json, Value};

fn translated(name: &str) -> ClickHouseDeployment {
    translate(&fixture(name)).expect("translates")
}

fn refusal(value: Value) -> TranslationError {
    let plan = PlanFile::from_value(value).expect("decodes");
    translate(&plan).expect_err("refused")
}

fn shape(node: usize, reason: &str) -> TranslationError {
    TranslationError::UnsupportedShape {
        node,
        reason: reason.into(),
    }
}

#[test]
fn stored_sum_worked_example() {
    let deployment = translated("sot_1h_stored_sum");
    assert_eq!(
        normalized(&deployment.script()),
        normalized(&golden("f1.sql"))
    );
    assert_eq!(deployment.tables.len(), 2);
    assert_eq!(deployment.materialized_views.len(), 1);
    assert_eq!(deployment.queries.len(), 1);
    assert!(deployment.substitutions.is_empty());
    assert!(deployment.sharing_lost.is_empty());
}

#[test]
fn stored_rate_is_a_recorded_substitute() {
    let deployment = translated("rate_5m_stored");
    assert_eq!(
        normalized(&deployment.script()),
        normalized(&golden("f2.sql"))
    );
    assert_eq!(
        deployment.substitutions,
        vec![Substitution {
            node: 1,
            rule: "clickhouse.stored_rate.delta_sum_timestamp".into(),
            guarantee_difference: "a counter reset adds 0 for the step across it, where PromQL \
                                   adds the first value after the reset"
                .into(),
        }]
    );
    assert!(deployment.sharing_lost.is_empty());
}

#[test]
fn query_time_rate_is_equivalent() {
    let deployment = translated("rate_5m_query_time");
    assert_eq!(
        normalized(&deployment.script()),
        normalized(&golden("f3.sql"))
    );
    assert_eq!(deployment.tables.len(), 1);
    assert!(deployment.materialized_views.is_empty());
    assert_eq!(deployment.queries.len(), 1);
    assert!(deployment.substitutions.is_empty());
    assert!(deployment.sharing_lost.is_empty());
}

#[test]
fn label_equality_becomes_where() {
    let deployment = translated("max_5m_label_filter");
    assert_eq!(
        normalized(&deployment.script()),
        normalized(&golden("f4.sql"))
    );
    assert!(deployment.substitutions.is_empty());
    assert!(deployment.sharing_lost.is_empty());

    // A label value with a quote and a backslash is escaped.
    let mut value = fixture_value("max_5m_label_filter");
    payload_body(&mut value, 0)["predicates"][0]["Compare"]["right"] =
        json!({"Literal": {"Utf8": "j'3\\"}});
    let deployment = translate(&PlanFile::from_value(value).unwrap()).unwrap();
    assert!(
        normalized(&deployment.queries[0]).contains(r"WHERE `job` = 'j\'3\\' AND"),
        "{}",
        deployment.queries[0]
    );
}

#[test]
fn empty_reduce_has_no_group_by_and_no_row_when_empty() {
    let deployment = translated("count_distinct_5m");
    assert_eq!(
        normalized(&deployment.script()),
        normalized(&golden("f5.sql"))
    );
    assert!(deployment.substitutions.is_empty());
    assert!(deployment.sharing_lost.is_empty());
}

#[test]
fn shared_query_time_node_is_reported() {
    let deployment = translated("joint_5m");
    assert_eq!(
        deployment
            .tables
            .iter()
            .map(|t| normalized(t))
            .collect::<Vec<_>>(),
        vec![
            normalized(
                "CREATE TABLE src_latency (ts DateTime64(3, 'UTC'), value Float64, `job` LowCardinality(String), \
                 `$promql_series_identity` String, INDEX ts_minmax ts TYPE minmax GRANULARITY 1) \
                 ENGINE = MergeTree ORDER BY (`$promql_series_identity`, ts)"
            ),
            normalized(
                "CREATE TABLE src_requests_total (ts DateTime64(3, 'UTC'), value Float64, \
                 `job` LowCardinality(String), `$promql_series_identity` String, \
                 INDEX ts_minmax ts TYPE minmax GRANULARITY 1) \
                 ENGINE = MergeTree ORDER BY (`$promql_series_identity`, ts)"
            ),
        ]
    );
    assert!(deployment.materialized_views.is_empty());
    // Query order = root order [4, 3, 8, 2, 13].
    assert_eq!(deployment.queries.len(), 5);
    let over_node_1 = |aggregate: &str| {
        normalized(&format!(
            "SELECT `$promql_series_identity`, {aggregate} AS value FROM src_latency \
             WHERE ts > fromUnixTimestamp64Milli({{t:Int64}} - 300000) \
             AND ts <= fromUnixTimestamp64Milli({{t:Int64}}) \
             GROUP BY `$promql_series_identity`"
        ))
    };
    assert_eq!(
        normalized(&deployment.queries[0]),
        over_node_1("quantileExactInclusive(0.99)(value)")
    );
    assert_eq!(
        normalized(&deployment.queries[1]),
        over_node_1("quantileExactInclusive(0.5)(value)")
    );
    assert_eq!(
        normalized(&deployment.queries[3]),
        over_node_1("max(value)")
    );
    assert_eq!(
        normalized(&deployment.queries[2]),
        normalized(
            "SELECT `job`, avg(value) AS value FROM ( \
             SELECT `job`, `$promql_series_identity`, quantileExactInclusive(0.9)(value) AS value \
             FROM src_latency \
             WHERE ts > fromUnixTimestamp64Milli({t:Int64} - 300000) \
             AND ts <= fromUnixTimestamp64Milli({t:Int64}) \
             GROUP BY `job`, `$promql_series_identity`) \
             GROUP BY `job`"
        )
    );
    // Root 13: the query-time rate, the same SQL as F3's query.
    let f3_query = golden("f3.sql");
    let f3_query = &f3_query[f3_query.find("SELECT").unwrap()..];
    assert_eq!(
        normalized(&format!("{};", deployment.queries[4])),
        normalized(f3_query)
    );
    assert_eq!(
        deployment.sharing_lost,
        vec![SharingLost {
            node: 1,
            readers: vec![2, 3, 4]
        }]
    );
}

#[test]
fn stored_chain_keeps_its_scan_predicates() {
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 0)["predicates"] = json!([{"Compare": {
        "left": {"Column": 2}, "op": "Eq",
        "right": {"Literal": {"Utf8": "a"}}, "semantics": "promql"}}]);
    let deployment = translate(&PlanFile::from_value(value).unwrap()).unwrap();
    assert!(
        normalized(&deployment.materialized_views[0])
            .contains("FROM src_latency WHERE `job` = 'a' GROUP BY"),
        "{}",
        deployment.materialized_views[0]
    );
}

#[test]
fn stored_reads_need_whole_windows_and_aligned_times() {
    assert_eq!(
        translated("sot_1h_stored_sum").query_time_steps_ms,
        vec![60_000]
    );
    assert_eq!(
        translated("rate_5m_query_time").query_time_steps_ms,
        vec![1]
    );
    assert_eq!(translated("joint_5m").query_time_steps_ms, vec![1; 5]);
    // A 90 s range over 60 s windows.
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 2)["range"] = json!({"secs": 90, "nanos": 0});
    assert_eq!(
        refusal(value),
        shape(2, "a range that is not a whole number of windows")
    );
}

#[test]
fn query_time_results_kept_across_evaluations_are_refused() {
    // The ClickHouse files say `query_time_results_retained: false`.
    let mut value = fixture_value("joint_5m");
    value["dag"]["nodes"][1]["lifetime"] = json!({"AcrossEvaluations": "Memory"});
    assert_eq!(
        refusal(value),
        shape(1, "query-time result kept across evaluations")
    );
}

#[test]
fn unknown_operator_is_refused_at_its_node() {
    // M2
    let mut value = fixture_value("sot_1h_stored_sum");
    value["dag"]["nodes"][3]["payload"] = json!({"ASAP": {"FancyNewOp": {"children": [2]}}});
    assert_eq!(refusal(value), shape(3, "operator ASAP.FancyNewOp"));
}

#[test]
fn answer_changing_fields_are_refused_at_their_node() {
    // M3: a regrouping merge of stored windows.
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 3)["group_by"] = json!([2]);
    assert_eq!(
        refusal(value),
        shape(3, "regrouping merge of stored windows")
    );

    // M4: `without` grouping.
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 5)["reduction"] = json!({"Reduce": {"without": [2]}});
    assert_eq!(refusal(value), shape(5, "without"));

    // M6: a predicate other than label equality.
    let mut value = fixture_value("max_5m_label_filter");
    payload_body(&mut value, 0)["predicates"][0]["Compare"]["op"] = json!("Ne");
    assert_eq!(refusal(value), shape(0, "predicate"));

    // M8: Aggregate filters, one per measure.
    let mut value = fixture_value("joint_5m");
    payload_body(&mut value, 2)["filters"] = json!([{"Literal": {"Boolean": true}}]);
    assert_eq!(refusal(value), shape(2, "filter"));
    // L2: a measure without a filter (`[null]`) is no filter.
    let mut value = fixture_value("joint_5m");
    payload_body(&mut value, 2)["filters"] = json!([null]);
    assert_eq!(
        translate(&PlanFile::from_value(value).unwrap()).unwrap(),
        translated("joint_5m")
    );

    // L1: label equality with SQL semantics.
    let mut value = fixture_value("max_5m_label_filter");
    payload_body(&mut value, 0)["predicates"][0]["Compare"]["semantics"] = json!("sql");
    assert_eq!(refusal(value), shape(0, "predicate"));

    // M9: Aggregate having.
    let mut value = fixture_value("joint_5m");
    payload_body(&mut value, 2)["having"] = json!({"Literal": {"Boolean": true}});
    assert_eq!(refusal(value), shape(2, "having"));

    // M10: a chain filter.
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 1)["filter"] = json!({"Literal": {"Boolean": true}});
    assert_eq!(refusal(value), shape(1, "filter"));

    // M11: a summary input that is not the sample value.
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 1)["input"]["weight"] = json!({"Constant": 1.0});
    assert_eq!(refusal(value), shape(1, "summary input"));

    // M12: a SQL table source.
    let mut value = fixture_value("max_5m_label_filter");
    payload_body(&mut value, 0)["source"] = json!({"Table": {"table_ref": "t"}});
    assert_eq!(refusal(value), shape(0, "table source"));
}

#[test]
fn irate_state_is_refused() {
    // M7: the family is read from `summary_type` only.
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 1)["summary_type"] = json!({"ExactAggregate": ["IRate", "IRate"]});
    assert_eq!(refusal(value), shape(1, "exact IRate"));
}

#[test]
fn retention_is_recorded_and_ttl_is_separate() {
    // T2: the chain keeps 1 h of windows; its Scan runs at ingestion only, so
    // the source table has no raw-data retention.
    let deployment = translated("sot_1h_stored_sum");
    assert_eq!(
        deployment.table_retention,
        vec![TableRetention {
            table: "v1".into(),
            row_time_sql: "toDateTime(intDiv(window_end_ms, 1000))".into(),
            retention_ms: 3_600_000,
        }]
    );
    assert_eq!(
        deployment.ttl_statements(),
        vec!["ALTER TABLE v1 MODIFY TTL toDateTime(intDiv(window_end_ms, 1000)) + toIntervalSecond(3600)"]
    );
    assert!(!deployment.script().contains("TTL"));
    // A query-time Scan keeps the 5 m its range reads.
    assert_eq!(
        translated("rate_5m_query_time").table_retention,
        vec![TableRetention {
            table: "src_requests_total".into(),
            row_time_sql: "toDateTime(ts)".into(),
            retention_ms: 300_000,
        }]
    );
}

#[test]
fn grouped_query_time_summary_groups_by_its_columns() {
    // G1: the showcase q4c shape, `sum by (job) (sum_over_time(latency[1m]))`
    // every 10 s: a query-time SummaryAgg(Sum) reduced to `job`.
    let deployment = translate(&public_fixture(
        "q4c_sum_over_time__1m_1hz_10s",
        "clickhouse.equivalent-only",
    ))
    .expect("translates");
    assert_eq!(
        normalized(&deployment.script()),
        normalized(
            "CREATE TABLE src_latency (ts DateTime64(3, 'UTC'), value Float64, `job` LowCardinality(String), \
             `$promql_series_identity` String, INDEX ts_minmax ts TYPE minmax GRANULARITY 1) \
             ENGINE = MergeTree ORDER BY (`$promql_series_identity`, ts);\n\n\
             SELECT `job`, sum(value) AS value FROM src_latency \
             WHERE ts > fromUnixTimestamp64Milli({t:Int64} - 60000) \
             AND ts <= fromUnixTimestamp64Milli({t:Int64}) GROUP BY `job`;"
        )
    );
    assert_eq!(deployment.query_time_steps_ms, vec![1]);
    assert!(deployment.substitutions.is_empty());
}

#[test]
fn top_k_keeps_k_series_per_group() {
    // G5: `topk(10, sum by (instance) (rate(requests_total[5m])))`.
    let deployment = translate(&public_fixture(
        "topk__5m__1hz__60s",
        "clickhouse.equivalent-only",
    ))
    .expect("translates");
    let query = normalized(&deployment.queries[0]);
    assert!(
        query.starts_with(
            "SELECT `instance`, value FROM ( SELECT `instance`, sum(value) AS value FROM ( \
             SELECT `instance`, `$promql_series_identity`, increase * ("
        ),
        "{query}"
    );
    assert!(
        query.ends_with(") GROUP BY `instance`) ORDER BY value DESC, `instance` LIMIT 10"),
        "{query}"
    );
    // `topk(4, ...) by (region)` in mixed-03 (root 21): k per region.
    let deployment =
        translate(&public_fixture("mixed-03", "clickhouse.equivalent-only")).expect("translates");
    assert!(normalized(&deployment.queries[7]).ends_with(
        "ORDER BY value DESC, `region`, `service`, `$promql_series_identity` LIMIT 8 BY `region`, `service`"
    ));
}

#[test]
fn instant_selector_reads_each_series_latest_sample() {
    // G6: `sum by (job) (data)`; the plan's lookback is 5 ms.
    let deployment =
        translate(&public_fixture("rangeheavy", "clickhouse.equivalent-only")).expect("translates");
    assert_eq!(
        normalized(&deployment.queries[0]),
        normalized(
            "SELECT `job`, sum(value) AS value FROM ( \
             SELECT `job`, `$promql_series_identity`, argMax(value, ts) AS value FROM src_data \
             WHERE ts > fromUnixTimestamp64Milli({t:Int64} - 5) \
             AND ts <= fromUnixTimestamp64Milli({t:Int64}) \
             GROUP BY `job`, `$promql_series_identity`) GROUP BY `job`"
        )
    );
}

#[test]
fn average_as_sum_over_count_translates() {
    // G2, G4, G7, G8: mixed-03 root 11 is `avg by (region, service)
    // (quantile_over_time(0.9, data[100s]))`, planned as the sum and the
    // count of the per-series p90s by (job, region, service), each regrouped
    // to (region, service), the count cast to Float64, then divided.
    let plan = public_fixture("mixed-03", "clickhouse.equivalent-only");
    let deployment = translate(&plan).expect("translates");
    assert_eq!(plan.roots[3], 11);
    let query = normalized(&deployment.queries[3]);
    let p90 = "SELECT `job`, `region`, `service`, `$promql_series_identity`, \
               quantileExactInclusive(0.9)(value) AS value FROM src_data \
               WHERE ts > fromUnixTimestamp64Milli({t:Int64} - 100000) \
               AND ts <= fromUnixTimestamp64Milli({t:Int64}) \
               GROUP BY `job`, `region`, `service`, `$promql_series_identity`";
    let regrouped = |f: &str| {
        format!(
            "SELECT `region`, `service`, sum(value) AS value FROM ( \
             SELECT `job`, `region`, `service`, {f} AS value FROM ( {p90}) \
             GROUP BY `job`, `region`, `service`) GROUP BY `region`, `service`"
        )
    };
    let count = format!(
        "SELECT `region` AS `region`, `service` AS `service`, toFloat64(value) AS value FROM ( {})",
        regrouped("count()")
    );
    let expected = format!(
        "SELECT `region` AS `region`, `service` AS `service`, value AS value FROM ( \
         SELECT `region`, `service`, toFloat64(lhs_value) / toFloat64(rhs_value) AS value \
         FROM ( SELECT `region`, `service`, value AS lhs_value FROM ( {sum})) AS lhs \
         INNER JOIN ( SELECT `region`, `service`, value AS rhs_value FROM ( {count})) AS rhs \
         USING (`region`, `service`) \
         WHERE throwIf(NOT (isFinite(toFloat64(lhs_value)) AND isFinite(toFloat64(rhs_value)) \
         AND isFinite(toFloat64(lhs_value) / toFloat64(rhs_value))), \
         'a checked division has a value that is not finite') = 0)",
        sum = regrouped("sum(value)"),
    );
    assert_eq!(query, normalized(&expected));
    // L8: each query's step is the window width of the stored chains it
    // reads: roots 33 and 34 read 1 s windows, 40 1 s, 45 30 s.
    assert_eq!(
        deployment.query_time_steps_ms,
        vec![1000, 1000, 1, 1, 1, 1000, 30_000, 1, 1]
    );
    // G9: root 34 sums column 3 (`sum`) of node 33's output.
    assert!(normalized(&deployment.queries[1])
        .starts_with("SELECT `region`, `service`, sum(value) AS value FROM ("));
    assert!(deployment.substitutions.is_empty());
}

#[test]
fn unsupported_binary_operators_and_projections_are_refused() {
    let mixed = || fixture_value_public("mixed-03", "clickhouse.equivalent-only");
    let mut value = mixed();
    payload_body(&mut value, 10)["operator"]["vector_match"] = json!({"card": "OneToOne"});
    assert_eq!(refusal(value), shape(10, "vector matching"));
    let mut value = mixed();
    payload_body(&mut value, 10)["return_bool"] = json!(true);
    assert_eq!(refusal(value), shape(10, "bool comparison"));
    let mut value = mixed();
    payload_body(&mut value, 10)["operator"]["checked_relative_division"] = json!(true);
    assert_eq!(refusal(value), shape(10, "checked relative division"));
    let mut value = mixed();
    payload_body(&mut value, 10)["operator"]["kind"] = json!({"Arithmetic": "Mod"});
    assert_eq!(
        refusal(value),
        shape(10, r#"binary operator {"Arithmetic":"Mod"}"#)
    );
    let mut value = mixed();
    payload_body(&mut value, 6)["cols"][2]["expr"] = json!({"Literal": {"Float64": 1.0}});
    assert_eq!(refusal(value), shape(6, "projected expression"));
    // G9: a measure over a label column.
    let mut value = mixed();
    payload_body(&mut value, 34)["measures"][0]["col"] = json!(0);
    assert_eq!(refusal(value), shape(34, "measure over a label column"));
}

#[test]
fn grouped_stored_chain_groups_its_view() {
    // G3: a chain reduced to `job` (output without the series identity).
    let mut value = fixture_value("sot_1h_stored_sum");
    payload_body(&mut value, 1)["reduction"] = json!({"Reduce": [2]});
    value["dag"]["nodes"][1]["output_schema"]["fields"]
        .as_array_mut()
        .unwrap()
        .retain(|field| field["name"] != "$promql_series_identity");
    let deployment = translate(&PlanFile::from_value(value).unwrap()).expect("translates");
    assert_eq!(
        normalized(&deployment.tables[1]),
        normalized(
            "CREATE TABLE v1 (`job` LowCardinality(String), window_end_ms Int64, \
             state AggregateFunction(sum, Float64)) \
             ENGINE = AggregatingMergeTree ORDER BY (window_end_ms, `job`)"
        )
    );
    assert!(normalized(&deployment.materialized_views[0])
        .ends_with("FROM src_latency GROUP BY `job`, window_end_ms"));
}

#[test]
fn a_label_filter_leads_the_source_sorting_key() {
    // L4 (and F4's golden): `job` is filtered on, so it leads the key.
    let deployment = translated("max_5m_label_filter");
    assert!(normalized(&deployment.tables[0])
        .ends_with("ORDER BY (`job`, `$promql_series_identity`, ts)"));
}

/// X10: the goldens run on ClickHouse 26.8.39.13 (the 26.8 LTS build for CPUs without AVX2) and give the hand-computed
/// answers. Needs a `clickhouse local` of that release: set
/// `AQPBM_CLICKHOUSE_LOCAL` to a shell command that runs it reading SQL on
/// stdin (e.g. `clickhouse local`, or the same through `docker run -i` or
/// `ssh`). The query parameter `t` is appended as `--param_t=<ms>`.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn goldens_run_on_clickhouse() {
    let command = std::env::var("AQPBM_CLICKHOUSE_LOCAL").expect("AQPBM_CLICKHOUSE_LOCAL");
    assert_eq!(version(&command, "version()"), "26.8.39.13");
    // L6: the 26.8 amd64compat build in use (sha256 aae73dc5…fe36ca) reports this git hash.
    assert_eq!(
        version(
            &command,
            "(SELECT value FROM system.build_options WHERE name = 'VERSION_GITHASH')"
        ),
        "d5bbb64d525197f84f9dd21489f5f6d6fdb966e5"
    );
    // F1: series a/1 = 1.0, a/2 = 0.5, b/1 = 2.0 every second for 1 h; at t = 1 h:
    // a = 3600 · 1.0 + 3600 · 0.5 = 5400, b = 3600 · 2.0 = 7200.
    let latency_three = "INSERT INTO src_latency SELECT \
        fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), v, j, id FROM numbers(3600) \
        ARRAY JOIN [1.0, 0.5, 2.0] AS v, ['a', 'a', 'b'] AS j, \
        ['latency{job=\"a\",i=\"1\"}', 'latency{job=\"a\",i=\"2\"}', 'latency{job=\"b\",i=\"1\"}'] AS id \
        ORDER BY 1";
    assert_eq!(
        run(&command, "f1.sql", latency_three, 3_600_000),
        "a\t5400\nb\t7200"
    );
    // F2/F3: three counters, each in its own job; at t = 600 s the window is
    // (300 s, 600 s]; Prometheus extrapolatedRate (v3.15.1) by hand:
    // x: v(s) = s for s <= 450, then s - 450 (a reset at 451 s). Samples 301..600
    //    s: n = 300, first 301, last 150, one drop after 450: increase
    //    150 - 301 + 450 = 299; sampled 299 s; average gap 1 s; start gap 1 s
    //    (< 1.1, kept; zero point 299 · 301 / 299 = 301 s is later); end gap 0.
    //    rate = 299 · (299 + 1 + 0) / 299 / 300 = 1.
    // y: v(s) = s from 310 s: start gap 10 s >= 1.1, so half a gap, 0.5 s;
    //    increase 290, sampled 290: rate = 290 · (290 + 0.5) / 290 / 300 = 290.5 / 300.
    // z: v(s) = 2 (s - 301) + 0.5: increase 598, sampled 299; the zero point
    //    299 · 0.5 / 598 = 0.25 s is before the 1 s start gap, so 0.25:
    //    rate = 598 · (299 + 0.25) / 299 / 300 = 598.5 / 300.
    // Stored windows: deltaSumTimestamp adds 0 at x's drop, so x's increase is
    // 149 + 149 = 298 and its rate 298 · 300 / 299 / 300 = 298 / 299 (lower by
    // the first value after the reset, 1); y and z have no reset and agree.
    let counters = "INSERT INTO src_requests_total SELECT \
        fromUnixTimestamp64Milli(toInt64(s) * 1000), v, j, concat('requests_total{job=\"', j, '\"}') \
        FROM (SELECT number + 1 AS s, 'x' AS j, if(s <= 450, s, s - 450) AS v FROM numbers(600) \
              UNION ALL SELECT number + 310, 'y', number + 310 FROM numbers(291) \
              UNION ALL SELECT number + 301, 'z', 2 * number + 0.5 FROM numbers(300)) \
        ORDER BY 1";
    let expected_query_time = [("x", 1.0), ("y", 290.5 / 300.0), ("z", 598.5 / 300.0)];
    assert_rates(
        &run(&command, "f3.sql", counters, 600_000),
        &expected_query_time,
    );
    let expected_stored = [
        ("x", 298.0 / 299.0),
        ("y", 290.5 / 300.0),
        ("z", 598.5 / 300.0),
    ];
    assert_rates(
        &run(&command, "f2.sql", counters, 600_000),
        &expected_stored,
    );
    // F4: j3 = s, j4 = 10 s; max over (300 s, 600 s] of j3 only = 600.
    let latency_two = "INSERT INTO src_latency SELECT \
        fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), (number + 1) * m, j, id \
        FROM numbers(600) ARRAY JOIN [1, 10] AS m, ['j3', 'j4'] AS j, \
        ['latency{job=\"j3\"}', 'latency{job=\"j4\"}'] AS id ORDER BY 1";
    assert_eq!(
        run(&command, "f4.sql", latency_two, 600_000),
        "j3\tlatency{job=\"j3\"}\t600"
    );
    // F5: three instances; 3 at 600 s, and no row once the window is empty.
    let instances = "INSERT INTO src_latency SELECT \
        fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), 1.0, i, \
        concat('latency{instance=\"', i, '\"}') FROM numbers(600) \
        ARRAY JOIN ['i1', 'i2', 'i3'] AS i ORDER BY 1";
    assert_eq!(run(&command, "f5.sql", instances, 600_000), "3");
    // 9,960,000 ms is a window end (166 windows of 60 s), long after the data.
    assert_eq!(run(&command, "f5.sql", instances, 9_960_000), "");
}

/// `job<TAB>rate` rows equal `expected` to 1e-9 (relative).
fn assert_rates(rows: &str, expected: &[(&str, f64)]) {
    let got: Vec<(String, f64)> = rows
        .lines()
        .map(|row| {
            let (job, rate) = row.split_once('\t').expect("job<TAB>rate");
            (job.to_string(), rate.parse().expect("a number"))
        })
        .collect();
    assert_eq!(got.len(), expected.len(), "{rows}");
    for ((job, rate), (want_job, want)) in got.iter().zip(expected) {
        assert_eq!(job, want_job);
        assert!(
            (rate - want).abs() <= 1e-9 * want.abs(),
            "{job}: {rate} vs {want}"
        );
    }
}

fn version(command: &str, expression: &str) -> String {
    use std::io::Write;
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("clickhouse local");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("SELECT {expression} FORMAT TSV;\n").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Runs a golden's CREATE statements, then `insert`, then its query at `t`;
/// returns the TSV rows, sorted, joined by newlines.
fn run(command: &str, golden_name: &str, insert: &str, t: i64) -> String {
    use std::io::Write;
    let text: String = golden(golden_name)
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let statements: Vec<&str> = text
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut script = String::new();
    for statement in statements.iter().filter(|s| s.starts_with("CREATE")) {
        script.push_str(statement);
        script.push_str(";\n");
    }
    script.push_str(insert);
    script.push_str(";\n");
    for statement in statements.iter().filter(|s| !s.starts_with("CREATE")) {
        script.push_str(statement);
        script.push_str(" FORMAT TSV;\n");
    }
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{command} --param_t={t}"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("clickhouse local");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "clickhouse local failed on {golden_name}"
    );
    let mut rows: Vec<&str> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .collect();
    rows.sort_unstable();
    rows.join("\n")
}

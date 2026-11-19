//! Translations run on ClickHouse 26.8.39.13 (`#[ignore]`: set
//! `AQPBM_CLICKHOUSE_LOCAL` to a shell command that runs its `clickhouse
//! local` reading SQL on stdin; `t` is appended as `--param_t=<ms>`).
//! Expected answers are computed by hand in the comments.

mod common;

use aqpbm_engines_external::clickhouse::{translate, ClickHouseDeployment, INSERT_SETTINGS};
use aqpbm_engines_external::plan_file::PlanFile;
use common::{
    fixture, fixture_value_public, least_common_multiple, payload_body, public_fixture,
    run_clickhouse,
};
use serde_json::json;

/// The deployment's tables and views, then `setup`, then query `query`.
fn script(deployment: &ClickHouseDeployment, setup: &[String], query: usize) -> String {
    let mut script = String::new();
    for statement in deployment
        .tables
        .iter()
        .chain(&deployment.materialized_views)
        .chain(setup)
    {
        script.push_str(statement);
        script.push_str(";\n");
    }
    script.push_str(&deployment.queries[query]);
    script.push_str(" FORMAT TSV;\n");
    script
}

/// `INSERT INTO <table> (columns) SETTINGS <INSERT_SETTINGS> <select>`.
fn insert(table: &str, columns: &str, select: &str) -> String {
    format!("INSERT INTO {table} ({columns}) SETTINGS {INSERT_SETTINGS} {select}")
}

/// `label<TAB>value` rows as (label, value).
fn parse(rows: &str) -> Vec<(String, f64)> {
    rows.lines()
        .map(|row| {
            let (label, value) = row.rsplit_once('\t').expect("label<TAB>value");
            let value = match value {
                "nan" => f64::NAN,
                "inf" => f64::INFINITY,
                v => v.parse().expect("a number"),
            };
            (label.to_string(), value)
        })
        .collect()
}

fn assert_close(rows: &str, expected: &[(&str, f64)]) {
    let got = parse(rows);
    assert_eq!(got.len(), expected.len(), "{rows}");
    for (label, want) in expected {
        let value = got
            .iter()
            .find(|(l, _)| l == label)
            .unwrap_or_else(|| panic!("no {label} in {rows}"))
            .1;
        assert!(
            (value - want).abs() <= 1e-9 * want.abs().max(1.0),
            "{label}: {value} vs {want}"
        );
    }
}

/// 60 inserts into `src_requests_total` of 10 s of `rows` each, in time
/// order. `rows` is a `SELECT` with the columns `ts, value, job, id` and the
/// sample time `s_ms`.
fn ten_second_inserts(rows: &str) -> Vec<String> {
    (0..60)
        .map(|chunk| {
            insert(
                "src_requests_total",
                "ts, value, job, `$promql_series_identity`",
                &format!(
                    "SELECT ts, value, job, id FROM ({rows}) \
                     WHERE s_ms > {} AND s_ms <= {} ORDER BY s_ms",
                    chunk * 10_000,
                    (chunk + 1) * 10_000
                ),
            )
        })
        .collect()
}

/// 2,000 counters, `v(s) = s · m` with `m = 1 + k mod 7` and job
/// `j<k mod 10>`, samples every second for 600 s, 60 inserts. At t = 600 s
/// over (300 s, 600 s]: n = 300, first 301 s, increase 299 m, sampled 299 s,
/// average gap 1 s, start gap 1 s (kept, < 1.1; the zero point 301 s is
/// later), end gap 0: rate = 299 m · 300 / 299 / 300 = m. Per job, the sum of
/// m over its 200 counters.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn stored_rate_is_right_under_parallel_reads() {
    let deployment = translate(&fixture("rate_5m_stored")).unwrap();
    let rows = "SELECT fromUnixTimestamp64Milli(toInt64(s * 1000)) AS ts, \
                toFloat64(s * (1 + k % 7)) AS value, concat('j', toString(k % 10)) AS job, \
                concat('requests_total{k=\"', toString(k), '\"}') AS id, s * 1000 AS s_ms \
                FROM (SELECT number + 1 AS s FROM numbers(600)) AS samples \
                CROSS JOIN (SELECT number AS k FROM numbers(2000)) AS counters";
    let inserts = ten_second_inserts(rows);
    let expected: Vec<(String, f64)> = (0..10)
        .map(|job| {
            let sum: u64 = (0..2000u64)
                .filter(|k| k % 10 == job)
                .map(|k| 1 + k % 7)
                .sum();
            (format!("j{job}"), sum as f64)
        })
        .collect();
    let expected: Vec<(&str, f64)> = expected.iter().map(|(j, v)| (j.as_str(), *v)).collect();
    let setup = |before: &[&str], after: &[&str]| -> Vec<String> {
        before
            .iter()
            .map(|s| s.to_string())
            .chain(inserts.iter().cloned())
            .chain(after.iter().map(|s| s.to_string()))
            .collect()
    };
    // Merges stopped: one stored row per insert and window.
    for threads in [1, 16, 40] {
        let setup = setup(
            &["SYSTEM STOP MERGES"],
            &[&format!("SET max_threads = {threads}")],
        );
        let rows = run_clickhouse(&script(&deployment, &setup, 0), 600_000).unwrap();
        assert_close(&rows, &expected);
    }
    // All merged.
    let setup = setup(&[], &["OPTIMIZE TABLE v1 FINAL", "SET max_threads = 16"]);
    assert_close(
        &run_clickhouse(&script(&deployment, &setup, 0), 600_000).unwrap(),
        &expected,
    );
    // Partly merged: 30 inserts merged, 30 not.
    let mut setup: Vec<String> = inserts[..30].to_vec();
    setup.push("OPTIMIZE TABLE v1 FINAL".into());
    setup.push("SYSTEM STOP MERGES".into());
    setup.extend(inserts[30..].iter().cloned());
    setup.push("SET max_threads = 16".into());
    assert_close(
        &run_clickhouse(&script(&deployment, &setup, 0), 600_000).unwrap(),
        &expected,
    );
}

/// Counter resets, each series in its own job, 60 inserts of 10 s,
/// merges stopped, read at t = 600 s over (300 s, 600 s]. In every series
/// below the start gap is 1 s or less than 1.1 average gaps and the end gap
/// 0 or half a gap, so the rate is the increase · 300 / 299 / 300 =
/// increase / 299 for x, y, z (written as such to show the increase):
/// * x: `s` until 450 s, then `s − 450`: a reset at an insert boundary inside
///   a window (451 s). PromQL increase 150 − 301 + 450 = 299; stored 149 +
///   149 = 298 (lower by the first value after the reset, 1).
/// * y: `3 s` until 360 s, then `s − 360`: a reset at a window boundary
///   (361 s). PromQL 240 − 903 + 1080 = 417; stored 177 + 239 = 416.
/// * z: `s` until 399 s, `s − 5` until 499 s, then `(s − 500) / 2`: a partial
///   drop (399 → 395) and a reset (494 → 0), both inside an insert. PromQL
///   50 − 301 + 399 + 494 = 642; stored 98 + 99 + 50 = 247 (lower by 395 +
///   0).
/// * w: `s` at 5, 15, …, 595 s, one sample per insert, so every stored row
///   holds one sample: 30 samples, increase 290, sampled 290 s, average gap
///   10 s, start and end gaps 5 s: rate 290 · 300 / 290 / 300 = 1, both
///   ways.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn stored_rate_differs_from_promql_by_the_first_value_after_each_reset() {
    let rows = "SELECT fromUnixTimestamp64Milli(toInt64(s * 1000)) AS ts, v AS value, j AS job, \
                concat('requests_total{job=\"', j, '\"}') AS id, s * 1000 AS s_ms FROM ( \
                SELECT number + 1 AS s, 'x' AS j, toFloat64(if(s <= 450, s, s - 450)) AS v FROM numbers(600) \
                UNION ALL SELECT number + 1, 'y', toFloat64(if(number + 1 <= 360, 3 * (number + 1), number + 1 - 360)) FROM numbers(600) \
                UNION ALL SELECT number + 1, 'z', multiIf(number + 1 < 400, toFloat64(number + 1), \
                  number + 1 < 500, toFloat64(number + 1 - 5), (number + 1 - 500) / 2) FROM numbers(600) \
                UNION ALL SELECT number * 10 + 5, 'w', toFloat64(number * 10 + 5) FROM numbers(60))";
    let inserts = ten_second_inserts(rows);
    let setup: Vec<String> = ["SYSTEM STOP MERGES".to_string()]
        .into_iter()
        .chain(inserts)
        .chain(["SET max_threads = 16".to_string()])
        .collect();
    let stored = translate(&fixture("rate_5m_stored")).unwrap();
    assert_close(
        &run_clickhouse(&script(&stored, &setup, 0), 600_000).unwrap(),
        &[
            ("x", 298.0 / 299.0),
            ("y", 416.0 / 299.0),
            ("z", 247.0 / 299.0),
            ("w", 1.0),
        ],
    );
    let query_time = translate(&fixture("rate_5m_query_time")).unwrap();
    assert_close(
        &run_clickhouse(&script(&query_time, &setup, 0), 600_000).unwrap(),
        &[
            ("x", 299.0 / 299.0),
            ("y", 417.0 / 299.0),
            ("z", 642.0 / 299.0),
            ("w", 1.0),
        ],
    );
}

/// Overlapping segments (an insert that goes back in time) are an error, not
/// a wrong rate.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn overlapping_stored_segments_are_an_error() {
    let deployment = translate(&fixture("rate_5m_stored")).unwrap();
    let rows = |from: u64, to: u64| {
        insert(
            "src_requests_total",
            "ts, value, job, `$promql_series_identity`",
            &format!(
                "SELECT fromUnixTimestamp64Milli(toInt64(s * 1000)), toFloat64(s), 'x', 'x' \
                 FROM (SELECT number + {from} AS s FROM numbers({})) ORDER BY 1",
                to - from + 1
            ),
        )
    };
    // The second insert goes back to 400 s, inside the window it reads.
    let setup = vec!["SYSTEM STOP MERGES".into(), rows(1, 450), rows(400, 600)];
    let error = run_clickhouse(&script(&deployment, &setup, 0), 600_000).unwrap_err();
    assert!(error.contains("overlap in time"), "{error}");
}

/// A stored read at a time that is not a window end fails.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn unaligned_evaluation_time_is_an_error() {
    let deployment = translate(&fixture("sum_over_time_1h_stored_sum")).unwrap();
    let setup = vec![insert(
        "src_latency",
        "ts, value, job, `$promql_series_identity`",
        "SELECT fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), 1.0, 'a', 'a' \
         FROM numbers(7200) ORDER BY 1",
    )];
    let error = run_clickhouse(&script(&deployment, &setup, 0), 7_230_000).unwrap_err();
    assert!(
        error.contains("not a multiple of the window width, 60000 ms"),
        "{error}"
    );
    // At a window end: 3600 one-valued samples in (3600 s, 7200 s].
    assert_eq!(
        run_clickhouse(&script(&deployment, &setup, 0), 7_200_000).unwrap(),
        "a\t3600"
    );
}

/// The TTL statements are accepted, and a NULL value is refused
/// under `INSERT_SETTINGS` in both a `VALUES` and an `INSERT … SELECT`.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn ttl_statements_apply_and_null_values_are_refused() {
    for name in ["sum_over_time_1h_stored_sum", "rate_5m_query_time"] {
        let deployment = translate(&fixture(name)).unwrap();
        let mut setup = deployment.ttl_statements();
        assert!(!setup.is_empty());
        setup.push("SELECT 1 FORMAT Null".into());
        run_clickhouse(&script(&deployment, &setup, 0), 0).expect("TTL statements apply");
    }
    let deployment = translate(&fixture("max_5m_label_filter")).unwrap();
    for insert_null in [
        format!(
            "INSERT INTO src_latency (ts, value, job, `$promql_series_identity`) \
             SETTINGS {INSERT_SETTINGS} VALUES (1000, NULL, 'j3', 'a')"
        ),
        format!(
            "INSERT INTO src_latency (ts, value, job, `$promql_series_identity`) \
             SETTINGS {INSERT_SETTINGS} SELECT toDateTime64(1, 3), CAST(NULL, 'Nullable(Float64)'), 'j3', 'a'"
        ),
    ] {
        let script = script(&deployment, &[insert_null], 0);
        let error = run_clickhouse(&script, 600_000).unwrap_err();
        assert!(error.contains("NULL"), "{error}");
    }
}

/// The showcase sum_over_time shape at 1 m, `sum by (job) (sum_over_time(latency[1m]))`: job a has
/// two series, 1.0 and 0.5 each second, job b one series of 2.0; over
/// (60 s, 120 s]: a = 60 · 1.5 = 90, b = 120.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn grouped_query_time_sum_answers() {
    let deployment = translate(&public_fixture(
        "sum_over_time__1m_1hz_10s",
        "clickhouse.equivalent-only",
    ))
    .unwrap();
    let setup = vec![insert(
        "src_latency",
        "ts, value, job, `$promql_series_identity`",
        "SELECT fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), v, j, id FROM numbers(120) \
         ARRAY JOIN [1.0, 0.5, 2.0] AS v, ['a', 'a', 'b'] AS j, ['a1', 'a2', 'b1'] AS id ORDER BY 1",
    )];
    assert_eq!(
        run_clickhouse(&script(&deployment, &setup, 0), 120_000).unwrap(),
        "a\t90\nb\t120"
    );
}

/// `topk(k, sum by (instance) (rate(requests_total[5m])))` with
/// instances i1..i5 at rate r (`v = r · s`) and i6 at NaN. k = 2 keeps i5 and
/// i4; k = 5 keeps i1..i5: NaN ranks below every number, as in PromQL.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn top_k_answers() {
    let setup = vec![insert(
        "src_requests_total",
        "ts, value, instance, `$promql_series_identity`",
        "SELECT fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), \
         if(r = 6, nan, toFloat64(r * (number + 1))), concat('i', toString(r)), concat('i', toString(r)) \
         FROM numbers(600) ARRAY JOIN [1, 2, 3, 4, 5, 6] AS r ORDER BY 1",
    )];
    for (k, expected) in [
        (2, "i4\t4\ni5\t5"),
        (5, "i1\t1\ni2\t2\ni3\t3\ni4\t4\ni5\t5"),
    ] {
        let mut value = fixture_value_public("topk__5m__1hz__60s", "clickhouse.equivalent-only");
        payload_body(&mut value, 5)["measures"][0]["k"] = json!(k);
        let deployment = translate(&PlanFile::from_value(value).unwrap()).unwrap();
        assert_eq!(
            run_clickhouse(&script(&deployment, &setup, 0), 600_000).unwrap(),
            expected,
            "k = {k}"
        );
    }
}

/// `sum by (job) (data)` with the plan's 5 ms lookback, at t = 100 s:
/// s1 (a) has samples at t − 10 ms (100) and t − 3 ms (1): latest 1; s2 (a)
/// only at t − 5 ms, outside (t − 5 ms, t]; s3 (a) at t (2); s4 (b) at
/// t − 1 ms (4). a = 1 + 2 = 3, b = 4.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn instant_selector_answers() {
    let deployment =
        translate(&public_fixture("range_heavy", "clickhouse.equivalent-only")).unwrap();
    let setup = vec![insert(
        "src_data",
        "ts, value, job, `$promql_series_identity`",
        "SELECT fromUnixTimestamp64Milli(toInt64(ms)), v, j, id FROM values('ms Int64, v Float64, j String, id String', \
         (99990, 100, 'a', 's1'), (99995, 50, 'a', 's2'), (99997, 1, 'a', 's1'), \
         (99999, 4, 'b', 's4'), (100000, 2, 'a', 's3')) ORDER BY ms",
    )];
    assert_eq!(
        run_clickhouse(&script(&deployment, &setup, 0), 100_000).unwrap(),
        "a\t3\nb\t4"
    );
}

/// mixed-03 root 11, `avg by (region, service)
/// (quantile_over_time(0.9, data[100s]))` as sum / count, at t = 100 s:
/// constant series (j1, r1, s1) = 1, (j2, r1, s1) = 3, (j1, r2, s1) = 5, so
/// each p90 is the constant: r1/s1 = (1 + 3) / 2 = 2, r2/s1 = 5. With two
/// p90s of 1.5e308 the sum overflows to inf: the checked division is an
/// error, not inf.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn average_as_sum_over_count_answers() {
    let deployment = translate(&public_fixture("mixed-03", "clickhouse.equivalent-only")).unwrap();
    let series = |values: &str| {
        vec![insert(
            "src_data",
            "ts, value, job, region, service, `$promql_series_identity`",
            &format!(
                "SELECT fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), v, j, r, 's1', concat(j, r) \
                 FROM numbers(100) ARRAY JOIN {values} ORDER BY 1"
            ),
        )]
    };
    let setup = series("[1.0, 3.0, 5.0] AS v, ['j1', 'j2', 'j1'] AS j, ['r1', 'r1', 'r2'] AS r");
    assert_eq!(
        run_clickhouse(&script(&deployment, &setup, 3), 100_000).unwrap(),
        "r1\ts1\t2\nr2\ts1\t5"
    );
    let setup = series("[1.5e308, 1.5e308] AS v, ['j1', 'j2'] AS j, ['r1', 'r1'] AS r");
    let error = run_clickhouse(&script(&deployment, &setup, 3), 100_000).unwrap_err();
    assert!(error.contains("checked division"), "{error}");
}

/// Every committed public translation runs on ClickHouse, on two series
/// per metric (every label empty but the series identity) sampled every
/// second for 2 h, at the first multiple of every query's step from 1 h.
/// This checks the SQL, not the answers.
#[test]
#[ignore = "needs ClickHouse 26.8.39.13 (AQPBM_CLICKHOUSE_LOCAL)"]
fn public_translations_run() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plans/public");
    let mut failures = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let plan = PlanFile::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let deployment = translate(&plan).unwrap();
        let step = deployment
            .query_time_steps_ms
            .iter()
            .fold(1, |a, &b| least_common_multiple(a, b));
        let t = 3_600_000u64.div_ceil(step) * step;
        let mut script = String::new();
        for statement in deployment
            .tables
            .iter()
            .chain(&deployment.materialized_views)
        {
            script.push_str(statement);
            script.push_str(";\n");
        }
        for table in deployment.tables.iter().filter_map(|t| {
            t.strip_prefix("CREATE TABLE src_")
                .map(|rest| format!("src_{}", rest.split(' ').next().unwrap()))
        }) {
            script.push_str(&insert(
                &table,
                "ts, value, `$promql_series_identity`",
                "SELECT fromUnixTimestamp64Milli(toInt64(number + 1) * 1000), toFloat64(number + 1), id \
                 FROM numbers(7200) ARRAY JOIN ['s1', 's2'] AS id ORDER BY 1",
            ));
            script.push_str(";\n");
        }
        for query in &deployment.queries {
            script.push_str(query);
            script.push_str(" FORMAT Null;\n");
        }
        if let Err(error) = run_clickhouse(&script, t as i64) {
            failures.push(format!("{}: {error}", path.display()));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

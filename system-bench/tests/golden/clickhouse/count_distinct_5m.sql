CREATE TABLE src_latency (ts DateTime64(3, 'UTC'), value Float64, `instance` LowCardinality(String), `$promql_series_identity` String)
  ENGINE = Null;

CREATE TABLE v1 (`instance` LowCardinality(String), `$promql_series_identity` String, window_end_ms Int64,
                 state AggregateFunction(count))
  ENGINE = AggregatingMergeTree ORDER BY (window_end_ms, `instance`, `$promql_series_identity`);

CREATE MATERIALIZED VIEW v1_mv TO v1 AS
  SELECT `instance`, `$promql_series_identity`,
         (intDiv(toUnixTimestamp64Milli(ts) - 1, 60000) + 1) * 60000 AS window_end_ms,
         countState() AS state
  FROM src_latency
  GROUP BY `instance`, `$promql_series_identity`, window_end_ms;

SELECT count() AS value
FROM (
  SELECT `instance`, count() AS value
  FROM (
    SELECT `instance`, `$promql_series_identity`, countMerge(state) AS value
    FROM v1
    WHERE window_end_ms > {t:Int64} - 300000 AND window_end_ms <= {t:Int64}
      AND (SELECT throwIf({t:Int64} % 60000 != 0, 'the evaluation time is not a multiple of the window width, 60000 ms')) = 0
    GROUP BY `instance`, `$promql_series_identity`)
  GROUP BY `instance`)
HAVING count() > 0;

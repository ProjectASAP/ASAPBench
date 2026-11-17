CREATE TABLE src_latency (ts DateTime64(3, 'UTC'), value Float64, `job` LowCardinality(String), `$promql_series_identity` String,
                          INDEX ts_minmax ts TYPE minmax GRANULARITY 1)
  ENGINE = MergeTree ORDER BY (`$promql_series_identity`, ts);

CREATE TABLE v1 (`job` LowCardinality(String), `$promql_series_identity` String, window_end_ms Int64,
                 state AggregateFunction(sum, Float64))
  ENGINE = AggregatingMergeTree ORDER BY (window_end_ms, `job`, `$promql_series_identity`);

CREATE MATERIALIZED VIEW v1_mv TO v1 AS
  SELECT `job`, `$promql_series_identity`,
         (intDiv(toUnixTimestamp64Milli(ts) - 1, 60000) + 1) * 60000 AS window_end_ms,
         sumState(value) AS state
  FROM src_latency
  GROUP BY `job`, `$promql_series_identity`, window_end_ms;

SELECT `job`, sum(value) AS value
FROM (
  SELECT `job`, `$promql_series_identity`, sumMerge(state) AS value
  FROM v1
  WHERE window_end_ms > {t:Int64} - 3600000 AND window_end_ms <= {t:Int64}
    AND (SELECT throwIf({t:Int64} % 60000 != 0, 'the evaluation time is not a multiple of the window width, 60000 ms')) = 0
  GROUP BY `job`, `$promql_series_identity`)
GROUP BY `job`;

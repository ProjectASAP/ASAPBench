CREATE TABLE src_latency (ts DateTime64(3, 'UTC'), value Float64, `job` String, `$promql_series_identity` String)
  ENGINE = MergeTree ORDER BY (ts, `$promql_series_identity`);

CREATE TABLE v1 (`job` String, `$promql_series_identity` String, window_end_ms Int64,
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
  GROUP BY `job`, `$promql_series_identity`)
GROUP BY `job`;

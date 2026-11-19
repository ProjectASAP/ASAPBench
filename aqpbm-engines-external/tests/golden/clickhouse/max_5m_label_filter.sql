CREATE TABLE src_latency (ts DateTime64(3, 'UTC'), value Float64, `job` LowCardinality(String), `$promql_series_identity` String,
                          INDEX ts_minmax ts TYPE minmax GRANULARITY 1)
  ENGINE = MergeTree ORDER BY (`job`, `$promql_series_identity`, ts);

SELECT `job`, `$promql_series_identity`, max(value) AS value
FROM src_latency
WHERE `job` = 'j3'
  AND ts > fromUnixTimestamp64Milli({t:Int64} - 300000) AND ts <= fromUnixTimestamp64Milli({t:Int64})
GROUP BY `job`, `$promql_series_identity`;

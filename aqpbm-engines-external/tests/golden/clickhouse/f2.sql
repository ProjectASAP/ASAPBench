-- Stored windows of a counter (a substitute): deltaSumTimestamp adds 0 for the step
-- across a counter reset. The query applies Prometheus extrapolatedRate
-- (promql/functions.go, v3.15.1) to the merged window states.
CREATE TABLE src_requests_total (ts DateTime64(3, 'UTC'), value Float64, `job` LowCardinality(String), `$promql_series_identity` LowCardinality(String))
  ENGINE = MergeTree ORDER BY (ts, `$promql_series_identity`);

CREATE TABLE v1 (`job` LowCardinality(String), `$promql_series_identity` LowCardinality(String), window_end_ms Int64,
                 s_count AggregateFunction(count),
                 s_first_ms AggregateFunction(min, Int64),
                 s_last_ms AggregateFunction(max, Int64),
                 s_first_value AggregateFunction(argMin, Float64, Int64),
                 s_increase AggregateFunction(deltaSumTimestamp, Float64, Int64))
  ENGINE = AggregatingMergeTree ORDER BY (window_end_ms, `job`, `$promql_series_identity`);

CREATE MATERIALIZED VIEW v1_mv TO v1 AS
  SELECT `job`, `$promql_series_identity`,
         (intDiv(toUnixTimestamp64Milli(ts) - 1, 60000) + 1) * 60000 AS window_end_ms,
         countState() AS s_count,
         minState(toUnixTimestamp64Milli(ts)) AS s_first_ms,
         maxState(toUnixTimestamp64Milli(ts)) AS s_last_ms,
         argMinState(value, toUnixTimestamp64Milli(ts)) AS s_first_value,
         deltaSumTimestampState(value, toUnixTimestamp64Milli(ts)) AS s_increase
  FROM src_requests_total
  GROUP BY `job`, `$promql_series_identity`, window_end_ms;

SELECT `job`, sum(value) AS value
FROM (
  SELECT `job`, `$promql_series_identity`,
         increase * (sampled + least(d_start, d_zero) + d_end) / sampled / 300 AS value
  FROM (
    SELECT `job`, `$promql_series_identity`, increase,
           (last_ms - first_ms) / 1000 AS sampled,
           sampled / (n - 1) AS avg_gap,
           if((first_ms - ({t:Int64} - 300000)) / 1000 >= 1.1 * avg_gap,
              avg_gap / 2, (first_ms - ({t:Int64} - 300000)) / 1000) AS d_start,
           if(increase > 0 AND v_first >= 0, sampled * v_first / increase, d_start) AS d_zero,
           if(({t:Int64} - last_ms) / 1000 >= 1.1 * avg_gap,
              avg_gap / 2, ({t:Int64} - last_ms) / 1000) AS d_end
    FROM (
      SELECT `job`, `$promql_series_identity`,
             countMerge(s_count) AS n,
             minMerge(s_first_ms) AS first_ms,
             maxMerge(s_last_ms) AS last_ms,
             argMinMerge(s_first_value) AS v_first,
             deltaSumTimestampMerge(s_increase) AS increase
      FROM v1
      WHERE window_end_ms > {t:Int64} - 300000 AND window_end_ms <= {t:Int64}
      GROUP BY `job`, `$promql_series_identity`
      HAVING n >= 2)))
GROUP BY `job`;

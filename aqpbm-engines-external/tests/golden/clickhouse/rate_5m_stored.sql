-- Stored windows of a counter (a substitute): deltaSumTimestamp adds 0 for the step
-- across a counter reset. Each stored row is one time-ordered segment of one series
-- (one insert's samples in one window, or adjacent ones merged). The query never merges
-- counter states across rows: it reads each segment, joins a series' segments in time
-- order (the step into a segment counts when it is an increase, as within a segment;
-- overlapping segments are an error), then applies Prometheus extrapolatedRate
-- (promql/functions.go, v3.15.1).
CREATE TABLE src_requests_total (ts DateTime64(3, 'UTC'), value Float64, `job` LowCardinality(String), `$promql_series_identity` String)
  ENGINE = Null;

CREATE TABLE v1 (`job` LowCardinality(String), `$promql_series_identity` String, window_end_ms Int64,
                 s_sample_count AggregateFunction(count),
                 s_first_ms AggregateFunction(min, Int64),
                 s_last_ms AggregateFunction(max, Int64),
                 s_first_value AggregateFunction(argMin, Float64, Int64),
                 s_last_value AggregateFunction(argMax, Float64, Int64),
                 s_positive_delta_sum AggregateFunction(deltaSumTimestamp, Float64, Int64))
  ENGINE = AggregatingMergeTree ORDER BY (window_end_ms, `job`, `$promql_series_identity`);

CREATE MATERIALIZED VIEW v1_mv TO v1 AS
  SELECT `job`, `$promql_series_identity`,
         (intDiv(toUnixTimestamp64Milli(ts) - 1, 60000) + 1) * 60000 AS window_end_ms,
         countState() AS s_sample_count,
         minState(toUnixTimestamp64Milli(ts)) AS s_first_ms,
         maxState(toUnixTimestamp64Milli(ts)) AS s_last_ms,
         argMinState(value, toUnixTimestamp64Milli(ts)) AS s_first_value,
         argMaxState(value, toUnixTimestamp64Milli(ts)) AS s_last_value,
         deltaSumTimestampState(value, toUnixTimestamp64Milli(ts)) AS s_positive_delta_sum
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
             sum(seg_sample_count) AS n,
             min(seg_first_ms) AS first_ms,
             max(seg_last_ms) AS last_ms,
             argMin(seg_first_value, seg_first_ms) AS v_first,
             sum(seg_positive_delta_sum) + sum(step_into_segment) AS increase
      FROM (
        SELECT `job`, `$promql_series_identity`,
               seg_sample_count, seg_first_ms, seg_last_ms, seg_first_value, seg_positive_delta_sum,
               greatest(0, seg_first_value - lagInFrame(seg_last_value, 1, seg_first_value) OVER segments)
                 AS step_into_segment,
               throwIf(lagInFrame(seg_last_ms, 1, toInt64(-1)) OVER segments >= seg_first_ms,
                       'stored segments of a series overlap in time: insert each series in time order')
                 AS segments_overlap
        FROM (
          SELECT `job`, `$promql_series_identity`,
                 finalizeAggregation(s_sample_count) AS seg_sample_count,
                 finalizeAggregation(s_first_ms) AS seg_first_ms,
                 finalizeAggregation(s_last_ms) AS seg_last_ms,
                 finalizeAggregation(s_first_value) AS seg_first_value,
                 finalizeAggregation(s_last_value) AS seg_last_value,
                 finalizeAggregation(s_positive_delta_sum) AS seg_positive_delta_sum
          FROM v1
          WHERE window_end_ms > {t:Int64} - 300000 AND window_end_ms <= {t:Int64}
            AND (SELECT throwIf({t:Int64} % 60000 != 0, 'the evaluation time is not a multiple of the window width, 60000 ms')) = 0)
        WINDOW segments AS (PARTITION BY `job`, `$promql_series_identity` ORDER BY seg_first_ms
                            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW))
      GROUP BY `job`, `$promql_series_identity`
      HAVING n >= 2 AND max(segments_overlap) = 0)))
GROUP BY `job`;

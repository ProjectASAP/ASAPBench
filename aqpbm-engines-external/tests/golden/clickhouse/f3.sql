-- Prometheus extrapolatedRate (promql/functions.go, v3.15.1) over the raw samples
-- in (t - 5m, t]: the increase adds the previous value at each drop (counter reset);
-- the 1.1 x average-gap threshold applies first, then the zero-point clamp.
CREATE TABLE src_requests_total (ts DateTime64(3, 'UTC'), value Float64, `job` LowCardinality(String), `$promql_series_identity` LowCardinality(String))
  ENGINE = MergeTree ORDER BY (ts, `$promql_series_identity`);

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
             arraySort(groupArray((toUnixTimestamp64Milli(ts), value))) AS points,
             arrayMap(p -> p.2, points) AS vals,
             length(points) AS n,
             points[1].1 AS first_ms,
             points[n].1 AS last_ms,
             vals[1] AS v_first,
             vals[n] - vals[1]
               + arraySum(arrayMap((cur, prev) -> if(cur < prev, prev, 0),
                                   arrayPopFront(vals), arrayPopBack(vals))) AS increase
      FROM src_requests_total
      WHERE ts > fromUnixTimestamp64Milli({t:Int64} - 300000) AND ts <= fromUnixTimestamp64Milli({t:Int64})
      GROUP BY `job`, `$promql_series_identity`
      HAVING n >= 2)))
GROUP BY `job`;

# Deployment capabilities of the external engines

Each file is an ASAPPlanner deployment-capabilities file (the `DeploymentCapabilities` JSON of ASAPPlanner #1374). Pass it with `asap_planner --deployment-capabilities <file>` so that the planner plans only for what the engine can run:

| File | What the planner may use |
|---|---|
| `clickhouse.equivalent-only.json` | class (a) only: same answers as the planner's semantics |
| `clickhouse.with-substitutes.json` | (a) and (b): (b) changes the guarantee as stated below |
| `victoriametrics.equivalent-only.json` | (a) only |
| `victoriametrics.with-substitutes.json` | (a) and (b) |

Classes:
- **(a) equivalent:** the engine computes the same answer.
- **(b) substitute:** the engine computes something close, with the stated difference.
- **(c) absent:** the engine cannot do it.

The translator in this crate is the check that the files are right. A plan made with a file must translate without `UnsupportedShape`. In an equivalent-only run, it must also translate without any substitution.

The tables cite engine sources at the release-tag commits of the binaries in use:
- ClickHouse 26.8.34.13 (`v26.8.34.13-lts`, `e0cfed8d5e66`);
- VictoriaMetrics v1.154.1 (`64c2e75fe7dc`).

Prometheus `extrapolatedRate` is cited at v3.15.1.

## ClickHouse

| Plan element | Class | ClickHouse | Guarantee difference | Evidence |
|---|---|---|---|---|
| Stored windows, exact Sum / Count / Min / Max | a | `sumState` / `countState` / `minState` / `maxState` in an AggregatingMergeTree via a materialized view; `-Merge` at query time | — | `src/AggregateFunctions/Combinators/AggregateFunctionState.cpp` |
| Stored windows, exact Rate / Increase | b | `count`, `min`/`max(ts)`, `argMin(value, ts)` and `deltaSumTimestamp(value, ts)` states, then Prometheus `extrapolatedRate` on the merged states | A counter reset adds 0 for the step across it. PromQL adds the first value after the reset. | `src/AggregateFunctions/AggregateFunctionDeltaSumTimestamp.cpp` (adds only positive deltas; merges by timestamp) |
| Query-time Rate / Increase | a | Prometheus `extrapolatedRate` in SQL over the raw samples, with reset correction | — | Prometheus `promql/functions.go` `extrapolatedRate` |
| IRate, stored or at query time (also the `i_rate` measure) | c | — | — | not written; excluded in both files |
| HyperLogLog distinct count (`Hll`) | a | `uniqCombined64(p)`, p in 12..20 | — | `src/AggregateFunctions/AggregateFunctionUniqCombined.cpp` |
| KMV distinct count (`Kmv`) | b | `uniqTheta` | lg_k is fixed at 12 (4,096 entries) | `src/AggregateFunctions/ThetaSketchData.h` |
| KLL quantile (`Kll`) | b | `quantileGK(accuracy)` | Deterministic rank error ≤ n / accuracy instead of KLL's probabilistic bound | `src/AggregateFunctions/QuantileGK.h` |
| DDSketch quantile (`DDSketch`) | b | `quantileDD(α)` | Reads rank q·(n − 1); the α bound holds at that rank | `src/AggregateFunctions/DDSketch.h` (L79) |
| Top-k (`CmsWithHeap`, `CountSketchWithHeap`) | b | `topK(k)` (Space-Saving) | No guaranteed result | `src/Common/SpaceSaving.h` |
| Raw samples retained | a | the MergeTree source table | — | — |
| Query-time results kept across evaluations | c | — | — | — |

## VictoriaMetrics

| Plan element | Class | VictoriaMetrics | Guarantee difference | Evidence |
|---|---|---|---|---|
| Stored windows, exact Sum / Count / Min / Max | a | stream aggregation outputs `sum_samples` / `count_samples` / `min` / `max`, `enable_windows: true` | — | `lib/streamaggr/streamaggr.go` |
| Counter reset handling | b | `removeCounterResets` | A drop smaller than prev/8 adds only prev − v. A staleness gap clears the correction. `increase` with no earlier sample starts the counter at 0. | `app/vmselect/promql/rollup.go` |
| Counter extrapolation | b | none: `rate` / `increase` are not extrapolated | The increase over the samples, not extrapolated to the range's ends | `app/vmselect/promql/rollup.go` |
| IRate | b | MetricsQL `irate`, query time only | VictoriaMetrics' look-behind rule | `app/vmselect/promql/rollup.go` |
| KLL / DDSketch quantile | b | stream output `histogram_bucket` (VictoriaMetrics histogram), `histogram_quantile` | Relative error ≤ 10^(1/18) − 1 ≈ 0.136 | `lib/streamaggr/histogram_bucket.go` |
| Hll / Kmv distinct count, top-k | b | exact `count(count by (…))` / `topk` over raw samples, query time only | Exact, but no stored state | `app/vmselect/promql/aggr.go` |
| Raw samples retained | a | — | — | — |

In `victoriametrics.equivalent-only.json`, both counter semantics are `false`. The planner therefore excludes every Rate / Increase / IRate state and every `rate` / `increase` / `irate` / `resets` / `delta` measure.

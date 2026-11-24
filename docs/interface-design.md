# ASAPBench interface design

## Who reads what

| Reader | Implements | Calls | Read |
|---|---|---|---|
| Adds a sketch library | a registry entry and wrapper functions | — | [Scope and overview](#scope-and-overview), [Terms](#terms), [Operator level](#operator-level), [Measurement, accuracy and errors](#measurement-accuracy-and-errors) |
| Adds a system | an adapter: `EngineUnderTest` and `InstalledArm` | the conformance check | [Scope and overview](#scope-and-overview), [Terms](#terms), [System level](#system-level), [Measurement, accuracy and errors](#measurement-accuracy-and-errors) |
| Runs the benchmark | — | the command line | [Scope and overview](#scope-and-overview), [Terms](#terms), [Inputs and outputs](#inputs-and-outputs) |
| A crate that uses ASAPBench as a library (for example ASAPPlanner) | as needed | the public modules | [System level](#system-level), [Public and internal](#public-and-internal) |

## Scope and overview

ASAPBench measures at two levels:

- **Operator level**: one sketch algorithm implementation. It measures throughput, latency, memory and accuracy across insert, query and merge operations.
- **System level**: one system running a set of recurring queries over a stream of time-series data. It measures ingest cost, query cost, stored state size and accuracy. A system is an in-process executor or an external server. If the system uses sketches, they are measured as part of the system.

```
                      ┌────────────────┐
 data description ──► │ data generator │── samples ──┬────────────────────────┐
                      └────────────────┘             │                        │
                                                     ▼                        ▼
                              ┌─────────────────────────────────────┐   ┌──────────────┐
                              │ under test                          │   │ exact answer │
                              │ operator level: wrapper functions   │   └──────┬───────┘
                              │ system level: adapter               │          │
                              └──────────────────┬──────────────────┘          │
                                the harness times every call                   │
                                                 ▼                             ▼
                                        answers + cost ─────────────────► scoring ──► results
```

Out of scope:

- how a plan is produced;
- how a system or a sketch works inside;
- turning answers into use-case decisions.

## Terms

| Term | Meaning |
|---|---|
| harness | the code that calls what is under test, reads the clocks and writes the results; it lives in the crate `aqpbm-core` |
| `approxbench` | the command-line program |
| system | what runs the queries at system level; the code calls it an engine (`EngineUnderTest`, `engine_options`, the `engine` field of a result) |
| adapter | the code that implements `EngineUnderTest` and `InstalledArm` for one system |
| workload | a set of queries and the time-series data they run over; all its queries use the same evaluation interval |
| arm | one way to run a workload: same data and queries, different configuration |
| sample chunk | the samples in `(end_ms − interval, end_ms]`, where interval is the workload's evaluation interval; a system ingests one chunk at a time |
| repetition | one timed pass of a measurement; the first `warmup_runs` are discarded, the next `measured_runs` are kept |
| run | everything one command measures |

## Operator level

### Interface

A sketch under test is a registry entry, one sketch implementation the benchmark can measure, plus one wrapper function per operation. A wrapper function prepares one operation for timing. Both live in the `sketch-bench` crate. Wrapper functions are plain functions, not a trait: libraries take different input shapes, and a shared trait would put the conversion cost inside the timed region.

The registry entry:

```rust
pub struct SketchId {
    pub algorithm: &'static str,          // algorithm family, e.g. "cms"; entries in one family take the same parameters
    pub variant: &'static str,            // e.g. "cms-fastpath-vector2d"
    pub library: &'static str,            // e.g. "oxide", "datasketches", "lib"
    pub description: &'static str,
    pub capability: Capability,           // the statistic it answers: frequency, quantile, cardinality, ...
    pub comparator: Option<&'static str>, // the accuracy scorer for this entry, e.g. "frequency"; `--comparator` must name it
    pub operations: OperationMask,        // which of Insert, Query, Merge, Prepare it supports
    pub metrics: MetricsMask,             // which of Throughput, Latency, Accuracy it supports
}
```

The operations: `Insert` adds items. `Query` reads an estimate from the sketch. `Merge` folds several sketches, called shards, into one. `Prepare` is the step some sketches need between inserting and querying, such as sorting.

A wrapper function returns one closure per repetition. Calling the closure is the timed work. The harness provides these closure types:

```rust
pub type Pass = Box<dyn FnOnce() -> usize>;                   // one repetition; returns the sketch's memory in bytes
pub type QueryPass<A> = Box<dyn FnOnce() -> (Vec<A>, usize)>; // one repetition; returns the estimates and the memory in bytes

pub struct StepPass {                                         // one repetition for latency: each call does one unit of work
    pub steps: usize,                                         // calls in one repetition
    pub step: Box<dyn FnMut(usize)>,
    pub footprint: Box<dyn Fn() -> usize>,                    // memory in bytes, read after the last call
}
```

The wrapper function shapes. `I` is the item type, `P` the query input type, `A` the estimate type.

```rust
// Throughput and accuracy.
// (parameters, items, repetitions)
pub type InsertBody<I> =
    fn(&ParamSet, Rc<Vec<I>>, usize) -> Result<Vec<Pass>, BuildError>;
// (parameters, items, query inputs, repetitions)
pub type QueryBody<I, P, A> =
    fn(&ParamSet, Rc<Vec<I>>, Rc<Vec<P>>, usize) -> Result<Vec<QueryPass<A>>, BuildError>;
// (parameters, items, shards, repetitions)
pub type MergeBody<I> =
    fn(&ParamSet, Rc<Vec<I>>, usize, usize) -> Result<Vec<Pass>, BuildError>;
// (parameters, items, repetitions)
pub type PrepareBody<I> =
    fn(&ParamSet, Rc<Vec<I>>, usize) -> Result<Vec<Pass>, BuildError>;

// Latency: the same insert and merge, one unit of work per call.
pub type InsertStepBody<I> =
    fn(&ParamSet, Rc<Vec<I>>, usize) -> Result<Vec<StepPass>, BuildError>;
pub type MergeStepBody<I> =
    fn(&ParamSet, Rc<Vec<I>>, usize, usize) -> Result<Vec<StepPass>, BuildError>;
```

Preconditions:

- Every shard in a merge is built with the same parameters.
- A closure has everything it needs before it is called.

### Call order

1. Find the requested registry entry. Stop with an error if it does not exist.
2. Call the wrapper function. It returns one closure per repetition, each holding a sketch and data already built.
3. Call the closures in order. The timer covers only the closure.
4. After the timer stops, read memory and score the estimates.
5. Write one result record for the command.

### Adding a sketch library

In the `sketch-bench` crate:

1. Add a `SketchId` entry to the registry.
2. Write one wrapper function per supported operation.
3. Bind the entry to its wrapper functions.
4. Run `approxbench sketchbench --list-impls`, which lists every registry entry, and check that the new entry is there. Then measure each operation once.

## System level

### Interface

A system plugs in through an adapter. The adapter implements two traits: `EngineUnderTest` describes the system and installs an arm; `InstalledArm` is one installed arm that the harness feeds and queries.

```rust
pub trait EngineUnderTest {
    /// Id, version and build; written into every run.
    fn description(&self) -> EngineDescription;
    /// Where the harness reads this system's CPU time.
    fn cpu_accounting(&self) -> CpuAccounting;
    /// Install one arm of one workload. Not timed.
    fn install(&self, workload: &WorkloadSpec, arm: &ArmSpec)
        -> Result<Box<dyn InstalledArm>, EngineError>;
}

pub trait InstalledArm {
    /// What was installed: the plan text, the kinds of its operators, its stored outputs.
    fn plan_summary(&self) -> PlanSummary;
    /// Ingest one sample chunk. The harness times the call.
    fn ingest(&mut self, chunk: &SampleChunk) -> Result<StepReport, EngineError>;
    /// The answers of every query at `at_ms`. The harness times the call.
    fn evaluate(&mut self, at_ms: i64) -> Result<(Answers, StepReport), EngineError>;
    /// Bytes of state held after the last chunk.
    fn footprint(&self) -> Result<Footprint, EngineError>;
}

pub enum CpuAccounting {
    CallingThread,            // in-process system: the work runs on the calling thread
    Processes(Vec<u32>),      // external server: read the CPU of these process ids
    Cgroup(CgroupSource),     // external server in its own cgroup v2 group: read the group's files
}
```

What the harness passes in: the workload, the arm, and the samples chunk by chunk.

```rust
pub struct WorkloadSpec {
    pub name: String,
    pub queries: Vec<WorkloadQuery>,
    pub data: Vec<DataRecord>,
}

pub struct WorkloadQuery {
    pub id: String,
    pub texts: Vec<QueryText>,        // the same query in several languages; the system picks one it supports
    pub window_ms: u64,               // each evaluation at t covers (t − window_ms, t]
    pub evaluation_interval_ms: u64,  // the same for every query of a workload; a workload that mixes them is rejected
}

pub struct QueryText {
    pub language: QueryLanguage,
    pub text: String,
}

pub enum QueryLanguage {
    PromQl,
    Sql,
}

pub struct DataRecord {
    pub metric: String,
    pub description: TimeSeriesDescription,
}

pub struct ArmSpec {
    pub name: String,
    pub raw_data_retained: bool,           // whether the system keeps raw samples
    pub engine_options: serde_json::Value, // read only by the adapter; an adapter that runs a given plan reads the plan file path here
}

pub struct SampleChunk {
    pub end_ms: i64,
    pub samples: Vec<Sample>,
}

pub struct Sample {
    pub labels: Arc<SeriesLabels>,
    pub ts_ms: i64,
    pub value: f64,
}
```

What the adapter returns: answers, step reports, the footprint and a plan summary.

```rust
/// Query id → output series labels → value: one value per (query, output series) pair.
pub struct Answers(pub BTreeMap<String, BTreeMap<SeriesLabels, f64>>);

/// What the system reports about one step. The harness does not use it for timing.
pub struct StepReport {
    pub scanned_bytes: Option<u64>,
    pub engine_reported_cpu_ns: Option<u64>,
}

pub struct Footprint {
    pub view_bytes: u64,         // bytes of stored outputs: state kept between evaluations, such as materialized views or sketches
    pub raw_retained_bytes: u64, // bytes of raw samples kept
    pub peak_bytes: Option<u64>, // peak memory during the run, if the system reports it
}

pub struct PlanSummary {
    pub text: String,
    pub node_kinds: Vec<String>,     // the kind of each operator in the plan, e.g. "scan", "sum"
    pub stored_outputs: Vec<String>,
}

pub struct EngineError(pub String);
```

Output series labels follow PromQL: a function or an aggregation drops `__name__`, and only a bare selector keeps it. Every adapter uses this rule so answers from different systems can be matched by label set. For example, `sum_over_time(probe[10s])` with id `sum` over the series `probe{instance="instance_0"}` answers under `"sum" → {instance="instance_0"}`.

The adapter for an external server runs in the harness process. `ingest` sends the samples to the server, for example by Prometheus Remote Write, and `evaluate` calls the server's query interface.

### Call order

The evaluation times are every multiple of the evaluation interval from the data start plus the longest window, through the data end. Example: windows of 30 s and 20 s, every 10 s, data from 0 for 120 s: evaluations at 30 s, 40 s, ..., 120 s.

1. `install(workload, arm)`. Not timed.
2. For each sample chunk: time `ingest(chunk)`.
3. If the chunk's `end_ms` is an evaluation time `t`: time `evaluate(t)`.
4. After the last chunk: `footprint()`.

Steps 1–4 are done once per repetition, each on its own install. Each timed step reports the median over the measured repetitions. The answers of the last repetition are scored, and the arm's result is written.

### Adding a system

1. Write an adapter: implement `EngineUnderTest` and `InstalledArm`.
2. Choose the `CpuAccounting` that matches where the system runs.
3. Add the system to the list of systems `approxbench benchmark-external` can run.
4. Pass the conformance check, `harness::conformance::check(engine, clocks)`. It installs small probe workloads and checks that:
   - two installs with the same data give the same answers;
   - the window leaves out a sample exactly at `t − window_ms`;
   - for `CallingThread`, the work runs on the calling thread.

## Inputs and outputs

### Inputs

**Workload**

```json
{
  "name": "http-latency",
  "queries": [{
    "id": "p99",
    "texts": [{"language": "PromQl", "text": "quantile_over_time(0.99, latency[5m])"}],
    "window_ms": 300000,
    "evaluation_interval_ms": 10000
  }],
  "data": [{
    "metric": "latency",
    "description": {
      "labels": [{"name": "instance", "cardinality": 10}],
      "series_num": 10,
      "start_ms": 0,
      "sample_interval_ms": 1000,
      "duration_ms": 3600000,
      "value_distribution": {"kind": "uniform", "lower_bound": 0.0, "upper_bound": 1000.0, "seed": 1},
      "counter": false,
      "seed": 42
    }
  }]
}
```

The data description:

- `labels`: each label and how many values it takes (`instance_0` to `instance_9` here);
- `series_num`: how many distinct series, at most the product of the label cardinalities;
- `start_ms`, `sample_interval_ms`, `duration_ms`: the first sample is one interval after the start;
- `value_distribution`: what each series draws its values from, with its own seed;
- `counter`: if true, each value is the running sum of the draws, so the series never decreases;
- `seed`: fixes the label sets and, mixed with the distribution's seed, every value.

**Arm**

```json
{"name": "noraw", "raw_data_retained": false, "engine_options": {"plan_file": "plans/noraw/http-latency.json"}}
```

**Protocol**: the number of repetitions and the load limit.

```json
{"warmup_runs": 1, "measured_runs": 5, "max_core_busy_fraction": 0.10}
```

**System-level command**: run every workload and arm in a run directory on one system, then join the results of every system.

```sh
approxbench benchmark-external run    --run-directory <dir> --engine <system> [--variant <arm>] [--core 0] [--warmup 1] [--runs 5]
approxbench benchmark-external report --run-directory <dir> --truth <system>/<arm>
```

**Operator-level command**: names the registry entry, its parameters, the data, and the operations and metrics to measure.

```sh
approxbench sketchbench \
    --variant cms-fastpath-vector2d --library lib \
    --config 'rows=5 cols=2048' \
    --dataset uniform --size 1000000 --cardinality 100000 \
    --dtype i64 \
    --operations insert,query --metrics throughput \
    --report results.jsonl
```

### Outputs

At system level, one directory per run:

```
<root>/runs/<machine id>/<UTC time>-<subcommand>-<run name>/
  run.json                                  command line, version, machine, systems, protocol
  workloads/                                the workloads used
  data/<metric>.datagen.json                the data descriptions
  plans/                                    plan files given to systems that take one
  results/<system>/<workload>__<arm>.json   one result per arm
  status.log                                one line per step: CPU core, how busy it was before and after, load average
```

`<machine id>` names the machine. `<run name>` is chosen by whoever starts the run.

One arm's result (excerpt):

```json
{
  "engine": "clickhouse", "workload": "http-latency", "arm": "noraw",
  "plan": {"text": "...", "node_kinds": ["scan", "quantile"], "stored_outputs": ["mv_p99"]},
  "ingest":   [{"median": {"thread_cpu_ns": null, "process_cpu_ns": 812000, "wall_ns": 950000}}],
  "evaluate": [{"median": {"thread_cpu_ns": null, "process_cpu_ns": 64000,  "wall_ns": 88000}}],
  "answers": [[300000, {"p99": {"{instance=\"instance_0\"}": 412.5}}]],
  "footprint": {"view_bytes": 18432, "raw_retained_bytes": 0, "peak_bytes": null},
  "ingested_samples": 36000
}
```

- `ingest` has one entry per sample chunk, `evaluate` one per evaluation time.
- `thread_cpu_ns` is the calling thread's CPU time, set only for an in-process system. `process_cpu_ns` is the CPU time of the measured processes.
- Each `answers` entry is `[evaluation time in ms, answers]`.
- `ingested_samples` is the number of samples one install ingested.

At operator level, the `--report` file gets one JSON line per command: which entry (`sketch`, `algorithm`, `impl`), its parameters (`sketch_config`), the data (`workload`), the number of repetitions (`runs`), and the measured metrics per operation (`bench`).

## Measurement, accuracy and errors

### Measurement

- **The timer**: nothing under test times itself, so every system and sketch is timed the same way and the numbers can be compared. The harness reads the clocks before and after each call. `CpuAccounting` says which clock.
- **CPU time** (ns): for an in-process system, the calling thread's CPU clock. For `Processes`, the processes' own cgroup `cpu.stat`, or their threads' `schedstat`. For `Cgroup`, the group's `cpu.stat`.
- **Wall time** (ns): read before and after each call.
- **Memory** (bytes): at operator level, the value the closure returns; at system level, `footprint`, plus the cgroup's memory readings for an external server.
- **Throughput**: units processed per second of CPU time. Items for insert, query inputs for query, shards for merge.
- **Repetitions**: the first `warmup_runs` repetitions are discarded, then `measured_runs` repetitions (an odd number) are measured and the median is reported. Accuracy is measured once, since it does not change for the same data and parameters.
- **Load check**: the harness pins each step to one CPU core (`--core`). If that core is busier than `max_core_busy_fraction` before a step, the step does not run; if it is busier after the step, the step is recorded as over the limit. Both are recorded in `status.log`.

### Accuracy

- The exact answer is the reference an answer is scored against. It comes from one of two sources:
  - each query declares the statistic it computes in a use-case spec (crate `aqpbm-usecase`), and the exact answer is computed from the raw samples;
  - one system's arm serves as the exact answer: `--truth <system>/<arm>`.
- The error is computed per query × series × evaluation time. The default is relative error, `|answer − exact| / |exact|`.

### Errors

| Condition | What the harness does |
|---|---|
| `install` fails, for example because the system supports none of the query's languages | the arm is recorded as not run, with the reason; other arms continue |
| `ingest` or `evaluate` fails | the arm stops and is recorded as failed |
| the workload has no data, or mixes evaluation intervals | the arm is recorded as not run |
| the CPU core is too busy before a step | see the load check above |
| the requested registry entry does not exist | the command stops with an error |
| a wrapper function cannot prepare its closures (`BuildError`) | the entry is recorded as failed and not measured |

## Public and internal

| Part | Status |
|---|---|
| registry entries and wrapper functions | you add these to the `sketch-bench` crate |
| `Pass`, `QueryPass`, `StepPass` | provided by the harness |
| the system-level traits | you implement these in your own crate |
| the system-level types, the input and output formats | public, versioned |
| the conformance check | public |
| running one arm, the clocks, the run directory | public, callable as a library |
| the data generator's internals, report aggregation, visualization | internal, may change without notice |

A crate that uses ASAPBench as a library depends on `aqpbm-core`. With `default-features = false` it leaves out the latency histogram. It may use:

- the system-level interface and types (`harness::engine`, `harness::data`);
- the clocks (`harness::protocol`);
- running one arm (`harness::arm_run`);
- the run directory (`harness::run_directory`);
- the conformance check (`harness::conformance`).

## Open questions

1. Should `InstalledArm` gain four optional methods? All four serve a query optimizer, which picks a plan using a cost model (a formula that predicts what a plan costs):
   - `predicted`: the cost the system's own cost model predicts;
   - `price`: the measured usage, priced by the system's own cost model;
   - `node_profiles`: the CPU time each operator used;
   - `calibration`: the harness measures a few basic operations so the system can fit the constants of its cost model.
2. Should the declared statistic of a query move from the use-case spec into `WorkloadQuery`, so one workload file carries both the query and how to compute its exact answer?
3. How is a series scored when the exact answer is 0, or when a system returns a series the exact answer does not have, or misses one?
4. Should quantile queries be scored by rank error instead of relative error?
5. Is the arm that runs the exact query directly on a system compared as (a) the same plan on different systems, or (b) each system configured as its users normally would?
6. Should external servers take plans in a format that does not depend on any planner? If not, the planner's format belongs in the adapter.
7. Is the arm with hand-written materialized views compared as the same plan on different systems, or as each system configured as its users normally would?

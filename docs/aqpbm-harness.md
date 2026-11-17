# `aqpbm-harness` Design

`aqpbm-harness` measures an engine under test (an in-process planner and
executor, an external server such as ClickHouse, or a fake) under one
measurement protocol. The engine never times itself: the harness feeds it
data, asks it for answers and reads the clocks around every call.

Example: the fake engine answers `sum_over_time(probe[10s])` over the
left-open window `(t - 10 s, t]`. One arm of that workload under the default
protocol installs the arm 6 times (1 warm-up + 5 measured runs), ingests each
refresh chunk into every install as one protocol job, and records per job the
5 measured samples and their median. Ingesting 1,000 samples at 50 ns per
sample plus 10 µs per call records 60,000 ns.

The crate depends on no sketch library, no polars, no CLI crate and no
planner (`tests/dependencies.rs` checks this).

## Components

| Module | What it holds |
|---|---|
| `engine` | The traits an engine implements (`EngineUnderTest`, `InstalledArm`, `Calibratable`) and the records it exchanges with the harness (`StepReport`, `Answers`, `Footprint`, ...). |
| `protocol` | The clocks, pinning, the idle check, `Protocol::run_job`, `measure_sizes`, the median and the noise band. |
| `arm_run` | `run_arm`: one arm of one workload under the protocol. |
| `data`, `data_stats` | Workload data as `aqpbm-datagen` descriptions, cut into refresh chunks; per-metric statistics (series count, sample interval, distinct label values, value range and sign). |
| `fit` | Relative-weighted least squares over named cost constants, and leave-one-setting-out validation. |
| `accuracy` | Error quantiles and the worst accuracy curve over merge counts. |
| `machine`, `records`, `rundir` | The machine key, the JSON records of a run, and the run directory they are written to. |
| `gate`, `sweep` | The gate verdict (selected arm against exact) and sweep arithmetic (regret, ranking agreement). |
| `fake`, `conformance` | A deterministic fake engine on a fake clock, and the checks every engine adapter must pass. |

## The measurement protocol

One job is one step (a refresh chunk, an evaluation, or one calibration size)
measured `measured_runs` times:

1. Refuse an even or zero run count, and a core outside the process's allowed
   set (read once, before the harness pins any thread).
2. Check that the core is idle: busy at most `max_core_busy_fraction` (10%)
   over one `/proc/stat` sampling interval (100 ms). Consecutive jobs share a
   sample: a job's after-sample is the next job's before-sample.
3. Pin the calling thread to the core, run the warm-up calls, then time each
   measured call: CPU clocks, then wall clock, before the call; wall clock,
   then CPU clocks, after it.
4. Sample the core again and mark the job if it turned busy.
5. The median is the sample in the middle by the engine's CPU.

The engine's CPU is read where its `cpu_accounting()` says
(`MeasurementClocks::clock_for` picks the clock):

- `CallingThread` (in-process): the calling thread's CPU clock; the median is
  over thread CPU.
- `Processes(pids)` (external servers): the servers' own CPU, never the
  harness process's, at µs or ns resolution. When every pid is in one cgroup
  v2 group of its own (not the harness's, not the root), the group's
  `cpu.stat` `usage_usec`, which also counts exited threads and children;
  otherwise the sum of `/proc/<pid>/task/*/schedstat` (ns on CPU of the live
  threads). A pid that is missing or a zombie fails the job. The median is
  over process CPU.

`run_arm` holds `warmup_runs + measured_runs` installs at once (6× one arm's
memory by default), so that each step is one job whose k-th call goes to
install k and every measured ingest is a first ingest. Each call's answers
are moved out of the timed call and freed after the job; the answers of the
last run are recorded.

## Calibration and fitting

An engine with a cost model implements `Calibratable`: one `CostTarget` per
constant, a closure per size that runs the operator and returns the units it
processed, and `charged_units`, the units its own cost model charges.
`measure_sizes` runs each size as one protocol job and returns one
`FitPoint { setting: size, terms: [units, 1], measured: median CPU }`;
`fit_constants` fits the constants and `leave_one_setting_out` validates
them on held-out settings. The conformance kit checks that the closure's
units match `charged_units`.

## Code interface

```rust
pub trait EngineUnderTest {
    fn description(&self) -> EngineDescription;
    fn cpu_accounting(&self) -> CpuAccounting;
    fn install(&self, workload: &WorkloadSpec, arm: &ArmSpec) -> Result<Box<dyn InstalledArm>, EngineError>;
    fn calibration(&mut self) -> Option<&mut dyn Calibratable> { None }
}

impl Protocol {
    pub fn run_job(&self, core: usize, probe: &mut dyn LoadProbe, clock: &EngineClock,
        job: &mut dyn FnMut() -> Result<StepReport, EngineError>) -> Result<JobRecord, ProtocolError>;
}

pub fn run_arm(engine: &dyn EngineUnderTest, workload: &WorkloadSpec, arm: &ArmSpec,
    schedule: &ArmSchedule, protocol: &Protocol, core: usize, probe: &mut dyn LoadProbe,
    clocks: &MeasurementClocks) -> Result<ArmResult, ArmRunError>;

pub fn measure_sizes(protocol: &Protocol, core: usize, probe: &mut dyn LoadProbe,
    clock: &EngineClock, sizes: &[u64], run: &mut dyn FnMut(u64) -> Result<u64, EngineError>)
    -> Result<Vec<FitPoint>, ProtocolError>;

pub fn check(engine: &mut dyn EngineUnderTest, clocks: &MeasurementClocks)
    -> Result<ConformanceReport, Vec<ConformanceFailure>>;
```

A run directory is
`<root>/runs/<machine key>/<UTC>-<subcommand>-<label>/` with `run.json`,
`data/<metric>.datagen.json`, `results/<engine>/<workload>__<arm>.json` and
`status.log`; an existing directory or result file is never overwritten.

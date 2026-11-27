//! Engine-neutral measurement harness.
//!
//! An engine under test (an in-process planner and executor, or an external
//! server) implements [`engine::EngineUnderTest`]. The harness installs one
//! arm of a workload on it, feeds it the samples of `aqpbm-datagen` in
//! refresh chunks, asks it for answers at evaluation times, and times every
//! step itself under one measurement protocol: the engine never reports its
//! own time.
//!
//! Example: the fake engine of [`fake`] answers `sum_over_time(probe[10s])`
//! over the left-open window `(t - 10 s, t]`; [`conformance::check`] runs it
//! twice under one seed and checks the answers match, the window boundary,
//! the calibration units and that the work ran on the calling thread.
//!
//! Scope beside the rest of `aqpbm-core`: [`measure()`](fn@crate::measure)
//! times one sketch's closures and [`crate::benchmark_result`] holds the
//! per-sketch JSONL records; this module measures a whole engine and owns
//! the per-arm run records ([`records`]). It reads the clocks of
//! [`crate::metrics`] (`ThreadCpuTime`, `CpuTimeSampler`) rather than its own.

pub mod arm_run;
pub mod benchmark_external;
pub mod cgroup;
pub mod conformance;
pub mod cpu_frequency;
pub mod data;
pub mod data_stats;
pub mod engine;
pub mod error_quantiles;
pub mod fake;
pub mod fit;
pub mod gate;
pub mod machine;
pub mod one_shot;
pub mod one_shot_report;
pub mod protocol;
pub mod records;
pub mod run_directory;
pub mod sweep;
pub mod write_throughput;

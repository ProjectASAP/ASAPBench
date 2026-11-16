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

pub mod accuracy;
pub mod arm_run;
pub mod conformance;
pub mod data;
pub mod data_stats;
pub mod engine;
pub mod fake;
pub mod fit;
pub mod gate;
pub mod machine;
pub mod protocol;
pub mod records;
pub mod rundir;
pub mod sweep;

//! The rows each [`registry`](crate::registry) entry names, and the call that
//! runs the row an entry names.

use std::marker::PhantomData;
use std::rc::Rc;

use crate::request::{ColumnRoles, Dtype, Requirement};
use crate::wrappers::{
    interleave, BuildError, Folds, InsertBody, InsertStepBody, MergeSplit, ParallelInsertBody,
    PrepareBody, QueryBody, QueryPass, StepPass,
};

/// A row that cannot be built at the requested config says so in its own words;
/// the framework carries them.
fn cannot_build(e: BuildError) -> RunError {
    RunError::Sketch(e.0)
}
use crate::wrappers::frequency_value::FrequencyValue;
use aqpbm_core::accuracy::cardinality::CardinalityGT;
use aqpbm_core::accuracy::frequency::FrequencyGT;
use aqpbm_core::accuracy::keyed::{
    KeyedCardinalityGT, KeyedEntropyGT, KeyedL1NormGT, KeyedL2NormGT,
};
use aqpbm_core::accuracy::membership::MembershipGT;
use aqpbm_core::accuracy::quantile::{RankErrorGT, RelativeValueErrorGT};
use aqpbm_core::accuracy::subpopulation::{
    GroupKey, SubpopCardinalityGT, SubpopCdfErrorGT, SubpopEntropyGT, SubpopFrequencyGT,
    SubpopL1NormGT, SubpopL2NormGT, SubpopRankErrorGT, SubpopSumGT,
};
use aqpbm_core::accuracy::topk::TopkGT;
use aqpbm_core::accuracy::Score;
use aqpbm_core::accuracy::{questions, CountedValue, GroundTruth, PerGroup};
use aqpbm_core::error::RunError;
use aqpbm_core::measure::{
    record_calls, runs_for, Measurement, Pass as CorePass, Report, RunOutcome, MIN_MERGE_SHARDS,
};
use aqpbm_core::metrics::{Metric, Operation};
use aqpbm_core::{ColumnItem, GeneratedTable, KeyOffset};

const KEYED_KEY_COLUMN: usize = 0;

pub struct ValueColumn {
    pub index: usize,
    pub key_offset: Option<KeyOffset>,
}

/// One invocation's worth of work: one closure per measurement, in the order
/// they were asked for. The provenance is not here — the frontend generated the
/// data and already holds the description that names it.
pub type Measurements = Vec<((Operation, Metric), Measurement)>;

/// What a registry entry names to bind itself to code: one row, entered with a
/// request and the data, handing back the closures.
pub type RowBinding = fn(
    &Requirement,
    &ValueColumn,
    GeneratedTable,
    &[(Operation, Metric)],
) -> Result<Measurements, RunError>;

type Materialise<I> = fn(&ValueColumn, GeneratedTable) -> Result<Rc<Vec<I>>, RunError>;

/// Run the row the registry entry names and hand back the closures.
pub fn measurements(
    req: &Requirement,
    columns: &ColumnRoles,
    table: GeneratedTable,
    want: &[(Operation, Metric)],
) -> Result<Measurements, RunError> {
    let row = crate::registry::find(&req.variant, &req.library)
        .ok_or_else(|| {
            RunError::Sketch(format!(
                "{}/{} is not a registered row",
                req.variant, req.library
            ))
        })?
        .row;
    let mut order = columns.labels.clone();
    order.push(columns.value);
    let table = table.select(&order)?;
    let value_column = ValueColumn {
        index: columns.labels.len(),
        key_offset: columns.value_key_offset.clone(),
    };
    row(req, &value_column, table, want)
}

mod bloom;
mod cms;
mod cms_heap;
mod count_matrix;
mod cs;
mod dd;
mod exact;
mod fixed_matrix;
mod fold;
mod hll;
mod hydra;
mod hydra_univmon;
mod kll;
mod kmv;
mod materialise;
mod measurement;
mod nitro;
mod space_saving;
mod statistic;
mod string_keyed;
mod uniform_sampling;
mod univmon;

pub(crate) use bloom::*;
pub(crate) use cms::*;
pub(crate) use cms_heap::*;
pub(crate) use count_matrix::*;
pub(crate) use cs::*;
pub(crate) use dd::*;
pub(crate) use exact::*;
pub(crate) use fixed_matrix::*;
pub(crate) use fold::*;
pub(crate) use hll::*;
pub(crate) use hydra::*;
pub(crate) use hydra_univmon::*;
pub(crate) use kll::*;
pub(crate) use kmv::*;
use materialise::*;
use measurement::*;
pub(crate) use nitro::*;
pub(crate) use space_saving::*;
use statistic::*;
pub(crate) use string_keyed::*;
pub(crate) use uniform_sampling::*;
pub(crate) use univmon::*;

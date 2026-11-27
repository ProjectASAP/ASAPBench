//! Engine runs over one run directory, and the report that joins them.
//!
//! An engine run is one engine under test run as its own process over a run
//! directory another step prepared:
//!
//! ```text
//! workloads/<workload>.json                 harness WorkloadSpec
//! plans/<variant>/variant.json              {"substitutions_allowed": false}, optionally
//!                                           "plans": false, "run_by_default": false
//! plans/<variant>/<workload>.plan.json      a plan file, opaque to the harness
//! plans/<variant>/<workload>.no-plan.json   {"reason": "ExcludedByDeployment"}
//! ```
//!
//! [`run_engine`] runs every (workload, variant) cell on the engine and writes
//! `results/<engine>/<workload>__<variant>.cell.json` (always) and the
//! `ArmResult` beside it (when the cell ran). [`report`] joins every engine run's
//! cells into one row per cell, with absolute CPU, memory and accuracy
//! against a truth (one engine run's cells, or each query's declared
//! statistic), and [`write_report`] writes `summary.csv`, `summary.md`,
//! `accuracy.csv` and `scores.csv`.
//!
//! Example: engine `clickhouse` over variants `clickhouse.equivalent-only` and
//! `clickhouse.with-substitutes`, engine `asap` over both, report with truth
//! `asap/exact`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::accuracy::declared_truth::{without_name, AnswerAt, QueryTruth, Series, Statistic};
use crate::harness::arm_run::{run_arm, ArmSchedule};
use crate::harness::data::{workload_chunks, SeriesLabels};
use crate::harness::engine::{
    ArmSpec, CpuAccounting, EngineDescription, EngineUnderTest, WorkloadSpec,
};
use crate::harness::protocol::{LoadProbe, MeasurementClocks, Protocol};
use crate::harness::records::ArmResult;
use crate::harness::run_directory::{
    check_result_name, write_json, RunDirectory, RunDirectoryError,
};

/// `plans/<variant>/variant.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantRecord {
    /// `false` for a plan restricted to functions equivalent to the
    /// planner's: an engine that reports a substitution then disagrees with
    /// the capability file the plan was made under, and the cell fails.
    pub substitutions_allowed: bool,
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub plans: bool,
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub run_by_default: bool,
}

fn yes() -> bool {
    true
}

fn is_yes(value: &bool) -> bool {
    *value
}

/// `plans/<variant>/<workload>.no-plan.json`: why the planner wrote no plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoPlanRecord {
    pub reason: String,
}

/// What a variant holds for one workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEntry {
    PlanFile(PathBuf),
    NoPlan { reason: String },
    QueryTextOnly,
}

/// One variant of the run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub name: String,
    pub record: VariantRecord,
    /// Per workload.
    pub entries: BTreeMap<String, PlanEntry>,
}

/// One (workload, variant) cell of one engine: `<workload>__<variant>.cell.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineRunCell {
    pub engine: String,
    pub workload: String,
    pub variant: String,
    pub outcome: CellOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellOutcome {
    /// The arm ran; its `ArmResult` is beside the cell.
    Ran,
    /// The planner wrote no plan for this workload in this variant.
    NoPlan { reason: String },
    /// Installing or running failed, such as a translator refusal.
    NotRun { error: String },
}

/// `results/<engine>/engine-run-<UTC>.json`: one engine run and how it measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineRunRecord {
    pub engine: EngineDescription,
    pub cpu_accounting: CpuAccounting,
    pub protocol: Protocol,
    pub core: usize,
    /// The command line.
    pub args: Vec<String>,
}

/// The workloads of a run directory, by name (the file stem, which must
/// equal the spec's `name`).
pub fn read_workloads(
    run_directory: &RunDirectory,
) -> Result<BTreeMap<String, WorkloadSpec>, BenchmarkExternalError> {
    let mut workloads = BTreeMap::new();
    for path in sorted_files(&run_directory.path().join("workloads"))? {
        let stem = file_stem_with_suffix(&path, ".json")?;
        // Refused here, before any cell runs, not when its result is written.
        check_result_name("workload", &stem, true)?;
        let spec: WorkloadSpec = read_json(&path)?;
        if spec.name != stem {
            return Err(BenchmarkExternalError::Layout(format!(
                "{} holds workload '{}'; the file must be named after it",
                path.display(),
                spec.name
            )));
        }
        workloads.insert(stem, spec);
    }
    Ok(workloads)
}

/// The variants under `plans/`, each with one entry per workload.
pub fn read_variants(
    run_directory: &RunDirectory,
    workloads: &BTreeMap<String, WorkloadSpec>,
) -> Result<Vec<Variant>, BenchmarkExternalError> {
    let mut variants = Vec::new();
    for dir in sorted_entries(&run_directory.path().join("plans"))? {
        if !dir.is_dir() {
            return Err(BenchmarkExternalError::Layout(format!(
                "{} is not a variant directory",
                dir.display()
            )));
        }
        let name = file_name(&dir);
        check_result_name("variant", &name, true)?;
        let mut record = None;
        let mut entries = BTreeMap::new();
        for path in sorted_files(&dir)? {
            let file = file_name(&path);
            let (workload, entry) = if file == "variant.json" {
                record = Some(read_json::<VariantRecord>(&path)?);
                continue;
            } else if let Some(workload) = file.strip_suffix(".plan.json") {
                (workload.to_string(), PlanEntry::PlanFile(absolute(&path)?))
            } else if let Some(workload) = file.strip_suffix(".no-plan.json") {
                let NoPlanRecord { reason } = read_json(&path)?;
                (workload.to_string(), PlanEntry::NoPlan { reason })
            } else {
                return Err(BenchmarkExternalError::Layout(format!(
                    "{} is neither variant.json, <workload>.plan.json nor <workload>.no-plan.json",
                    path.display()
                )));
            };
            if !workloads.contains_key(&workload) {
                return Err(BenchmarkExternalError::Layout(format!(
                    "{} names workload '{workload}', which is not in workloads/",
                    path.display()
                )));
            }
            if entries.insert(workload.clone(), entry).is_some() {
                return Err(BenchmarkExternalError::Layout(format!(
                    "variant {name} has both {workload}.plan.json and {workload}.no-plan.json"
                )));
            }
        }
        let record = record.ok_or_else(|| {
            BenchmarkExternalError::Layout(format!("variant {name} has no variant.json"))
        })?;
        if !record.plans {
            if let Some(workload) = entries.keys().next() {
                return Err(BenchmarkExternalError::Layout(format!(
                    "variant {name} has plans: false but holds an entry for {workload}"
                )));
            }
            entries = workloads
                .keys()
                .map(|workload| (workload.clone(), PlanEntry::QueryTextOnly))
                .collect();
        }
        if let Some(missing) = workloads.keys().find(|w| !entries.contains_key(*w)) {
            return Err(BenchmarkExternalError::Layout(format!(
                "variant {name} has neither {missing}.plan.json nor {missing}.no-plan.json"
            )));
        }
        variants.push(Variant {
            name,
            record,
            entries,
        });
    }
    Ok(variants)
}

/// How one engine run measures.
pub struct EngineRunSetup<'a> {
    pub protocol: &'a Protocol,
    pub core: usize,
    pub probe: &'a mut dyn LoadProbe,
    pub clocks: &'a MeasurementClocks<'a>,
    pub args: Vec<String>,
}

/// Runs every (workload, variant) cell of `run_directory` on `engine`, the
/// variants limited to `only_variants` when given. A cell whose
/// `.cell.json` exists is refused before anything runs, so an engine run can go
/// variant by variant but never overwrites a result. A cell that fails is
/// written as `NotRun` and the next one runs.
pub fn run_engine(
    run_directory: &RunDirectory,
    engine: &dyn EngineUnderTest,
    only_variants: Option<&[String]>,
    mut setup: EngineRunSetup,
) -> Result<Vec<EngineRunCell>, BenchmarkExternalError> {
    let workloads = read_workloads(run_directory)?;
    let mut variants = read_variants(run_directory, &workloads)?;
    if let Some(only) = only_variants {
        if let Some(unknown) = only
            .iter()
            .find(|v| !variants.iter().any(|x| &x.name == *v))
        {
            return Err(BenchmarkExternalError::Layout(format!(
                "no variant {unknown} under plans/"
            )));
        }
        variants.retain(|variant| only.contains(&variant.name));
    } else {
        let any = !variants.is_empty();
        variants.retain(|variant| variant.record.run_by_default);
        if any && variants.is_empty() {
            return Err(BenchmarkExternalError::Layout(
                "no variant under plans/ runs by default; name one with --variant".into(),
            ));
        }
    }
    let engine_id = engine.description().id;
    let engine_directory = run_directory.path().join("results").join(&engine_id);
    for variant in &variants {
        for workload in workloads.keys() {
            let cell = cell_path(&engine_directory, workload, &variant.name);
            if cell.exists() {
                return Err(BenchmarkExternalError::CellExists(cell));
            }
        }
    }
    std::fs::create_dir_all(&engine_directory).map_err(|e| io(&engine_directory, e))?;
    write_json(
        &engine_directory.join(format!(
            "engine-run-{}.json",
            Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
        )),
        &EngineRunRecord {
            engine: engine.description(),
            cpu_accounting: engine.cpu_accounting(),
            protocol: setup.protocol.clone(),
            core: setup.core,
            args: setup.args.clone(),
        },
    )?;

    let mut cells = Vec::new();
    for variant in &variants {
        for (workload_name, workload) in &workloads {
            let outcome = match &variant.entries[workload_name] {
                PlanEntry::NoPlan { reason } => CellOutcome::NoPlan {
                    reason: reason.clone(),
                },
                PlanEntry::PlanFile(plan_file) => match run_cell(
                    run_directory,
                    engine,
                    workload,
                    variant,
                    serde_json::json!({ "plan_file": plan_file }),
                    &mut setup,
                ) {
                    Ok(()) => CellOutcome::Ran,
                    Err(error) => CellOutcome::NotRun { error },
                },
                PlanEntry::QueryTextOnly => match run_cell(
                    run_directory,
                    engine,
                    workload,
                    variant,
                    serde_json::json!({}),
                    &mut setup,
                ) {
                    Ok(()) => CellOutcome::Ran,
                    Err(error) => CellOutcome::NotRun { error },
                },
            };
            let cell = EngineRunCell {
                engine: engine_id.clone(),
                workload: workload_name.clone(),
                variant: variant.name.clone(),
                outcome,
            };
            write_json(
                &cell_path(&engine_directory, workload_name, &variant.name),
                &cell,
            )?;
            cells.push(cell);
        }
    }
    Ok(cells)
}

/// Runs one cell and writes its `ArmResult`; the error text otherwise.
fn run_cell(
    run_directory: &RunDirectory,
    engine: &dyn EngineUnderTest,
    workload: &WorkloadSpec,
    variant: &Variant,
    engine_options: serde_json::Value,
    setup: &mut EngineRunSetup,
) -> Result<(), String> {
    let schedule = ArmSchedule::for_workload(workload).map_err(|e| e.to_string())?;
    let arm = ArmSpec {
        name: variant.name.clone(),
        raw_data_retained: true,
        engine_options,
    };
    let result = run_arm(
        engine,
        workload,
        &arm,
        &schedule,
        setup.protocol,
        setup.core,
        setup.probe,
        setup.clocks,
    )
    .map_err(|e| e.to_string())?;
    if !variant.record.substitutions_allowed {
        if let Some(first) = result.plan.substitutions.first() {
            return Err(format!(
                "variant {} allows no substitutions, but the engine substituted: {first}",
                variant.name
            ));
        }
    }
    run_directory
        .write_arm(&result)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// One row of `summary.csv`: one cell of one engine. Every number is absolute;
/// `None` is an empty field (the cell did not run, or the engine has no such
/// counter).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub workload: String,
    pub engine: String,
    pub variant: String,
    /// `ok`, `no plan: <reason>` or `not run: <error>`.
    pub status: String,
    /// `thread` (in-process, the calling thread's CPU), `processes` or
    /// `cgroup` (a server's whole process or group, background threads included).
    pub cpu_accounting: Option<&'static str>,
    /// Sum over refresh chunks of the median ingest CPU, per ingested sample.
    pub ingest_cpu_ns_per_sample: Option<f64>,
    /// Mean over evaluation times of the median evaluation CPU, in ms.
    pub query_cpu_ms_per_evaluation: Option<f64>,
    /// The same for the engine's own per-query CPU counter; `None` when any
    /// step lacks it.
    pub engine_counter_cpu_ms_per_evaluation: Option<f64>,
    /// A `cgroup` server's CPU over the whole arm, steps and the work between
    /// them, over every install, in ms.
    pub server_cpu_ms_whole_arm: Option<f64>,
    /// A `cgroup` server's `memory.current` before the installs.
    pub server_memory_before_bytes: Option<u64>,
    /// A `cgroup` server's `memory.current` after the run, every install held.
    pub server_memory_after_bytes: Option<u64>,
    /// A `cgroup` server's `memory.peak` over the container's whole life.
    pub server_lifetime_peak_bytes: Option<u64>,
    pub server_process_proportional_set_size_before_bytes: Option<u64>,
    pub server_process_proportional_set_size_after_bytes: Option<u64>,
    pub server_process_peak_resident_set_size_bytes: Option<u64>,
    pub installs_held: Option<u32>,
    /// View plus raw bytes the last install holds.
    pub stored_bytes_last_install: Option<u64>,
    /// The truth has no answers for this workload (its cell did not run).
    pub no_truth: bool,
    pub accuracy: Option<PointErrors>,
    pub substitutions: Vec<String>,
    pub sharing_lost: Vec<String>,
}

/// Relative errors of answers against the truth over a set of points.
#[derive(Debug, Clone, PartialEq)]
pub struct PointErrors {
    pub max_relative_error: Option<f64>,
    pub median_relative_error: Option<f64>,
    /// Points both answered.
    pub points: u64,
    /// Truth points the answer lacks.
    pub missing_points: u64,
    /// Answer points the truth lacks.
    pub extra_points: u64,
}

/// One row of `accuracy.csv`: one query of one cell.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryAccuracy {
    pub workload: String,
    pub engine: String,
    pub variant: String,
    pub query: String,
    pub errors: PointErrors,
    pub scores: BTreeMap<String, ScoreSummary>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoreSummary {
    pub max: f64,
    pub median: f64,
    pub points: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportTruth {
    Declared,
    Cells { engine: String, variant: String },
}

impl ReportTruth {
    pub fn parse(text: &str) -> Result<ReportTruth, String> {
        if text == "declared" {
            return Ok(ReportTruth::Declared);
        }
        let (engine, variant) = text
            .split_once('/')
            .ok_or_else(|| format!("--truth must be declared or <engine>/<variant>, not {text}"))?;
        Ok(ReportTruth::Cells {
            engine: engine.to_string(),
            variant: variant.to_string(),
        })
    }

    pub fn name(&self) -> String {
        match self {
            ReportTruth::Declared => "declared".to_string(),
            ReportTruth::Cells { engine, variant } => format!("{engine}/{variant}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `engine/variant` of the truth, or `declared`.
    pub truth: String,
    pub rows: Vec<ReportRow>,
    pub accuracy: Vec<QueryAccuracy>,
}

/// `|a - t| / |t|`; equal values (0 and 0, the same infinity) 0; both NaN
/// 0; one NaN, `t = 0` with `a != 0`, or an infinity on one side only (or
/// opposite infinities), infinite.
pub fn relative_error(answer: f64, truth: f64) -> f64 {
    if answer.is_nan() && truth.is_nan() {
        0.0
    } else if answer.is_nan() || truth.is_nan() {
        f64::INFINITY
    } else if answer == truth {
        0.0
    } else if truth == 0.0 || truth.is_infinite() || answer.is_infinite() {
        f64::INFINITY
    } else {
        (answer - truth).abs() / truth.abs()
    }
}

/// The middle value; the mean of the two middle values for an even count.
pub(crate) fn median_of(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    Some(if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    })
}

fn point_errors(errors: &[f64], missing_points: u64, extra_points: u64) -> PointErrors {
    PointErrors {
        max_relative_error: errors.iter().copied().reduce(f64::max),
        median_relative_error: median_of(errors),
        points: errors.len() as u64,
        missing_points,
        extra_points,
    }
}

/// One query's errors against the truth, before they are summarized.
#[derive(Debug, Default)]
struct QueryErrors {
    /// One relative error per point both answered.
    errors: Vec<f64>,
    missing_points: u64,
    extra_points: u64,
}

/// Per query, the errors of `result`'s answers against `truth`'s. The two
/// must answer at the same times.
fn errors_against(
    result: &ArmResult,
    truth: &ArmResult,
) -> Result<BTreeMap<String, QueryErrors>, String> {
    let times = |r: &ArmResult| r.answers.iter().map(|(t, _)| *t).collect::<Vec<_>>();
    if times(result) != times(truth) {
        return Err(format!(
            "workload {}: {}/{} answered at {:?} but the truth at {:?}",
            result.workload,
            result.engine,
            result.arm,
            times(result),
            times(truth)
        ));
    }
    let mut per_query: BTreeMap<String, QueryErrors> = BTreeMap::new();
    for ((_, answers), (_, truth_answers)) in result.answers.iter().zip(&truth.answers) {
        let queries: std::collections::BTreeSet<&String> =
            answers.0.keys().chain(truth_answers.0.keys()).collect();
        for query in queries {
            let entry = per_query.entry(query.clone()).or_default();
            let empty = BTreeMap::new();
            let got = answers.0.get(query).unwrap_or(&empty);
            let want = truth_answers.0.get(query).unwrap_or(&empty);
            for (series, truth_value) in want {
                match got.get(series) {
                    Some(value) => entry.errors.push(relative_error(*value, *truth_value)),
                    None => entry.missing_points += 1,
                }
            }
            entry.extra_points += got
                .keys()
                .filter(|series| !want.contains_key(*series))
                .count() as u64;
        }
    }
    Ok(per_query)
}

/// Joins every engine run's cells into the report, with accuracy against
/// `truth`: the cells of one engine run, or each query's declared statistic
/// computed from the workload's data. Refuses a truth with no cell, a
/// workload whose ran cells ingested different numbers of samples (the
/// engine runs did not read the same data), and, against declared
/// statistics, a workload that no longer generates the samples its cells
/// ingested.
pub fn report(
    run_directory: &RunDirectory,
    truth: &ReportTruth,
) -> Result<Report, BenchmarkExternalError> {
    let results = run_directory.path().join("results");
    let mut cells: Vec<(EngineRunCell, Option<ArmResult>)> = Vec::new();
    for engine_directory in sorted_entries(&results)? {
        for path in sorted_entries(&engine_directory)? {
            if !file_name(&path).ends_with(".cell.json") {
                continue;
            }
            let cell: EngineRunCell = read_json(&path)?;
            let result = match cell.outcome {
                CellOutcome::Ran => Some(read_json::<ArmResult>(
                    &engine_directory.join(format!("{}__{}.json", cell.workload, cell.variant)),
                )?),
                _ => None,
            };
            cells.push((cell, result));
        }
    }
    let (truth_engine, truth_variant) = match truth {
        ReportTruth::Cells { engine, variant } => (engine.as_str(), variant.as_str()),
        ReportTruth::Declared => ("", ""),
    };
    if matches!(truth, ReportTruth::Cells { .. })
        && !cells
            .iter()
            .any(|(cell, _)| cell.engine == truth_engine && cell.variant == truth_variant)
    {
        return Err(BenchmarkExternalError::Report(format!(
            "the truth {truth_engine}/{truth_variant} has no cells under results/"
        )));
    }
    let workloads = match truth {
        ReportTruth::Declared => read_workloads(run_directory)?,
        ReportTruth::Cells { .. } => BTreeMap::new(),
    };
    let mut series_of: BTreeMap<String, (Vec<Series>, u64)> = BTreeMap::new();
    let mut samples_per_workload: BTreeMap<&str, (u64, String)> = BTreeMap::new();
    for (cell, result) in &cells {
        let Some(result) = result else { continue };
        let here = format!("{}/{}", cell.engine, cell.variant);
        let (samples, first) = samples_per_workload
            .entry(&cell.workload)
            .or_insert((result.ingested_samples, here.clone()));
        if *samples != result.ingested_samples {
            return Err(BenchmarkExternalError::Report(format!(
                "workload {}: {first} ingested {samples} samples but {here} {}; the engine runs did not read the same data",
                cell.workload, result.ingested_samples
            )));
        }
    }
    let truth_of = |workload: &str| {
        cells.iter().find_map(|(cell, result)| {
            (cell.engine == truth_engine
                && cell.variant == truth_variant
                && cell.workload == workload)
                .then_some(result.as_ref())
                .flatten()
        })
    };
    let mut rows = Vec::new();
    let mut accuracy = Vec::new();
    for (cell, result) in &cells {
        let status = match &cell.outcome {
            CellOutcome::Ran => "ok".to_string(),
            CellOutcome::NoPlan { reason } => format!("no plan: {reason}"),
            CellOutcome::NotRun { error } => format!("not run: {error}"),
        };
        let mut row = ReportRow {
            workload: cell.workload.clone(),
            engine: cell.engine.clone(),
            variant: cell.variant.clone(),
            status,
            cpu_accounting: None,
            ingest_cpu_ns_per_sample: None,
            query_cpu_ms_per_evaluation: None,
            engine_counter_cpu_ms_per_evaluation: None,
            server_cpu_ms_whole_arm: None,
            server_memory_before_bytes: None,
            server_memory_after_bytes: None,
            server_lifetime_peak_bytes: None,
            server_process_proportional_set_size_before_bytes: None,
            server_process_proportional_set_size_after_bytes: None,
            server_process_peak_resident_set_size_bytes: None,
            installs_held: None,
            stored_bytes_last_install: None,
            no_truth: false,
            accuracy: None,
            substitutions: Vec::new(),
            sharing_lost: Vec::new(),
        };
        if let Some(result) = result {
            fill_measurements(&mut row, result);
            if matches!(truth, ReportTruth::Declared) {
                let workload = workloads.get(&cell.workload).ok_or_else(|| {
                    BenchmarkExternalError::Report(format!(
                        "workload {} is not in workloads/",
                        cell.workload
                    ))
                })?;
                if !series_of.contains_key(&cell.workload) {
                    series_of.insert(cell.workload.clone(), series_of_workload(workload)?);
                }
                let (series, generated) = &series_of[&cell.workload];
                if *generated != result.ingested_samples {
                    return Err(BenchmarkExternalError::Report(format!(
                        "workload {}: workloads/{}.json generates {generated} samples but {}/{} ingested {}",
                        cell.workload, cell.workload, cell.engine, cell.variant, result.ingested_samples
                    )));
                }
                let per_query = declared_scores(workload, series, result)
                    .map_err(BenchmarkExternalError::Report)?;
                if per_query.is_empty() {
                    row.no_truth = true;
                } else {
                    let mut all = DeclaredQuery::default();
                    for (query, declared) in per_query {
                        all.relative_errors
                            .extend_from_slice(&declared.relative_errors);
                        all.points += declared.points;
                        all.missing_points += declared.missing_points;
                        all.extra_points += declared.extra_points;
                        accuracy.push(QueryAccuracy {
                            workload: cell.workload.clone(),
                            engine: cell.engine.clone(),
                            variant: cell.variant.clone(),
                            query,
                            errors: declared.point_errors(),
                            scores: declared.scores,
                        });
                    }
                    row.accuracy = Some(all.point_errors());
                }
            } else if let Some(truth) = truth_of(&cell.workload) {
                let per_query =
                    errors_against(result, truth).map_err(BenchmarkExternalError::Report)?;
                let (mut all, mut missing, mut extra) = (Vec::new(), 0, 0);
                for (query, errors) in per_query {
                    all.extend_from_slice(&errors.errors);
                    missing += errors.missing_points;
                    extra += errors.extra_points;
                    accuracy.push(QueryAccuracy {
                        workload: cell.workload.clone(),
                        engine: cell.engine.clone(),
                        variant: cell.variant.clone(),
                        query,
                        errors: point_errors(
                            &errors.errors,
                            errors.missing_points,
                            errors.extra_points,
                        ),
                        scores: BTreeMap::new(),
                    });
                }
                row.accuracy = Some(point_errors(&all, missing, extra));
            } else {
                row.no_truth = true;
            }
        }
        rows.push(row);
    }
    Ok(Report {
        truth: truth.name(),
        rows,
        accuracy,
    })
}

fn series_of_workload(
    workload: &WorkloadSpec,
) -> Result<(Vec<Series>, u64), BenchmarkExternalError> {
    let refresh_ms = ArmSchedule::for_workload(workload)
        .map_err(|e| BenchmarkExternalError::Report(format!("workload {}: {e}", workload.name)))?
        .refresh_ms;
    let mut points: BTreeMap<SeriesLabels, Vec<(i64, f64)>> = BTreeMap::new();
    let mut generated = 0u64;
    let chunks = workload_chunks(&workload.data, refresh_ms)
        .map_err(|e| BenchmarkExternalError::Report(format!("workload {}: {e}", workload.name)))?;
    for chunk in chunks {
        let chunk = chunk.map_err(|e| {
            BenchmarkExternalError::Report(format!("workload {}: {e}", workload.name))
        })?;
        generated += chunk.samples.len() as u64;
        for sample in chunk.samples {
            points
                .entry((*sample.labels).clone())
                .or_default()
                .push((sample.ts_ms, sample.value));
        }
    }
    let series = points
        .into_iter()
        .map(|(labels, mut points)| {
            points.sort_by_key(|(t, _)| *t);
            Series { labels, points }
        })
        .collect();
    Ok((series, generated))
}

const NOT_SCORES: [&str; 7] = [
    "truth",
    "estimate",
    "items",
    "grid_points",
    "probes",
    "stream_items",
    "stream_distinct",
];

#[derive(Debug, Default)]
struct DeclaredQuery {
    relative_errors: Vec<f64>,
    points: u64,
    missing_points: u64,
    extra_points: u64,
    scores: BTreeMap<String, ScoreSummary>,
}

impl DeclaredQuery {
    fn point_errors(&self) -> PointErrors {
        PointErrors {
            max_relative_error: self.relative_errors.iter().copied().max_by(f64::total_cmp),
            median_relative_error: median_of(&self.relative_errors),
            points: self.points,
            missing_points: self.missing_points,
            extra_points: self.extra_points,
        }
    }
}

fn declared_scores(
    workload: &WorkloadSpec,
    series: &[Series],
    result: &ArmResult,
) -> Result<BTreeMap<String, DeclaredQuery>, String> {
    let ends: Vec<i64> = result.answers.iter().map(|(t, _)| *t).collect();
    let mut per_query = BTreeMap::new();
    for query in &workload.queries {
        let Some(declared) = &query.truth else {
            continue;
        };
        let answered: Vec<AnswerAt> = result
            .answers
            .iter()
            .flat_map(|(t, answers)| {
                answers
                    .0
                    .get(&query.id)
                    .into_iter()
                    .flatten()
                    .map(|(labels, value)| (*t, labels.clone(), *value))
            })
            .collect();
        let cells = QueryTruth::new(declared, query.window_ms as i64)
            .score(series, &ends, &answered)
            .map_err(|e| format!("workload {}, query {}: {e}", workload.name, query.id))?;
        let mut summary = DeclaredQuery::default();
        let mut values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut truth_keys = std::collections::HashSet::new();
        for (end, labels, scores) in &cells {
            truth_keys.insert((*end, labels.clone()));
            match scores {
                None => summary.missing_points += 1,
                Some(scores) => {
                    summary.points += 1;
                    if let (Some(truth), Some(estimate)) =
                        (scores.get("truth"), scores.get("estimate"))
                    {
                        summary
                            .relative_errors
                            .push(relative_error(*estimate, *truth));
                    }
                    for (name, value) in scores {
                        if !NOT_SCORES.contains(&name.as_str()) {
                            values.entry(name.clone()).or_default().push(*value);
                        }
                    }
                }
            }
        }
        if !matches!(declared.statistic, Statistic::TopK { .. }) {
            summary.extra_points = answered
                .iter()
                .filter(|(t, labels, _)| !truth_keys.contains(&(*t, without_name(labels))))
                .count() as u64;
        }
        summary.scores = values
            .into_iter()
            .map(|(name, values)| {
                (
                    name,
                    ScoreSummary {
                        max: values
                            .iter()
                            .copied()
                            .max_by(f64::total_cmp)
                            .unwrap_or(f64::NAN),
                        median: median_of(&values).unwrap_or(f64::NAN),
                        points: values.len() as u64,
                    },
                )
            })
            .collect();
        per_query.insert(query.id.clone(), summary);
    }
    Ok(per_query)
}

fn fill_measurements(row: &mut ReportRow, result: &ArmResult) {
    let step_cpu_ns =
        |cost: &crate::harness::engine::StepCost| cost.thread_cpu_ns.unwrap_or(cost.process_cpu_ns);
    row.cpu_accounting = Some(match result.cpu_source.as_str() {
        "calling-thread" => "thread",
        "schedstat" => "processes",
        _ => "cgroup",
    });
    let ingest_ns: u64 = result
        .ingest
        .iter()
        .map(|job| step_cpu_ns(&job.median))
        .sum();
    row.ingest_cpu_ns_per_sample =
        (result.ingested_samples > 0).then(|| ingest_ns as f64 / result.ingested_samples as f64);
    let evaluations = result.evaluate.len();
    if evaluations > 0 {
        let query_ns: u64 = result
            .evaluate
            .iter()
            .map(|job| step_cpu_ns(&job.median))
            .sum();
        row.query_cpu_ms_per_evaluation = Some(query_ns as f64 / 1e6 / evaluations as f64);
        row.engine_counter_cpu_ms_per_evaluation = result
            .evaluate
            .iter()
            .map(|job| job.median.report.engine_reported_cpu_ns)
            .sum::<Option<u64>>()
            .map(|ns| ns as f64 / 1e6 / evaluations as f64);
    }
    row.server_cpu_ms_whole_arm = result.server_cpu_ns_whole_arm.map(|ns| ns as f64 / 1e6);
    row.server_memory_before_bytes = result
        .server_memory_before
        .map(|memory| memory.current_bytes);
    row.server_memory_after_bytes = result
        .server_memory_after
        .map(|memory| memory.current_bytes);
    row.server_lifetime_peak_bytes = result
        .server_memory_after
        .and_then(|memory| memory.lifetime_peak_bytes);
    row.server_process_proportional_set_size_before_bytes = result
        .server_process_memory_before
        .map(|memory| memory.proportional_set_size_bytes);
    row.server_process_proportional_set_size_after_bytes = result
        .server_process_memory_after
        .map(|memory| memory.proportional_set_size_bytes);
    row.server_process_peak_resident_set_size_bytes = result
        .server_process_memory_after
        .and_then(|memory| memory.peak_resident_set_size_bytes);
    row.installs_held = Some(result.installs_held);
    row.stored_bytes_last_install =
        Some(result.footprint.view_bytes + result.footprint.raw_retained_bytes);
    row.substitutions = result.plan.substitutions.clone();
    row.sharing_lost = result.plan.sharing_lost.clone();
}

/// The `summary.csv` header.
pub const SUMMARY_CSV_HEADER: &str = "workload,engine,variant,status,cpu_accounting,ingest_cpu_ns_per_sample,query_cpu_ms_per_evaluation,engine_counter_cpu_ms_per_evaluation,server_cpu_ms_whole_arm,server_memory_before_bytes,server_memory_after_bytes,server_lifetime_peak_bytes,server_process_proportional_set_size_before_bytes,server_process_proportional_set_size_after_bytes,server_process_peak_resident_set_size_bytes,installs_held,stored_bytes_last_install,max_relative_error,median_relative_error,points,missing_points,extra_points,substitutions,sharing_lost";

pub const SCORES_CSV_HEADER: &str = "workload,engine,variant,query,score,max,median,points";

/// The `accuracy.csv` header.
pub const ACCURACY_CSV_HEADER: &str =
    "workload,engine,variant,query,max_relative_error,median_relative_error,points,missing_points,extra_points";

fn field<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// A CSV field: quoted when it holds a comma, quote or newline.
fn csv_text(text: &str) -> String {
    if text.contains([',', '"', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

fn error_fields(errors: Option<&PointErrors>) -> String {
    match errors {
        Some(e) => format!(
            "{},{},{},{},{}",
            field(e.max_relative_error),
            field(e.median_relative_error),
            e.points,
            e.missing_points,
            e.extra_points
        ),
        None => ",,,,".to_string(),
    }
}

impl Report {
    /// `summary.csv`: the header and one line per row.
    pub fn summary_csv(&self) -> String {
        let mut text = format!("{SUMMARY_CSV_HEADER}\n");
        for row in &self.rows {
            text.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                csv_text(&row.workload),
                csv_text(&row.engine),
                csv_text(&row.variant),
                csv_text(&row.status),
                field(row.cpu_accounting),
                field(row.ingest_cpu_ns_per_sample),
                field(row.query_cpu_ms_per_evaluation),
                field(row.engine_counter_cpu_ms_per_evaluation),
                field(row.server_cpu_ms_whole_arm),
                field(row.server_memory_before_bytes),
                field(row.server_memory_after_bytes),
                field(row.server_lifetime_peak_bytes),
                field(row.server_process_proportional_set_size_before_bytes),
                field(row.server_process_proportional_set_size_after_bytes),
                field(row.server_process_peak_resident_set_size_bytes),
                field(row.installs_held),
                field(row.stored_bytes_last_install),
                error_fields(row.accuracy.as_ref()),
                csv_text(&row.substitutions.join(";")),
                csv_text(&row.sharing_lost.join(";")),
            ));
        }
        text
    }

    /// `accuracy.csv`: one line per query of every ran cell with a truth.
    pub fn accuracy_csv(&self) -> String {
        let mut text = format!("{ACCURACY_CSV_HEADER}\n");
        for row in &self.accuracy {
            text.push_str(&format!(
                "{},{},{},{},{}\n",
                csv_text(&row.workload),
                csv_text(&row.engine),
                csv_text(&row.variant),
                csv_text(&row.query),
                error_fields(Some(&row.errors))
            ));
        }
        text
    }

    pub fn scores_csv(&self) -> String {
        let mut text = format!("{SCORES_CSV_HEADER}\n");
        for row in &self.accuracy {
            for (name, summary) in &row.scores {
                text.push_str(&format!(
                    "{},{},{},{},{},{},{},{}\n",
                    csv_text(&row.workload),
                    csv_text(&row.engine),
                    csv_text(&row.variant),
                    csv_text(&row.query),
                    csv_text(name),
                    summary.max,
                    summary.median,
                    summary.points,
                ));
            }
        }
        text
    }

    /// `summary.md`: rows = workloads, columns = `engine/variant`, sorted.
    /// A ran cell reads `q 2.000 ms, i 2000.0 ns/sample, err 0.100 [cgroup]`.
    pub fn summary_markdown(&self) -> String {
        let columns: std::collections::BTreeSet<String> = self
            .rows
            .iter()
            .map(|row| format!("{}/{}", row.engine, row.variant))
            .collect();
        let workloads: std::collections::BTreeSet<&str> =
            self.rows.iter().map(|row| row.workload.as_str()).collect();
        let mut text = format!(
            "Absolute CPU per engine: `q` = query CPU per evaluation, `i` = ingest CPU per sample, `err` = max relative error against {}. `[thread]` engine runs count the calling thread only; `[cgroup]` engine runs count the whole server.\n\n| workload |",
            self.truth
        );
        for column in &columns {
            text.push_str(&format!(" {column} |"));
        }
        text.push_str("\n|---|");
        text.push_str(&"---|".repeat(columns.len()));
        text.push('\n');
        for workload in workloads {
            text.push_str(&format!("| {workload} |"));
            for column in &columns {
                let row = self.rows.iter().find(|row| {
                    row.workload == workload && format!("{}/{}", row.engine, row.variant) == *column
                });
                let cell = match row {
                    None => String::new(),
                    Some(row) if row.status == "ok" => format!(
                        "q {} ms, i {} ns/sample, err {} [{}]",
                        row.query_cpu_ms_per_evaluation
                            .map_or("-".into(), |v| format!("{v:.3}")),
                        row.ingest_cpu_ns_per_sample
                            .map_or("-".into(), |v| format!("{v:.1}")),
                        if row.no_truth {
                            "- (no truth)".to_string()
                        } else {
                            row.accuracy
                                .as_ref()
                                .and_then(|e| e.max_relative_error)
                                .map_or("-".into(), |v| format!("{v:.3}"))
                        },
                        row.cpu_accounting.unwrap_or("-"),
                    ),
                    Some(row) => {
                        let first_line = row.status.lines().next().unwrap_or("");
                        first_line
                            .chars()
                            .take(80)
                            .collect::<String>()
                            .replace('|', "/")
                    }
                };
                text.push_str(&format!(" {cell} |"));
            }
            text.push('\n');
        }
        text
    }
}

/// Writes `summary.csv`, `summary.md` and `accuracy.csv` in the run
/// directory, replacing earlier reports (a report is derived, never a result).
pub fn write_report(
    run_directory: &RunDirectory,
    report: &Report,
) -> Result<(), BenchmarkExternalError> {
    for (name, text) in [
        ("summary.csv", report.summary_csv()),
        ("summary.md", report.summary_markdown()),
        ("accuracy.csv", report.accuracy_csv()),
        ("scores.csv", report.scores_csv()),
    ] {
        let path = run_directory.path().join(name);
        std::fs::write(&path, text).map_err(|e| io(&path, e))?;
    }
    Ok(())
}

fn cell_path(engine_directory: &Path, workload: &str, variant: &str) -> PathBuf {
    engine_directory.join(format!("{workload}__{variant}.cell.json"))
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, BenchmarkExternalError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| io(dir, e))?
        .map(|entry| entry.map(|entry| entry.path()).map_err(|e| io(dir, e)))
        .collect::<Result<_, _>>()?;
    paths.sort();
    Ok(paths)
}

fn sorted_files(dir: &Path) -> Result<Vec<PathBuf>, BenchmarkExternalError> {
    let paths = sorted_entries(dir)?;
    if let Some(sub) = paths.iter().find(|path| !path.is_file()) {
        return Err(BenchmarkExternalError::Layout(format!(
            "{} is not a file",
            sub.display()
        )));
    }
    Ok(paths)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_stem_with_suffix(path: &Path, suffix: &str) -> Result<String, BenchmarkExternalError> {
    file_name(path)
        .strip_suffix(suffix)
        .map(str::to_string)
        .ok_or_else(|| {
            BenchmarkExternalError::Layout(format!("{} does not end in {suffix}", path.display()))
        })
}

fn absolute(path: &Path) -> Result<PathBuf, BenchmarkExternalError> {
    std::path::absolute(path).map_err(|e| io(path, e))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, BenchmarkExternalError> {
    let text = std::fs::read_to_string(path).map_err(|e| io(path, e))?;
    serde_json::from_str(&text).map_err(|error| BenchmarkExternalError::Json {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn io(path: &Path, error: std::io::Error) -> BenchmarkExternalError {
    BenchmarkExternalError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkExternalError {
    /// The run directory does not have the layout of the module docs.
    #[error("{0}")]
    Layout(String),
    /// A cell already has a result; results are never overwritten.
    #[error("{} exists; results are never overwritten", .0.display())]
    CellExists(PathBuf),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Json { path: PathBuf, message: String },
    #[error(transparent)]
    RunDirectory(#[from] RunDirectoryError),
    /// Inputs the report cannot join, such as engine runs that ingested different data.
    #[error("{0}")]
    Report(String),
}

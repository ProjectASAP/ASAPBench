//! Lanes over one run directory, and the report that joins them.
//!
//! A lane is one engine under test run as its own process over a run
//! directory another step prepared:
//!
//! ```text
//! workloads/<workload>.json                 harness WorkloadSpec
//! plans/<variant>/variant.json              {"substitutions_allowed": false}
//! plans/<variant>/<workload>.plan.json      a plan file, opaque to the harness
//! plans/<variant>/<workload>.no-plan.json   {"reason": "ExcludedByDeployment"}
//! ```
//!
//! [`run_lane`] runs every (workload, variant) cell on the engine and writes
//! `results/<lane>/<workload>__<variant>.cell.json` (always) and the
//! `ArmResult` beside it (when the cell ran). [`report`] joins every lane's
//! cells into one row per cell, with absolute CPU, memory and accuracy
//! against a truth lane, and [`write_report`] writes `summary.csv`,
//! `summary.md` and `accuracy.csv`.
//!
//! Example: lane `clickhouse` over variants `clickhouse.equivalent-only` and
//! `clickhouse.with-substitutes`, lane `asap` over both, report with truth
//! `asap/exact`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::harness::arm_run::{run_arm, ArmSchedule};
use crate::harness::engine::{ArmSpec, CpuAccounting, EngineDescription, EngineUnderTest, WorkloadSpec};
use crate::harness::protocol::{LoadProbe, MeasurementClocks, Protocol};
use crate::harness::records::ArmResult;
use crate::harness::rundir::{check_result_name, write_json, RunDir, RunDirError};

/// `plans/<variant>/variant.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantRecord {
    /// `false` for a plan restricted to functions equivalent to the
    /// planner's: an engine that reports a substitution then disagrees with
    /// the capability file the plan was made under, and the cell fails.
    pub substitutions_allowed: bool,
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
}

/// One variant of the run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub name: String,
    pub record: VariantRecord,
    /// Per workload.
    pub entries: BTreeMap<String, PlanEntry>,
}

/// One (workload, variant) cell of one lane: `<workload>__<variant>.cell.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneCell {
    pub lane: String,
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

/// `results/<lane>/lane-<UTC>.json`: one lane process and how it measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneRecord {
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
    run_dir: &RunDir,
) -> Result<BTreeMap<String, WorkloadSpec>, BenchExternalError> {
    let mut workloads = BTreeMap::new();
    for path in sorted_files(&run_dir.path().join("workloads"))? {
        let stem = file_stem_with_suffix(&path, ".json")?;
        // Refused here, before any cell runs, not when its result is written.
        check_result_name("workload", &stem, true)?;
        let spec: WorkloadSpec = read_json(&path)?;
        if spec.name != stem {
            return Err(BenchExternalError::Layout(format!(
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
    run_dir: &RunDir,
    workloads: &BTreeMap<String, WorkloadSpec>,
) -> Result<Vec<Variant>, BenchExternalError> {
    let mut variants = Vec::new();
    for dir in sorted_entries(&run_dir.path().join("plans"))? {
        if !dir.is_dir() {
            return Err(BenchExternalError::Layout(format!(
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
                return Err(BenchExternalError::Layout(format!(
                    "{} is neither variant.json, <workload>.plan.json nor <workload>.no-plan.json",
                    path.display()
                )));
            };
            if !workloads.contains_key(&workload) {
                return Err(BenchExternalError::Layout(format!(
                    "{} names workload '{workload}', which is not in workloads/",
                    path.display()
                )));
            }
            if entries.insert(workload.clone(), entry).is_some() {
                return Err(BenchExternalError::Layout(format!(
                    "variant {name} has both {workload}.plan.json and {workload}.no-plan.json"
                )));
            }
        }
        let record = record.ok_or_else(|| {
            BenchExternalError::Layout(format!("variant {name} has no variant.json"))
        })?;
        if let Some(missing) = workloads.keys().find(|w| !entries.contains_key(*w)) {
            return Err(BenchExternalError::Layout(format!(
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

/// How one lane process measures.
pub struct LaneSetup<'a> {
    pub protocol: &'a Protocol,
    pub core: usize,
    pub probe: &'a mut dyn LoadProbe,
    pub clocks: &'a MeasurementClocks<'a>,
    pub args: Vec<String>,
}

/// Runs every (workload, variant) cell of `run_dir` on `engine`, the
/// variants limited to `only_variants` when given. A cell whose
/// `.cell.json` exists is refused before anything runs, so a lane can run
/// variant by variant but never overwrites a result. A cell that fails is
/// written as `NotRun` and the next one runs.
pub fn run_lane(
    run_dir: &RunDir,
    engine: &dyn EngineUnderTest,
    only_variants: Option<&[String]>,
    mut setup: LaneSetup,
) -> Result<Vec<LaneCell>, BenchExternalError> {
    let workloads = read_workloads(run_dir)?;
    let mut variants = read_variants(run_dir, &workloads)?;
    if let Some(only) = only_variants {
        if let Some(unknown) = only
            .iter()
            .find(|v| !variants.iter().any(|x| &x.name == *v))
        {
            return Err(BenchExternalError::Layout(format!(
                "no variant {unknown} under plans/"
            )));
        }
        variants.retain(|variant| only.contains(&variant.name));
    }
    let lane = engine.description().id;
    let lane_dir = run_dir.path().join("results").join(&lane);
    for variant in &variants {
        for workload in workloads.keys() {
            let cell = cell_path(&lane_dir, workload, &variant.name);
            if cell.exists() {
                return Err(BenchExternalError::CellExists(cell));
            }
        }
    }
    std::fs::create_dir_all(&lane_dir).map_err(|e| io(&lane_dir, e))?;
    write_json(
        &lane_dir.join(format!(
            "lane-{}.json",
            Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
        )),
        &LaneRecord {
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
                PlanEntry::PlanFile(plan_file) => {
                    match run_cell(run_dir, engine, workload, variant, plan_file, &mut setup) {
                        Ok(()) => CellOutcome::Ran,
                        Err(error) => CellOutcome::NotRun { error },
                    }
                }
            };
            let cell = LaneCell {
                lane: lane.clone(),
                workload: workload_name.clone(),
                variant: variant.name.clone(),
                outcome,
            };
            write_json(&cell_path(&lane_dir, workload_name, &variant.name), &cell)?;
            cells.push(cell);
        }
    }
    Ok(cells)
}

/// Runs one cell and writes its `ArmResult`; the error text otherwise.
fn run_cell(
    run_dir: &RunDir,
    engine: &dyn EngineUnderTest,
    workload: &WorkloadSpec,
    variant: &Variant,
    plan_file: &Path,
    setup: &mut LaneSetup,
) -> Result<(), String> {
    let schedule = ArmSchedule::for_workload(workload).map_err(|e| e.to_string())?;
    let arm = ArmSpec {
        name: variant.name.clone(),
        raw_data_retained: true,
        engine_options: serde_json::json!({ "plan_file": plan_file }),
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
    run_dir.write_arm(&result).map_err(|e| e.to_string())?;
    Ok(())
}

/// One row of `summary.csv`: one cell of one lane. Every number is absolute;
/// `None` is an empty field (the cell did not run, or the engine has no such
/// counter).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub workload: String,
    pub lane: String,
    pub variant: String,
    /// `ok`, `no plan: <reason>` or `not run: <error>`.
    pub status: String,
    /// `thread` (in-process, the calling thread's CPU), `processes` or
    /// `cgroup` (a server's whole process or group, background threads included).
    pub cpu_accounting: Option<&'static str>,
    /// Sum over refresh chunks of the median ingest CPU, per ingested sample.
    pub ingest_cpu_ns_per_sample: Option<f64>,
    /// Mean over evaluation times of the median evaluation CPU, in ms.
    pub query_cpu_ms_per_eval: Option<f64>,
    /// The same for the engine's own per-query CPU counter; `None` when any
    /// step lacks it.
    pub engine_counter_cpu_ms_per_eval: Option<f64>,
    /// A `cgroup` server's CPU over the whole arm, steps and the work between
    /// them, over every install, in ms.
    pub server_cpu_ms_whole_arm: Option<f64>,
    /// A `cgroup` server's `memory.current` before the installs.
    pub server_memory_before_mib: Option<f64>,
    /// A `cgroup` server's `memory.current` after the run, every install held.
    pub server_memory_after_mib: Option<f64>,
    /// A `cgroup` server's `memory.peak` over the container's whole life.
    pub server_lifetime_peak_mib: Option<f64>,
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
    pub max_rel_error: Option<f64>,
    pub median_rel_error: Option<f64>,
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
    pub lane: String,
    pub variant: String,
    pub query: String,
    pub errors: PointErrors,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `lane/variant` of the truth.
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
fn median_of(values: &[f64]) -> Option<f64> {
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
        max_rel_error: errors.iter().copied().reduce(f64::max),
        median_rel_error: median_of(errors),
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

/// Joins every lane's cells into the report, with accuracy against the
/// cells of `truth_lane`/`truth_variant`. Refuses a truth with no cell, and
/// a workload whose ran cells ingested different numbers of samples (the
/// lanes did not read the same data).
pub fn report(
    run_dir: &RunDir,
    truth_lane: &str,
    truth_variant: &str,
) -> Result<Report, BenchExternalError> {
    let results = run_dir.path().join("results");
    let mut cells: Vec<(LaneCell, Option<ArmResult>)> = Vec::new();
    for lane_dir in sorted_entries(&results)? {
        for path in sorted_entries(&lane_dir)? {
            if !file_name(&path).ends_with(".cell.json") {
                continue;
            }
            let cell: LaneCell = read_json(&path)?;
            let result = match cell.outcome {
                CellOutcome::Ran => Some(read_json::<ArmResult>(
                    &lane_dir.join(format!("{}__{}.json", cell.workload, cell.variant)),
                )?),
                _ => None,
            };
            cells.push((cell, result));
        }
    }
    if !cells
        .iter()
        .any(|(cell, _)| cell.lane == truth_lane && cell.variant == truth_variant)
    {
        return Err(BenchExternalError::Report(format!(
            "the truth {truth_lane}/{truth_variant} has no cells under results/"
        )));
    }
    let mut samples_per_workload: BTreeMap<&str, (u64, String)> = BTreeMap::new();
    for (cell, result) in &cells {
        let Some(result) = result else { continue };
        let here = format!("{}/{}", cell.lane, cell.variant);
        let (samples, first) = samples_per_workload
            .entry(&cell.workload)
            .or_insert((result.ingested_samples, here.clone()));
        if *samples != result.ingested_samples {
            return Err(BenchExternalError::Report(format!(
                "workload {}: {first} ingested {samples} samples but {here} {}; the lanes did not read the same data",
                cell.workload, result.ingested_samples
            )));
        }
    }
    let truth_of = |workload: &str| {
        cells.iter().find_map(|(cell, result)| {
            (cell.lane == truth_lane && cell.variant == truth_variant && cell.workload == workload)
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
            lane: cell.lane.clone(),
            variant: cell.variant.clone(),
            status,
            cpu_accounting: None,
            ingest_cpu_ns_per_sample: None,
            query_cpu_ms_per_eval: None,
            engine_counter_cpu_ms_per_eval: None,
            server_cpu_ms_whole_arm: None,
            server_memory_before_mib: None,
            server_memory_after_mib: None,
            server_lifetime_peak_mib: None,
            installs_held: None,
            stored_bytes_last_install: None,
            no_truth: false,
            accuracy: None,
            substitutions: Vec::new(),
            sharing_lost: Vec::new(),
        };
        if let Some(result) = result {
            fill_measurements(&mut row, result);
            if let Some(truth) = truth_of(&cell.workload) {
                let per_query =
                    errors_against(result, truth).map_err(BenchExternalError::Report)?;
                let (mut all, mut missing, mut extra) = (Vec::new(), 0, 0);
                for (query, errors) in per_query {
                    all.extend_from_slice(&errors.errors);
                    missing += errors.missing_points;
                    extra += errors.extra_points;
                    accuracy.push(QueryAccuracy {
                        workload: cell.workload.clone(),
                        lane: cell.lane.clone(),
                        variant: cell.variant.clone(),
                        query,
                        errors: point_errors(
                            &errors.errors,
                            errors.missing_points,
                            errors.extra_points,
                        ),
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
        truth: format!("{truth_lane}/{truth_variant}"),
        rows,
        accuracy,
    })
}

const MIB: f64 = 1024.0 * 1024.0;

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
        row.query_cpu_ms_per_eval = Some(query_ns as f64 / 1e6 / evaluations as f64);
        row.engine_counter_cpu_ms_per_eval = result
            .evaluate
            .iter()
            .map(|job| job.median.report.engine_reported_cpu_ns)
            .sum::<Option<u64>>()
            .map(|ns| ns as f64 / 1e6 / evaluations as f64);
    }
    row.server_cpu_ms_whole_arm = result.server_cpu_ns_whole_arm.map(|ns| ns as f64 / 1e6);
    row.server_memory_before_mib = result
        .server_memory_before
        .map(|memory| memory.current_bytes as f64 / MIB);
    row.server_memory_after_mib = result
        .server_memory_after
        .map(|memory| memory.current_bytes as f64 / MIB);
    row.server_lifetime_peak_mib = result
        .server_memory_after
        .and_then(|memory| memory.lifetime_peak_bytes)
        .map(|bytes| bytes as f64 / MIB);
    row.installs_held = Some(result.installs_held);
    row.stored_bytes_last_install =
        Some(result.footprint.view_bytes + result.footprint.raw_retained_bytes);
    row.substitutions = result.plan.substitutions.clone();
    row.sharing_lost = result.plan.sharing_lost.clone();
}

/// The `summary.csv` header.
pub const SUMMARY_CSV_HEADER: &str = "workload,lane,variant,status,cpu_accounting,ingest_cpu_ns_per_sample,query_cpu_ms_per_eval,engine_counter_cpu_ms_per_eval,server_cpu_ms_whole_arm,server_memory_before_mib,server_memory_after_mib,server_lifetime_peak_mib,installs_held,stored_bytes_last_install,max_rel_error,median_rel_error,points,missing_points,extra_points,substitutions,sharing_lost";

/// The `accuracy.csv` header.
pub const ACCURACY_CSV_HEADER: &str =
    "workload,lane,variant,query,max_rel_error,median_rel_error,points,missing_points,extra_points";

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
            field(e.max_rel_error),
            field(e.median_rel_error),
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
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                csv_text(&row.workload),
                csv_text(&row.lane),
                csv_text(&row.variant),
                csv_text(&row.status),
                field(row.cpu_accounting),
                field(row.ingest_cpu_ns_per_sample),
                field(row.query_cpu_ms_per_eval),
                field(row.engine_counter_cpu_ms_per_eval),
                field(row.server_cpu_ms_whole_arm),
                field(row.server_memory_before_mib),
                field(row.server_memory_after_mib),
                field(row.server_lifetime_peak_mib),
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
                csv_text(&row.lane),
                csv_text(&row.variant),
                csv_text(&row.query),
                error_fields(Some(&row.errors))
            ));
        }
        text
    }

    /// `summary.md`: rows = workloads, columns = `lane/variant`, sorted.
    /// A ran cell reads `q 2.000 ms, i 2000.0 ns/sample, err 0.100 [cgroup]`.
    pub fn summary_markdown(&self) -> String {
        let columns: std::collections::BTreeSet<String> = self
            .rows
            .iter()
            .map(|row| format!("{}/{}", row.lane, row.variant))
            .collect();
        let workloads: std::collections::BTreeSet<&str> =
            self.rows.iter().map(|row| row.workload.as_str()).collect();
        let mut text = format!(
            "Absolute CPU per lane: `q` = query CPU per evaluation, `i` = ingest CPU per sample, `err` = max relative error against {}. `[thread]` lanes count the calling thread only; `[cgroup]` lanes count the whole server.\n\n| workload |",
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
                    row.workload == workload && format!("{}/{}", row.lane, row.variant) == *column
                });
                let cell = match row {
                    None => String::new(),
                    Some(row) if row.status == "ok" => format!(
                        "q {} ms, i {} ns/sample, err {} [{}]",
                        row.query_cpu_ms_per_eval
                            .map_or("-".into(), |v| format!("{v:.3}")),
                        row.ingest_cpu_ns_per_sample
                            .map_or("-".into(), |v| format!("{v:.1}")),
                        if row.no_truth {
                            "- (no truth)".to_string()
                        } else {
                            row.accuracy
                                .as_ref()
                                .and_then(|e| e.max_rel_error)
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
pub fn write_report(run_dir: &RunDir, report: &Report) -> Result<(), BenchExternalError> {
    for (name, text) in [
        ("summary.csv", report.summary_csv()),
        ("summary.md", report.summary_markdown()),
        ("accuracy.csv", report.accuracy_csv()),
    ] {
        let path = run_dir.path().join(name);
        std::fs::write(&path, text).map_err(|e| io(&path, e))?;
    }
    Ok(())
}

fn cell_path(lane_dir: &Path, workload: &str, variant: &str) -> PathBuf {
    lane_dir.join(format!("{workload}__{variant}.cell.json"))
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, BenchExternalError> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| io(dir, e))?
        .map(|entry| entry.map(|entry| entry.path()).map_err(|e| io(dir, e)))
        .collect::<Result<_, _>>()?;
    paths.sort();
    Ok(paths)
}

fn sorted_files(dir: &Path) -> Result<Vec<PathBuf>, BenchExternalError> {
    let paths = sorted_entries(dir)?;
    if let Some(sub) = paths.iter().find(|path| !path.is_file()) {
        return Err(BenchExternalError::Layout(format!(
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

fn file_stem_with_suffix(path: &Path, suffix: &str) -> Result<String, BenchExternalError> {
    file_name(path)
        .strip_suffix(suffix)
        .map(str::to_string)
        .ok_or_else(|| {
            BenchExternalError::Layout(format!("{} does not end in {suffix}", path.display()))
        })
}

fn absolute(path: &Path) -> Result<PathBuf, BenchExternalError> {
    std::path::absolute(path).map_err(|e| io(path, e))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, BenchExternalError> {
    let text = std::fs::read_to_string(path).map_err(|e| io(path, e))?;
    serde_json::from_str(&text).map_err(|error| BenchExternalError::Json {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn io(path: &Path, error: std::io::Error) -> BenchExternalError {
    BenchExternalError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BenchExternalError {
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
    RunDir(#[from] RunDirError),
    /// Inputs the report cannot join, such as lanes that ingested different data.
    #[error("{0}")]
    Report(String),
}

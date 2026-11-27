use std::collections::BTreeMap;

use crate::harness::benchmark_external::{median_of, relative_error};
use crate::harness::one_shot::{OneShotResult, QueryRecord, ResultTable, RunRecord};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConfigurationName {
    pub engine: String,
    pub configuration: String,
}

impl ConfigurationName {
    pub fn parse(text: &str) -> Result<ConfigurationName, String> {
        match text.split_once('/') {
            Some((engine, configuration)) if !engine.is_empty() && !configuration.is_empty() => {
                Ok(ConfigurationName {
                    engine: engine.to_string(),
                    configuration: configuration.to_string(),
                })
            }
            _ => Err(format!("'{text}' is not <engine>/<configuration>")),
        }
    }

    pub fn of(result: &OneShotResult) -> ConfigurationName {
        ConfigurationName {
            engine: result.engine.id.clone(),
            configuration: result.configuration.clone(),
        }
    }
}

impl std::fmt::Display for ConfigurationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.engine, self.configuration)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableComparison {
    pub max_relative_error: Option<f64>,
    pub compared_cells: usize,
    pub differing_text_cells: usize,
    pub missing_groups: usize,
    pub extra_groups: usize,
}

pub fn compare_tables(
    answer: &ResultTable,
    truth: &ResultTable,
    group_by_column_positions: &[usize],
) -> Result<TableComparison, String> {
    let both_have_rows = !answer.rows.is_empty() && !truth.rows.is_empty();
    if both_have_rows && answer.columns.len() != truth.columns.len() {
        return Err(format!(
            "{} columns, the truth has {}",
            answer.columns.len(),
            truth.columns.len()
        ));
    }
    if let Some(position) = group_by_column_positions.iter().find(|position| {
        [answer, truth]
            .iter()
            .any(|table| !table.rows.is_empty() && **position >= table.columns.len())
    }) {
        return Err(format!(
            "group column position {position} is past the {} columns",
            truth.columns.len()
        ));
    }
    let keyed = |table: &ResultTable| -> BTreeMap<Vec<String>, usize> {
        table
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let key = if group_by_column_positions.is_empty() {
                    vec![index.to_string()]
                } else {
                    group_by_column_positions
                        .iter()
                        .map(|position| row[*position].key_text())
                        .collect()
                };
                (key, index)
            })
            .collect()
    };
    let answer_rows = keyed(answer);
    let truth_rows = keyed(truth);
    let mut comparison = TableComparison {
        missing_groups: truth_rows
            .keys()
            .filter(|key| !answer_rows.contains_key(*key))
            .count(),
        extra_groups: answer_rows
            .keys()
            .filter(|key| !truth_rows.contains_key(*key))
            .count(),
        ..TableComparison::default()
    };
    for (key, truth_index) in &truth_rows {
        let Some(answer_index) = answer_rows.get(key) else {
            continue;
        };
        let (answer_row, truth_row) = (&answer.rows[*answer_index], &truth.rows[*truth_index]);
        for position in 0..truth.columns.len() {
            if group_by_column_positions.contains(&position) {
                continue;
            }
            let error = match (
                answer_row[position].as_number(),
                truth_row[position].as_number(),
            ) {
                (Some(value), Some(expected)) => relative_error(value, expected),
                (None, Some(_)) => f64::INFINITY,
                _ => {
                    if answer_row[position].key_text() != truth_row[position].key_text() {
                        comparison.differing_text_cells += 1;
                    }
                    continue;
                }
            };
            comparison.compared_cells += 1;
            comparison.max_relative_error = Some(
                comparison
                    .max_relative_error
                    .map_or(error, |max| max.max(error)),
            );
        }
    }
    Ok(comparison)
}

#[derive(Debug, Clone, PartialEq)]
pub struct OneShotReportRow {
    pub workload: String,
    pub configuration: ConfigurationName,
    pub query: String,
    pub status: String,
    pub runs: usize,
    pub first_run_cold: bool,
    pub time_source: &'static str,
    pub first_run_seconds: Option<f64>,
    pub hot_min_seconds: Option<f64>,
    pub hot_median_seconds: Option<f64>,
    pub hot_median_engine_cpu_seconds: Option<f64>,
    pub speedups: Vec<(ConfigurationName, Option<f64>)>,
    pub target_relative_error: Option<f64>,
    pub max_relative_error: Option<f64>,
    pub median_max_relative_error: Option<f64>,
    pub runs_over_target: Option<usize>,
    pub max_missing_groups: Option<usize>,
    pub max_extra_groups: Option<usize>,
    pub differing_text_cells: Option<usize>,
    pub truth_runs_agree: Option<bool>,
    pub runs_answered_exactly: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OneShotReport {
    pub truth: Option<ConfigurationName>,
    pub baselines: Vec<ConfigurationName>,
    pub rows: Vec<OneShotReportRow>,
    pub tables: Vec<TableRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    pub configuration: ConfigurationName,
    pub table: String,
    pub rows: u64,
    pub load_seconds: Option<f64>,
    pub data_size_bytes: Option<u64>,
}

fn uses_engine_time(runs: &[&RunRecord]) -> bool {
    !runs.is_empty() && runs.iter().all(|run| run.engine_reported_seconds.is_some())
}

fn seconds(run: &RunRecord, engine_time: bool) -> f64 {
    if engine_time {
        run.engine_reported_seconds.unwrap_or(run.wall_seconds)
    } else {
        run.wall_seconds
    }
}

struct Timing {
    engine_time: bool,
    first: Option<f64>,
    hot_min: Option<f64>,
    hot_median: Option<f64>,
    hot_cpu_median: Option<f64>,
}

fn timing_of(query: &QueryRecord) -> Timing {
    let answered: Vec<&RunRecord> = query
        .runs
        .iter()
        .filter(|run| run.error.is_none())
        .collect();
    let engine_time = uses_engine_time(&answered);
    let first = query
        .runs
        .first()
        .filter(|run| run.error.is_none())
        .map(|run| seconds(run, engine_time));
    let hot: Vec<f64> = query
        .runs
        .iter()
        .skip(1)
        .filter(|run| run.error.is_none())
        .map(|run| seconds(run, engine_time))
        .collect();
    let hot_runs: Vec<&RunRecord> = query
        .runs
        .iter()
        .skip(1)
        .filter(|run| run.error.is_none())
        .collect();
    let hot_cpu: Vec<f64> = hot_runs
        .iter()
        .filter_map(|run| run.engine_reported_cpu_seconds)
        .collect();
    Timing {
        engine_time,
        first,
        hot_min: hot.iter().copied().min_by(f64::total_cmp),
        hot_median: median_of(&hot),
        hot_cpu_median: if hot_cpu.len() == hot_runs.len() {
            median_of(&hot_cpu)
        } else {
            None
        },
    }
}

pub fn one_shot_report(
    results: &[OneShotResult],
    truth: Option<&ConfigurationName>,
    baselines: &[ConfigurationName],
) -> Result<OneShotReport, String> {
    let find = |name: &ConfigurationName, workload: &str| {
        results
            .iter()
            .find(|result| &ConfigurationName::of(result) == name && result.workload == workload)
    };
    for name in truth.into_iter().chain(baselines) {
        if !results
            .iter()
            .any(|result| &ConfigurationName::of(result) == name)
        {
            return Err(format!("no one-shot result of {name}"));
        }
    }
    let mut rows = Vec::new();
    let mut tables = Vec::new();
    for result in results {
        let configuration = ConfigurationName::of(result);
        for table in &result.tables {
            tables.push(TableRow {
                configuration: configuration.clone(),
                table: table.name.clone(),
                rows: table.rows,
                load_seconds: table.load_seconds,
                data_size_bytes: table.data_size_bytes,
            });
        }
        for query in &result.queries {
            let timing = timing_of(query);
            let failed = query.runs.iter().find_map(|run| run.error.clone());
            let status = match (&query.skipped, failed) {
                (Some(reason), _) => format!("skipped: {reason}"),
                (None, Some(error)) => format!("failed: {error}"),
                (None, None) => "ok".to_string(),
            };
            let speedups = baselines
                .iter()
                .map(|baseline| {
                    let speedup = find(baseline, &result.workload)
                        .and_then(|other| other.queries.iter().find(|other| other.id == query.id))
                        .and_then(|other| timing_of(other).hot_median)
                        .zip(timing.hot_median)
                        .map(|(baseline_seconds, seconds)| baseline_seconds / seconds);
                    (baseline.clone(), speedup)
                })
                .collect();
            let mut row = OneShotReportRow {
                workload: result.workload.clone(),
                configuration: configuration.clone(),
                query: query.id.clone(),
                status,
                runs: query.runs.len(),
                first_run_cold: result.cold_first_run,
                time_source: if timing.engine_time { "engine" } else { "wall" },
                first_run_seconds: timing.first,
                hot_min_seconds: timing.hot_min,
                hot_median_seconds: timing.hot_median,
                hot_median_engine_cpu_seconds: timing.hot_cpu_median,
                speedups,
                target_relative_error: result.error_target.map(|target| target.relative_error),
                max_relative_error: None,
                median_max_relative_error: None,
                runs_over_target: None,
                max_missing_groups: None,
                max_extra_groups: None,
                differing_text_cells: None,
                truth_runs_agree: None,
                runs_answered_exactly: None,
            };
            let answered_exactly: Vec<bool> = query
                .runs
                .iter()
                .filter_map(|run| run.details.get("answered_exactly")?.as_bool())
                .collect();
            if !answered_exactly.is_empty() {
                row.runs_answered_exactly =
                    Some(answered_exactly.iter().filter(|exact| **exact).count());
            }
            if let Some(truth) = truth {
                let truth_query = find(truth, &result.workload)
                    .and_then(|other| other.queries.iter().find(|other| other.id == query.id));
                let truth_table = truth_query
                    .and_then(|other| other.runs.iter().find_map(|run| run.result.as_ref()));
                if let (Some(truth_query), Some(truth_table)) = (truth_query, truth_table) {
                    row.truth_runs_agree = Some(runs_agree(truth_query, truth_table));
                    accuracy(&mut row, query, truth_table, result);
                }
            }
            rows.push(row);
        }
    }
    Ok(OneShotReport {
        truth: truth.cloned(),
        baselines: baselines.to_vec(),
        rows,
        tables,
    })
}

fn runs_agree(query: &QueryRecord, first: &ResultTable) -> bool {
    query
        .runs
        .iter()
        .filter_map(|run| run.result.as_ref())
        .all(|table| {
            compare_tables(table, first, &query.group_by_column_positions).is_ok_and(|comparison| {
                comparison.max_relative_error.unwrap_or(0.0) == 0.0
                    && comparison.missing_groups == 0
                    && comparison.extra_groups == 0
                    && comparison.differing_text_cells == 0
            })
        })
}

fn accuracy(
    row: &mut OneShotReportRow,
    query: &QueryRecord,
    truth: &ResultTable,
    result: &OneShotResult,
) {
    let mut max_errors = Vec::new();
    let mut over = 0;
    let (mut missing, mut extra, mut text_cells) = (0, 0, 0);
    for run in &query.runs {
        let Some(table) = &run.result else {
            continue;
        };
        let comparison = match compare_tables(table, truth, &query.group_by_column_positions) {
            Ok(comparison) => comparison,
            Err(reason) => {
                row.status = format!("not comparable: {reason}");
                return;
            }
        };
        let max_error = comparison.max_relative_error.unwrap_or(0.0);
        max_errors.push(max_error);
        missing = missing.max(comparison.missing_groups);
        extra = extra.max(comparison.extra_groups);
        text_cells = text_cells.max(comparison.differing_text_cells);
        if let Some(target) = result.error_target {
            if max_error > target.relative_error
                || comparison.missing_groups > 0
                || comparison.extra_groups > 0
                || comparison.differing_text_cells > 0
            {
                over += 1;
            }
        }
    }
    if max_errors.is_empty() {
        return;
    }
    row.max_relative_error = max_errors.iter().copied().max_by(f64::total_cmp);
    row.median_max_relative_error = median_of(&max_errors);
    row.runs_over_target = result.error_target.map(|_| over);
    row.max_missing_groups = Some(missing);
    row.max_extra_groups = Some(extra);
    row.differing_text_cells = Some(text_cells);
}

fn format_number(value: Option<f64>) -> String {
    value.map_or_else(String::new, |value| format!("{value}"))
}

fn format_count(value: Option<usize>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn format_flag(value: Option<bool>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn format_for_reading(value: Option<f64>) -> String {
    match value {
        None => "".into(),
        Some(value) if value.is_infinite() => "inf".into(),
        Some(value) if value != 0.0 && value.abs() < 0.001 => format!("{value:.1e}"),
        Some(value) => format!("{value:.3}"),
    }
}

fn csv_text(header: Vec<String>, records: Vec<Vec<String>>) -> String {
    let mut writer = csv::Writer::from_writer(Vec::new());
    for record in std::iter::once(header).chain(records) {
        writer.write_record(&record).expect("writing CSV to memory");
    }
    String::from_utf8(writer.into_inner().expect("writing CSV to memory"))
        .expect("CSV of UTF-8 fields")
}

impl OneShotReport {
    pub fn summary_csv(&self) -> String {
        let mut header: Vec<String> = [
            "workload",
            "engine",
            "configuration",
            "query",
            "status",
            "runs",
            "first_run_cold",
            "time_source",
            "first_run_seconds",
            "hot_min_seconds",
            "hot_median_seconds",
            "hot_median_engine_cpu_seconds",
        ]
        .map(str::to_string)
        .into();
        header.extend(
            self.baselines
                .iter()
                .map(|baseline| format!("speedup_over_{baseline}")),
        );
        header.extend(
            [
                "target_relative_error",
                "max_relative_error",
                "median_max_relative_error",
                "runs_over_target",
                "max_missing_groups",
                "max_extra_groups",
                "differing_text_cells",
                "truth_runs_agree",
                "runs_answered_exactly",
            ]
            .map(str::to_string),
        );
        let records = self
            .rows
            .iter()
            .map(|row| {
                let mut fields = vec![
                    row.workload.clone(),
                    row.configuration.engine.clone(),
                    row.configuration.configuration.clone(),
                    row.query.clone(),
                    row.status.clone(),
                    row.runs.to_string(),
                    row.first_run_cold.to_string(),
                    row.time_source.to_string(),
                    format_number(row.first_run_seconds),
                    format_number(row.hot_min_seconds),
                    format_number(row.hot_median_seconds),
                    format_number(row.hot_median_engine_cpu_seconds),
                ];
                fields.extend(
                    row.speedups
                        .iter()
                        .map(|(_, speedup)| format_number(*speedup)),
                );
                fields.extend([
                    format_number(row.target_relative_error),
                    format_number(row.max_relative_error),
                    format_number(row.median_max_relative_error),
                    format_count(row.runs_over_target),
                    format_count(row.max_missing_groups),
                    format_count(row.max_extra_groups),
                    format_count(row.differing_text_cells),
                    format_flag(row.truth_runs_agree),
                    format_count(row.runs_answered_exactly),
                ]);
                fields
            })
            .collect();
        csv_text(header, records)
    }

    pub fn tables_csv(&self) -> String {
        let header = [
            "engine",
            "configuration",
            "table",
            "rows",
            "load_seconds",
            "data_size_bytes",
        ]
        .map(str::to_string)
        .into();
        let records = self
            .tables
            .iter()
            .map(|row| {
                vec![
                    row.configuration.engine.clone(),
                    row.configuration.configuration.clone(),
                    row.table.clone(),
                    row.rows.to_string(),
                    format_number(row.load_seconds),
                    row.data_size_bytes
                        .map_or_else(String::new, |bytes| bytes.to_string()),
                ]
            })
            .collect();
        csv_text(header, records)
    }

    pub fn summary_markdown(&self) -> String {
        let mut text = String::new();
        if let Some(truth) = &self.truth {
            text.push_str(&format!("Accuracy against {truth}.\n\n"));
        }
        let mut header = vec![
            "workload".to_string(),
            "engine/configuration".into(),
            "query".into(),
            "status".into(),
            "first run s".into(),
            "hot median s".into(),
        ];
        header.extend(
            self.baselines
                .iter()
                .map(|baseline| format!("speedup over {baseline}")),
        );
        header.extend([
            "max relative error".into(),
            "runs over target".into(),
            "truth runs agree".into(),
            "answered exactly".into(),
        ]);
        text.push_str(&format!("| {} |\n", header.join(" | ")));
        text.push_str(&format!("|{}\n", " --- |".repeat(header.len())));
        for row in &self.rows {
            let mut cells = vec![
                row.workload.clone(),
                row.configuration.to_string(),
                row.query.clone(),
                row.status.replace('|', "\\|"),
                format_for_reading(row.first_run_seconds),
                format_for_reading(row.hot_median_seconds),
            ];
            cells.extend(
                row.speedups
                    .iter()
                    .map(|(_, speedup)| format_for_reading(*speedup)),
            );
            cells.extend([
                format_for_reading(row.max_relative_error),
                match (row.runs_over_target, row.target_relative_error) {
                    (Some(over), Some(target)) => format!("{over}/{} (target {target})", row.runs),
                    _ => String::new(),
                },
                format_flag(row.truth_runs_agree),
                row.runs_answered_exactly
                    .map_or_else(String::new, |exact| format!("{exact}/{}", row.runs)),
            ]);
            text.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        text
    }
}

pub fn write_one_shot_report(
    run_directory: &std::path::Path,
    report: &OneShotReport,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut written = Vec::new();
    for (name, text) in [
        ("one-shot-summary.csv", report.summary_csv()),
        ("one-shot-summary.md", report.summary_markdown()),
        ("one-shot-tables.csv", report.tables_csv()),
    ] {
        let path = run_directory.join(name);
        std::fs::write(&path, text)?;
        written.push(path);
    }
    Ok(written)
}
